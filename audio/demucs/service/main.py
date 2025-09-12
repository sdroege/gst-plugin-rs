# Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
#
# This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
# If a copy of the MPL was not distributed with this file, You can obtain one at
# <https://mozilla.org/MPL/2.0/>.
#
# SPDX-License-Identifier: MPL-2.0

import logging
logger = logging.getLogger(__name__)

import typing as tp

from pathlib import Path
import json

import asyncio

import websockets
from websockets.asyncio.server import ServerConnection, serve
import websockets.protocol
from websockets.typing import Subprotocol

from collections import deque
import traceback

import session

class Session:
    websocket: ServerConnection
    demucs_session: session.Session
    loop: asyncio.AbstractEventLoop

    max_in_flight_samples: int # samples

    input_task: tp.Optional[asyncio.Task]

    # Number of samples that are in in_queue, pending_chunks and out_queue
    in_flight_n_samples_cond: asyncio.Condition
    in_flight_n_samples: int

    out_queue_cond: asyncio.Condition # Protects everything below
    out_queue: deque[bytes]

    def __init__(self, websocket: ServerConnection, model_name: str, rate: int, chunk_duration: int, overlap: float):
        self.websocket = websocket
        self.demucs_session = session.Session(model_name, rate, chunk_duration, overlap, self.handle_output)
        self.loop = asyncio.get_event_loop()

        self.max_in_flight_samples = max(10 * self.demucs_session.rate, 3 * self.demucs_session.chunk_samples, 2 * self.demucs_session.latency_samples)

        self.input_task = None
        self.in_flight_n_samples_cond = asyncio.Condition()
        self.in_flight_n_samples = 0

        self.out_queue_cond = asyncio.Condition()
        self.out_queue = deque()

    def handle_output(self, output: bytes):
        async def send_chunk_to_writer_task(output: bytes):
            async with self.out_queue_cond:
                self.out_queue.append(output)
                self.out_queue_cond.notify()

        asyncio.run_coroutine_threadsafe(send_chunk_to_writer_task(output), self.loop)

    async def run(self):
        await self.websocket.send(json.dumps({
            'model_info': {
                'model_name': self.demucs_session.model_name,
                'sources': self.demucs_session.model_sources,
                'latency': self.demucs_session.latency_samples * 1000 // self.demucs_session.rate,
            }
        }))

        input_task = asyncio.current_task()
        assert input_task is not None
        self.input_task = input_task

        writer_task = self.loop.create_task(self.writer_task())

        try:
            async for message in self.websocket:
                if type(message) is str:
                    logger.warning(f'Unexpected string message from {self.websocket.remote_address}: {message}')
                    continue
                assert type(message) is bytes

                # Empty message => finish
                if len(message) == 0:
                    logger.info('Finishing')
                    break

                # Read as floats and de-interleave in one go
                n_samples = len(message) // session.CHANNELS // 4
                logger.debug(f'Processing message with {n_samples} samples')

                async with self.in_flight_n_samples_cond:
                    self.in_flight_n_samples += n_samples
                    logger.debug(f'{self.in_flight_n_samples} samples in flight after input')

                # Queue new samples for processing
                self.demucs_session.push_data(message)

                # Wait until fewer samples are in flight again
                async with self.in_flight_n_samples_cond:
                    while self.in_flight_n_samples >= self.max_in_flight_samples:
                        logger.debug(f'{self.in_flight_n_samples} samples in flight -- waiting')
                        await self.in_flight_n_samples_cond.wait()

            # No need to wait if the websocket was closed and that's why the loop is finished
            if self.websocket.state is websockets.protocol.State.OPEN:
                # Signal the processing to shut down after draining
                self.demucs_session.push_data(bytes())

                logger.debug('Waiting to drain')

                try:
                    await writer_task
                except Exception as e:
                    logger.warning(f'Failed waiting for draining to finish: {traceback.format_exception(e)}')
                    raise e

                await self.websocket.close()

                logger.debug('Drained')
            else:
                logger.debug('Shutting down input task immediately')

                self.demucs_session.cancel()
                writer_task.cancel()

                try:
                    await writer_task
                except asyncio.CancelledError:
                    current_task = asyncio.current_task()
                    assert current_task is not None
                    if current_task.cancelling() == 0:
                        raise
                    else:
                        pass

        except Exception as e:
            logger.warning(f'Shutting down because of exception: {traceback.format_exception(e)}')

            self.demucs_session.cancel()
            writer_task.cancel()
            raise e
        finally:
            self.input_task = None
            logger.debug('Finished input task')

    async def writer_task(self):
        try:
            while True:
                current_output = None
                n_samples = 0
                async with self.out_queue_cond:
                    while len(self.out_queue) == 0:
                        await self.out_queue_cond.wait()

                    current_output = self.out_queue.popleft()

                    if len(current_output) > 0:
                        n_samples = len(current_output) // session.CHANNELS // 4 // len(self.demucs_session.model_sources)
                        logger.debug(f'Handling output chunk with {n_samples} samples')
                    else:
                        assert len(self.out_queue) == 0
                        logger.debug('Drained writer task')
                        break

                assert current_output is not None
                try:
                    await self.websocket.send(current_output)
                except asyncio.CancelledError:
                    raise
                except Exception as e:
                    logger.debug(f'Failed sending output message: {traceback.format_exception(e)}')
                    raise

                async with self.in_flight_n_samples_cond:
                    assert self.in_flight_n_samples >= n_samples
                    self.in_flight_n_samples -= n_samples
                    logger.debug(f'{self.in_flight_n_samples} samples in flight after output')
                    self.in_flight_n_samples_cond.notify()

                # And next round

            try:
                await self.websocket.send(bytes())
            except asyncio.CancelledError:
                raise
            except Exception as e:
                logger.debug(f'Failed sending close message: {traceback.format_exception(e)}')

        except Exception as e:
            logger.warning(f'Shutting down writer task because of exception: {traceback.format_exception(e)}')

            if self.input_task is not None:
                self.input_task.cancel()
            # Processing thread is terminated by input task

            raise e
        finally:
            logger.debug('Finished writer task')

async def run_session(websocket: ServerConnection):
    from urllib.parse import urlparse, parse_qs

    assert websocket.request is not None

    url = urlparse(websocket.request.path)
    qs = parse_qs(url.query)

    try:
        model_name = qs['model-name'][0]
        rate = int(qs['rate'][0])
        chunk_duration = int(qs['chunk-duration'][0])
        overlap = float(qs['overlap'][0])

        logger.info(f'Starting new session for {websocket.remote_address} with model {model_name}, {rate}Hz, {chunk_duration} chunk duration, {overlap} overlap')
        session = Session(websocket, model_name, rate, chunk_duration, overlap)
    except asyncio.CancelledError:
        raise
    except Exception as e:
        logger.warning(f'Failed initializing session: {traceback.format_exception(e)}')
        await websocket.send(json.dumps({
            'error': f'Failed initializing session: {e}'
        }))
        return

    try:
        await session.run()
    finally:
        logger.info(f'Finished session for {websocket.remote_address}')

async def main(args):
    ssl_context = None
    if args.tls:
        import ssl
        ssl_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ssl_context.load_cert_chain(args.tls_cert, args.tls_key)

    async with serve(run_session, host=args.listen_address, port=args.listen_port, ssl=ssl_context, subprotocols=[Subprotocol('gst-demucs')]) as server:
        await server.serve_forever()

if __name__ == "__main__":
    import argparse
    import sys

    logging.basicConfig(stream=sys.stderr, level=logging.INFO)
    logger.setLevel(logging.DEBUG)

    parser = argparse.ArgumentParser()
    parser.add_argument('--listen-address', help='Host to listen on', default='localhost')
    parser.add_argument('--listen-port', help='Port to listen on', default=8765)
    parser.add_argument('--tls', help='Enable TLS', default=False)
    parser.add_argument('--tls-cert', help='TLS certificate', default=None, type=str)
    parser.add_argument('--tls-key', help='TLS private key', default=None, type=str)
    parser.add_argument('--model-repo', help='Model repo', default=None, type=Path)
    parser.add_argument('--model-device', help='Model device', default='cpu', type=str)
    args = parser.parse_args()

    session.init(args.model_device, args.model_repo)

    asyncio.run(main(args))
