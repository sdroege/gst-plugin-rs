# Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
#
# This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
# If a copy of the MPL was not distributed with this file, You can obtain one at
# <https://mozilla.org/MPL/2.0/>.
#
# SPDX-License-Identifier: MPL-2.0

import logging
logger = logging.getLogger(__name__)

from collections import deque
import traceback
import math
from pathlib import Path

import typing as tp
from dataclasses import dataclass

import threading
import concurrent.futures
from concurrent.futures import ThreadPoolExecutor, Future

import torch as th
import numpy as np

# float32 little endian
F32LE = np.dtype(np.float32).newbyteorder('<')

DEVICE: th.device
MODELS: dict[str, th.nn.Module] = dict()
MODEL_REPO: tp.Optional[Path] = None
# We only allow stereo models right now
CHANNELS: int = 2

PROCESSING_POOL: ThreadPoolExecutor = ThreadPoolExecutor()

@dataclass
class PendingChunk:
    index: int
    in_: tp.Optional[np.ndarray] = None
    out: tp.Optional[np.ndarray] = None
    num_zeroes: int = 0
    last: bool = False
    fut: tp.Optional[Future[None]] = None

class Session:
    output_cb: tp.Callable[[bytes], None]

    rate: int
    chunk_samples: int # samples
    overlap: float      # fraction of chunk_duration
    stride: int # samples, number of samples to remove from in_queue for every chunk
    weights: np.ndarray
    model: th.nn.Module
    model_sources: tp.List[str]
    model_name: str
    latency_samples: int # samples

    processing_thread: threading.Thread

    in_queue_cond: threading.Condition # Protects everything below
    in_queue: deque[np.ndarray]
    in_queue_n_samples: int
    # Finish processing cleanly
    in_queue_finish: bool
    # Immediately stop processing
    processing_thread_terminate: bool
    processing_exception: tp.Optional[Exception]

    pending_chunks_lock: threading.Lock  # Protects everything below
    pending_chunks: deque[PendingChunk]
    next_pending_chunk_index: int
    next_out_chunk_index: int

    def __init__(self, model_name: str, rate: int, chunk_duration: int, overlap: float, output_cb: tp.Callable[[bytes], None]):
        assert DEVICE is not None

        if rate < 32000 or rate > 48000:
            raise RuntimeError('Only rates between 32kHz and 48kHz are supported')
        if chunk_duration < 100:
            raise RuntimeError('Chunk duration needs to be >= 100ms')
        if overlap < 0.0 or overlap >= 1.0:
            raise RuntimeError('Overlap must be >= 0.0 and < 1.0')

        model = MODELS.get(model_name, None)
        if model is None:
            import demucs.pretrained
            import demucs.apply
            import multiprocessing

            # This is a workaround for a bug where the get_model() call below
            # would end up spawning another process of our host binary
            # (e.g. gst-launch-1.0) instead of Python. Pytorch uses tqdm
            # to display a progress bar when downloading models,
            # which internally uses multiprocessing, which then tries
            # to execute some code in a separate process, using sys.executable
            # as the target. Setting this to an empty string prevents that
            # and doesn't seem to break anything for our use case.
            # See:
            # https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/merge_requests/2554#note_3128828
            multiprocessing.set_executable("")

            model = demucs.pretrained.get_model(model_name, MODEL_REPO)
            if model is None:
                raise Exception('Failed to find model')

            if isinstance(model, demucs.apply.BagOfModels):
                assert len(model.models) == 1
                model = model.models[0]
            assert model.audio_channels == CHANNELS
            MODELS[model_name] = model

        assert isinstance(model.sources, tp.Iterable)
        model_sources = [str(x) for x in model.sources]
        logger.debug(f'Model {model_name} has sources {model_sources}')

        model = model.to(DEVICE)
        model.eval()

        self.output_cb = output_cb
        self.rate = rate
        self.chunk_samples = int((chunk_duration * rate) / 1000)
        self.overlap = overlap
        self.stride = max(int((1.0 - overlap) * self.chunk_samples), 1)
        if overlap == 0.0:
            self.stride = self.chunk_samples
        self.weights = np.concatenate([np.arange(1, self.chunk_samples // 2 + 1, 1.0), np.arange(self.chunk_samples - self.chunk_samples // 2, 0, -1.0)])
        self.weights /= self.weights.max()

        self.model = model
        self.model_name = model_name
        self.model_sources = model_sources

        self.latency_samples = int((math.ceil(1.0 / (1.0 - overlap)) + 1) * self.chunk_samples)
        if overlap == 0.0:
            self.latency_samples = self.chunk_samples

        self.in_queue_cond = threading.Condition()
        self.in_queue = deque()
        self.in_queue_n_samples = 0
        self.in_queue_finish = False
        self.processing_thread_terminate = False
        self.processing_exception = None

        self.pending_chunks_lock = threading.Lock()
        self.pending_chunks = deque()
        self.next_pending_chunk_index = 0
        self.next_out_chunk_index = 0

        self.processing_thread = threading.Thread(target=self.__processing_thread_fn)
        self.processing_thread.start()

    def __del__(self):
        self.cancel(True)

    def cancel(self, wait: bool = False):
        with self.in_queue_cond:
            self.processing_thread_terminate = True
            self.in_queue_cond.notify()

        futs = []
        with self.pending_chunks_lock:
            for pending_chunk in self.pending_chunks:
                if pending_chunk.fut is not None:
                    pending_chunk.fut.cancel()
                    futs.append(pending_chunk.fut)

        if wait:
            self.processing_thread.join()
            concurrent.futures.wait(futs)

        self.output_cb = lambda _: None

    def push_data(self, data: bytes):
        with self.in_queue_cond:
            if self.processing_exception is not None:
                raise self.processing_exception

            if self.processing_thread_terminate or self.in_queue_finish:
                return

            # Have data so just queue it up
            if len(data) != 0:
                # Read as floats and de-interleave in one go
                samples = np.frombuffer(data, dtype=F32LE).reshape((-1, CHANNELS)).swapaxes(0, 1)
                logger.debug(f'Processing data with {samples.shape[1]} samples')

                # Queue new samples for processing thread
                self.in_queue_n_samples += samples.shape[1]
                self.in_queue.append(samples)
            else:
                # Finishing
                logger.debug('Draining')
                # Signal the processing thread to shut down after draining
                self.in_queue_finish = True

            self.in_queue_cond.notify()

    def __processing_thread_fn(self):
        try:
            while True:
                new_chunk = None
                num_zeroes = 0

                with self.in_queue_cond:
                    while self.in_queue_n_samples < self.chunk_samples and not self.processing_thread_terminate and not self.in_queue_finish:
                        self.in_queue_cond.wait()

                    if self.processing_thread_terminate:
                        logger.debug('Terminating processing thread')
                        break

                    if len(self.in_queue) > 0:
                        new_chunk = np.zeros((CHANNELS, self.chunk_samples), dtype=F32LE)
                        copied = 0
                        for chunk in self.in_queue:
                            to_copy = min(chunk.shape[1], self.chunk_samples - copied)
                            logger.debug(f'copying {to_copy} samples')
                            np.copyto(new_chunk[:, copied:(copied+to_copy)], chunk[:, :to_copy], casting='no')
                            copied += to_copy

                            assert copied <= self.chunk_samples
                            if copied == self.chunk_samples:
                                break
                        num_zeroes = self.chunk_samples - copied

                        dropped = 0
                        while dropped < self.stride and len(self.in_queue) > 0:
                            chunk = self.in_queue[0]
                            to_drop = min(chunk.shape[1], self.stride - dropped)
                            if chunk.shape[1] == to_drop:
                                self.in_queue.popleft()
                            else:
                                self.in_queue[0] = chunk[:, to_drop:]

                            dropped += to_drop
                            self.in_queue_n_samples -= to_drop

                    if self.in_queue_finish and new_chunk is None:
                        assert self.in_queue_n_samples == 0
                        logger.debug('Drained input queue')

                        with self.pending_chunks_lock:
                            # Store terminator for later once the previous chunks are finished
                            pending_chunk = PendingChunk(self.next_pending_chunk_index, last=True)
                            self.next_pending_chunk_index += 1
                            self.pending_chunks.append(pending_chunk)

                        break

                assert new_chunk is not None

                # Pass chunk to model and write out result
                with self.pending_chunks_lock:
                    pending_chunk = PendingChunk(self.next_pending_chunk_index, in_ = new_chunk, num_zeroes=num_zeroes)
                    self.next_pending_chunk_index += 1
                    self.pending_chunks.append(pending_chunk)
                    logger.debug(f'Passing chunk {pending_chunk.index} of shape {new_chunk.shape} with {num_zeroes} padding samples to the model')

                    fut = PROCESSING_POOL.submit(self.__apply_model, pending_chunk)
                    pending_chunk.fut = fut

                # And next round

        except Exception as e:
            logger.warning(f'Got exception during processing: {traceback.format_exception(e)}')

            with self.in_queue_cond:
                self.processing_exception = e
            self.cancel()

            raise
        finally:
            logger.debug('Finished processing thread')

    def __apply_model(self, pending_chunk: PendingChunk):
        try:
            # Add batch dimension
            assert pending_chunk.in_ is not None

            # Check if cancelled
            with self.in_queue_cond:
                if self.processing_thread_terminate:
                    return

            in_ = np.expand_dims(pending_chunk.in_, axis=0).copy()

            # TODO: running mean/std instead of per chunk?
            ref = in_.mean(0)
            std = ref.std()
            mean = ref.mean()

            in_ -= mean
            in_ /= std

            chunk = th.tensor(in_).to(DEVICE)
            with th.no_grad():
                out = self.model(chunk)

            out *= std
            out += mean

            with self.pending_chunks_lock:
                # Check if cancelled
                with self.in_queue_cond:
                    if self.processing_thread_terminate:
                        return

                logger.debug(f'Received chunk {pending_chunk.index} of shape {out.shape} from the model')

                pending_chunk.out = out.squeeze(axis=0).numpy() # Remove batch dimension

                # Now finish all out chunks that are ready
                while True:
                    out_chunk_start = self.next_out_chunk_index * self.chunk_samples
                    out_chunk_end = out_chunk_start + self.chunk_samples

                    # Check if the current out chunk is ready
                    out_chunk_ready = False
                    for pending_chunk in self.pending_chunks:
                        # This pending chunk is not ready yet
                        if pending_chunk.out is None and not pending_chunk.last:
                            break

                        pending_chunk_start = pending_chunk.index * self.stride
                        pending_chunk_end = pending_chunk_start + self.chunk_samples

                        # Samples of pending chunk must be inside the current out chunk
                        assert out_chunk_end > pending_chunk_start
                        assert out_chunk_start < pending_chunk_end

                        # Next pending chunk starts after the end of this out chunk
                        # so this out chunk is complete, or the next one is the last
                        # one so there won't be any further samples
                        next_pending_chunk_start = pending_chunk_start + self.stride
                        if next_pending_chunk_start >= out_chunk_end or pending_chunk.last:
                            out_chunk_ready = True
                            break

                    if not out_chunk_ready:
                        logger.debug(f'No further chunk ready yet, next chunk would be {self.next_out_chunk_index}')
                        break

                    # If there are actual samples to copy ...
                    if not self.pending_chunks[0].last:
                        # Out chunk is ready so fill it from all the chunks that make it up
                        logger.debug(f'Outputting chunk {self.next_out_chunk_index} now')
                        out_chunk = np.zeros((len(self.model_sources), CHANNELS, self.chunk_samples), dtype=F32LE)
                        sum_weights = np.zeros(self.chunk_samples, dtype = F32LE)
                        out_chunk_length = self.chunk_samples

                        for pending_chunk in self.pending_chunks:
                            # Checked above
                            assert pending_chunk.out is not None or pending_chunk.last
                            # Last chunk so nothing to copy
                            if pending_chunk.last:
                                break
                            assert pending_chunk.out is not None

                            pending_chunk_start = pending_chunk.index * self.stride
                            pending_chunk_end = pending_chunk_start + self.chunk_samples

                            # Samples of pending chunk must be inside the current out chunk
                            assert out_chunk_end > pending_chunk_start
                            assert out_chunk_start < pending_chunk_end

                            copy_to_start = max(pending_chunk_start, out_chunk_start) - out_chunk_start
                            copy_from_start = max(pending_chunk_start, out_chunk_start) - pending_chunk_start
                            to_copy = min(pending_chunk_end, out_chunk_end) - max(pending_chunk_start, out_chunk_start)
                            assert to_copy > 0

                            out_chunk[:, :, copy_to_start:copy_to_start + to_copy] += pending_chunk.out[:, :, copy_from_start:copy_from_start + to_copy] * self.weights[copy_from_start:copy_from_start + to_copy]
                            sum_weights[copy_to_start:copy_to_start + to_copy] += self.weights[copy_from_start:copy_from_start + to_copy]

                            if pending_chunk.num_zeroes > 0:
                                zeroes_start = pending_chunk_end - pending_chunk.num_zeroes
                                if zeroes_start <= out_chunk_start:
                                    out_chunk_length = 0
                                elif zeroes_start <= out_chunk_end:
                                    out_chunk_length = self.chunk_samples - (out_chunk_end - zeroes_start)

                            # Next pending chunk starts after the end of this out chunk
                            # so this out chunk is complete
                            next_pending_chunk_start = pending_chunk_start + self.stride
                            if next_pending_chunk_start >= out_chunk_end:
                                break

                        if out_chunk_length == 0:
                            logger.debug('No non-zero samples left')
                        else:
                            if out_chunk_length < self.chunk_samples:
                                # Remove padding zeroes, if any
                                logger.debug(f'Removing {self.chunk_samples - out_chunk_length} padding samples')
                                out_chunk = out_chunk[:, :, :out_chunk_length]
                                sum_weights = sum_weights[:out_chunk_length]
                                logger.debug(f'Have {out_chunk.shape[2]} samples left')

                            assert sum_weights.min() > 0.0
                            out_chunk /= sum_weights

                            # Interleave channels and flatten the different sources and channels into a 1D array
                            out_chunk = out_chunk.swapaxes(1, 2).flatten()
                            self.output_cb(out_chunk.tobytes())

                    self.next_out_chunk_index += 1

                    next_out_chunk_start = self.next_out_chunk_index * self.chunk_samples
                    while len(self.pending_chunks) > 0:
                        pending_chunk = self.pending_chunks[0]

                        # This pending chunk is not ready yet
                        if pending_chunk.out is None and not pending_chunk.last:
                            break

                        pending_chunk_start = pending_chunk.index * self.stride
                        pending_chunk_end = pending_chunk_start + self.chunk_samples

                        # Samples of pending chunk are part of the next out chunk so
                        # stop iteration here
                        if pending_chunk_end > next_out_chunk_start:
                            break

                        # Otherwise drop the pending chunk as it's not used anymore
                        logger.debug(f'Dropping pending chunk {pending_chunk.index} now')
                        self.pending_chunks.popleft()

                        # If this was the last pending chunk then signal it to the
                        # writer task
                        if pending_chunk.last:
                            assert len(self.pending_chunks) == 0
                            self.output_cb(bytes())

        except Exception as e:
            logger.error(f'Got exception during model processing: {traceback.format_exception(e)}')
            with self.in_queue_cond:
                self.processing_exception = e
            self.cancel()
            raise

def init(model_device: str, model_repo: tp.Optional[Path]):
    global DEVICE
    global MODEL_REPO

    DEVICE = th.device(model_device)
    MODEL_REPO = model_repo
