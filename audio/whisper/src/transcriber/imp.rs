// Copyright (C) 2026 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-whispertranscriber
 *
 * Speech to Text element using [whisper_rs].
 *
 * The element uses a chunking strategy to make it compatible with live use cases. It will run
 * inference on each chunk, possibly prepended with the previous chunk to avoid misdetection
 * near the chunk boundaries.
 *
 * In live mode, its latency is a factor of:
 *
 * - The chunk-duration property
 * - The live-edge-offset property, which represents the duration by which the sliding window of
 *   output tokens trails the live edge, thus causing it to overlap with the previous chunk when
 *   non-zero
 * - The latency property, which must be configured to be greater than the actual observed
 *   processing latency for one inference run, and cannot be greater than the chunk duration.
 *
 * The element will log in order to assist with the process of tuning the latency property.
 *
 * In order to identify tokens the element needs to use [DTW token-level timestamps], and currently
 * does not support custom aheads, which means it can only be used with one of the models supported
 * by the model-preset property. That property must be set to match the actual model specified
 * through the model-path property, no check is performed to enforce this.
 *
 * The element re-exports the features exposed by the whisper-rs crate to select backends, this is
 * an example for building the element with CUDA support enabled:
 *
 * ```
 * cargo build --features=cuda
 * ```
 *
 * You can download models using the [whisper.cpp download script], this is an example for
 * downloading the large-v3 model:
 *
 * ```
 * ./download-ggml-model.sh large-v3
 * ```
 *
 * Equipped with this, this is an example for running live inference with the element introducing a
 * 6 seconds latency:
 *
 * ```
 * gst-launch-1.0 filesrc location=/home/meh/Music/chaplin.wav ! \
 *   wavparse ! audioconvert ! audioresample ! clocksync ! \
 *   queue max-size-time=5000000000 max-size-buffers=0 max-size-bytes=0 ! \
 *   whispertranscriber model-path=/home/meh/devel/whisper.cpp/models/ggml-large-v3.bin model-preset=large-v3 chunk-duration=4000 live-edge-offset=1000 latency=1000 ! \
 *   queue ! fakesink dump=true
 * ```
 *
 * The above is known to work fine using a RTX 5080 GPU.
 *
 * You can remove the clocksync element to test offline performance, the above pipeline
 * is known to yield a 10x real time processing rate using a RTX 5080 GPU, and
 * 7x real time for Vulkan on a Radeon RX9070 XT.
 *
 * [whisper_rs]: https://docs.rs/whisper-rs/latest/whisper_rs/
 * [DTW token-level timestamps]: https://docs.rs/whisper-rs/latest/whisper_rs/struct.DtwParameters.html
 * [whisper.cpp download script]: https://github.com/ggml-org/whisper.cpp/blob/master/models/download-ggml-model.sh
 *
 * Since: plugins-rs-0.15.0
 */
use super::{WhisperTranscriberModelPreset, WhisperTranscriberSamplingStrategy};
use byte_slice_cast::*;
use gst::subclass::prelude::*;
use gst::{glib, prelude::*};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperError,
    WhisperState, WhisperTokenId, set_log_callback,
};

use std::sync::{LazyLock, Mutex, MutexGuard, mpsc};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "whispertranscriber",
        gst::DebugColorFlags::empty(),
        Some("Whisper Speech to Text element"),
    )
});

#[derive(Debug, Clone)]
pub(super) struct Settings {
    latency_ms: u32,
    chunk_duration_ms: u32,
    live_edge_offset_ms: u32,
    model_path: Option<String>,
    use_gpu: bool,
    gpu_device_id: i32,
    n_threads: i32,
    translate: bool,
    language: Option<String>,
    detect_language: bool,
    suppress_blank: bool,
    suppress_nst: bool,
    debug_mode: bool,
    length_penalty: f32,
    entropy_thold: f32,
    logprob_thold: f32,
    model_preset: WhisperTranscriberModelPreset,
    sampling_strategy: WhisperTranscriberSamplingStrategy,
    greedy_best_of: i32,
    beam_search_size: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            latency_ms: 1_000,
            chunk_duration_ms: 4_000,
            live_edge_offset_ms: 1_000,
            model_path: None,
            use_gpu: true,
            gpu_device_id: 0,
            n_threads: 1,
            translate: false,
            language: None,
            detect_language: false,
            suppress_blank: true,
            suppress_nst: false,
            debug_mode: false,
            length_penalty: -1.0,
            entropy_thold: 2.4,
            logprob_thold: -1.0,
            model_preset: WhisperTranscriberModelPreset::Tiny,
            sampling_strategy: WhisperTranscriberSamplingStrategy::Greedy,
            greedy_best_of: 5,
            beam_search_size: 5,
        }
    }
}

#[cfg(any(not(windows), target_env = "gnu"))]
type LogLevel = u32;
#[cfg(all(windows, not(target_env = "gnu")))]
type LogLevel = i32;

unsafe extern "C" fn my_log_callback(
    level: LogLevel,
    text: *const i8,
    _user_data: *mut std::ffi::c_void,
) {
    let c_str = unsafe { std::ffi::CStr::from_ptr(text) };
    let Ok(text) = c_str.to_str() else {
        return;
    };

    let text = text.trim();

    match level {
        0 => gst::log!(WHISPERLIB_CAT, "{}", text),
        1 => gst::debug!(WHISPERLIB_CAT, "{}", text),
        2 => gst::info!(WHISPERLIB_CAT, "{}", text),
        3 => gst::warning!(WHISPERLIB_CAT, "{}", text),
        4 => gst::error!(WHISPERLIB_CAT, "{}", text),
        _ => (),
    }
}

static WHISPERLIB_CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    unsafe {
        set_log_callback(Some(my_log_callback), std::ptr::null_mut());
    }

    gst::DebugCategory::new(
        "whisperlib",
        gst::DebugColorFlags::empty(),
        Some("Whisper Library"),
    )
});

struct TokenAccumulator {
    gst_dtw: gst::ClockTime,
    data: Option<String>,
}

impl TokenAccumulator {
    fn drain(&mut self, gst_dtw_end: gst::ClockTime) -> Option<gst::Buffer> {
        self.data.take().map(|old_data| {
            let mut outbuf = gst::Buffer::from_slice(old_data);
            {
                let buf_mut = outbuf.get_mut().unwrap();
                buf_mut.set_pts(self.gst_dtw);
                buf_mut.set_duration(gst_dtw_end.saturating_sub(self.gst_dtw))
            }
            outbuf
        })
    }

    fn push(&mut self, gst_dtw: gst::ClockTime, data: String) -> Option<gst::Buffer> {
        if data.starts_with(' ') {
            let ret = self.drain(gst_dtw);

            self.data = Some(data);
            self.gst_dtw = gst_dtw;
            ret
        } else {
            if let Some(mut old_data) = self.data.take() {
                old_data.push_str(&data);
                self.data = Some(old_data);
                self.gst_dtw = gst_dtw;
            } else {
                self.data = Some(data);
            }
            None
        }
    }
}

#[derive(Default)]
struct State {
    // (live, min, max)
    upstream_latency: Option<(bool, gst::ClockTime, Option<gst::ClockTime>)>,
    model_state: Option<WhisperState>,
    // The chunk we are currently accumulating for inference
    current_chunk: Vec<f32>,
    // The previous chunk used for inference
    previous_chunk: Vec<f32>,
    // The first PTS of the oldest chunk (previous or current)
    chunked_pts: Option<gst::ClockTime>,
    // Used to determine whether a token is "special"
    token_eot: Option<WhisperTokenId>,
    out_pts: Option<gst::ClockTime>,
    inference_tx: Option<mpsc::Sender<(WhisperState, Result<i32, WhisperError>)>>,
    model_tx: Option<mpsc::Sender<WhisperState>>,
    model_rx: Option<mpsc::Receiver<WhisperState>>,
    thread_pool: Option<glib::ThreadPool>,
}

pub struct Transcriber {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl Transcriber {
    fn upstream_latency(&self) -> Option<(bool, gst::ClockTime, Option<gst::ClockTime>)> {
        if let Some(latency) = self.state.lock().unwrap().upstream_latency {
            return Some(latency);
        }

        let mut peer_query = gst::query::Latency::new();

        let ret = self.sinkpad.peer_query(&mut peer_query);

        if ret {
            let upstream_latency = peer_query.result();
            gst::info!(
                CAT,
                imp = self,
                "queried upstream latency: {upstream_latency:?}"
            );

            self.state.lock().unwrap().upstream_latency = Some(upstream_latency);

            Some(upstream_latency)
        } else {
            gst::trace!(CAT, imp = self, "could not query upstream latency");

            None
        }
    }

    fn push<'a>(
        &'a self,
        state: MutexGuard<'a, State>,
        mut output: Vec<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let Some(mut out_pts) = state.out_pts else {
            if !output.is_empty() {
                panic!("Trying to push non-empty output but no out pts was set");
            }
            return Ok(gst::FlowSuccess::Ok);
        };

        let chunked_pts = state.chunked_pts;

        drop(state);

        let (chunk_duration_ms, live_edge_offset_ms) = {
            let settings = self.settings.lock().unwrap();

            (
                settings.chunk_duration_ms as u64,
                settings.live_edge_offset_ms as u64,
            )
        };

        if let Some(chunked_pts) = chunked_pts {
            let sliding_window_start_edge = chunked_pts
                + gst::ClockTime::from_mseconds(
                    chunk_duration_ms.saturating_sub(live_edge_offset_ms),
                );
            if out_pts < sliding_window_start_edge {
                let _ = self.srcpad.push_event(
                    gst::event::Gap::builder(out_pts)
                        .duration(sliding_window_start_edge - out_pts)
                        .build(),
                );
                out_pts = sliding_window_start_edge;
            }
        }

        for buffer in output.drain(..) {
            let buf_pts = buffer.pts().unwrap();
            let buf_end_pts = buf_pts + buffer.duration().unwrap();

            if buf_pts > out_pts {
                let _ = self.srcpad.push_event(
                    gst::event::Gap::builder(out_pts)
                        .duration(buf_pts - out_pts)
                        .build(),
                );
            }

            self.srcpad.push(buffer)?;

            out_pts = buf_end_pts;
        }

        self.state.lock().unwrap().out_pts = Some(out_pts);

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling event {event:?}");

        use gst::EventView::*;
        match event.view() {
            FlushStart(_) => {
                {
                    let mut state = self.state.lock().unwrap();
                    let _ = state.inference_tx.take();
                    let _ = state.model_tx.take();
                }

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            FlushStop(_) => {
                // Make sure pad.push returned
                let _ = self.sinkpad.stream_lock();

                {
                    let mut state = self.state.lock().unwrap();

                    let token_eot = state.token_eot;
                    *state = State::default();
                    state.token_eot = token_eot;
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            Caps(_) => self.srcpad.push_event(
                gst::event::Caps::builder(self.srcpad.pad_template().unwrap().caps())
                    .seqnum(event.seqnum())
                    .build(),
            ),
            Eos(_) | Segment(_) | Gap(_) | SegmentDone(_) => {
                let mut output: Vec<gst::Buffer> = vec![];

                let mut state = self.state.lock().unwrap();
                state = match self.run_inference(state, true, &mut output) {
                    Ok(state) => state,
                    Err(_) => self.state.lock().unwrap(),
                };

                let _ = self.push(state, output);

                if let Gap(gap) = event.view() {
                    let (pts, duration) = gap.get();
                    self.state.lock().unwrap().out_pts =
                        Some(pts + duration.unwrap_or(gst::ClockTime::ZERO));
                }

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling query {:?}", query);

        match query.view_mut() {
            gst::QueryViewMut::Latency(ref mut q) => {
                if let Some((live, min, max)) = self.upstream_latency() {
                    if live {
                        let settings = self.settings.lock().unwrap();
                        let our_min_latency = gst::ClockTime::from_mseconds(
                            (settings.latency_ms
                                + settings.chunk_duration_ms
                                + settings.live_edge_offset_ms) as u64,
                        );
                        q.set(live, our_min_latency + min, max.opt_add(our_min_latency));
                    } else {
                        q.set(false, gst::ClockTime::ZERO, gst::ClockTime::NONE);
                    }
                    true
                } else {
                    false
                }
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }

    fn run_inference<'a>(
        &'a self,
        mut state: MutexGuard<'a, State>,
        drain: bool,
        output: &mut Vec<gst::Buffer>,
    ) -> Result<MutexGuard<'a, State>, gst::FlowError> {
        let Some(chunked_pts) = state.chunked_pts else {
            return Ok(state);
        };

        let mut model_state = match state.model_state.take() {
            None => {
                let Some(model_rx) = state.model_rx.take() else {
                    return Err(gst::FlowError::Flushing);
                };

                drop(state);

                let model_state = match model_rx.recv() {
                    Err(_) => {
                        return Err(gst::FlowError::Flushing);
                    }
                    Ok(model_state) => model_state,
                };

                state = self.state.lock().unwrap();

                model_state
            }
            Some(model_state) => model_state,
        };

        let is_live = state
            .upstream_latency
            .map(|(live, _, _)| live)
            .unwrap_or(false);

        gst::debug!(
            CAT,
            imp = self,
            "Running inference from chunk PTS {chunked_pts}, drain: {drain}"
        );

        let settings_clone = self.settings.lock().unwrap().clone();

        let (chunk_duration_ms, live_edge_offset_ms, our_latency) = {
            (
                settings_clone.chunk_duration_ms as u64,
                settings_clone.live_edge_offset_ms as u64,
                gst::ClockTime::from_mseconds(settings_clone.latency_ms as u64),
            )
        };

        let mut chunk_to_process = state.previous_chunk.clone();
        chunk_to_process.extend(&state.current_chunk);

        let processed_duration = gst::ClockTime::SECOND
            .mul_div_floor(chunk_to_process.len() as u64, 16_000)
            .unwrap();

        let clock = self.obj().clock();

        let now = clock.as_ref().map(|clock| clock.time());

        let (inference_tx, inference_rx) = mpsc::channel();

        state.inference_tx = Some(inference_tx);

        let this_weak = self.downgrade();

        if let Err(err) = state.thread_pool.as_ref().unwrap().push(move || {
            let mut params = FullParams::new(match settings_clone.sampling_strategy {
                WhisperTranscriberSamplingStrategy::Greedy => SamplingStrategy::Greedy {
                    best_of: settings_clone.greedy_best_of,
                },
                WhisperTranscriberSamplingStrategy::BeamSearch => SamplingStrategy::BeamSearch {
                    beam_size: settings_clone.beam_search_size,
                    patience: -1.,
                },
            });

            params.set_token_timestamps(true);

            params.set_n_threads(settings_clone.n_threads);
            params.set_translate(settings_clone.translate);
            params.set_detect_language(settings_clone.detect_language);
            params.set_suppress_blank(settings_clone.suppress_blank);
            params.set_suppress_nst(settings_clone.suppress_nst);
            params.set_debug_mode(settings_clone.debug_mode);
            params.set_length_penalty(settings_clone.length_penalty);
            params.set_entropy_thold(settings_clone.entropy_thold);
            params.set_logprob_thold(settings_clone.logprob_thold);
            params.set_language(settings_clone.language.as_deref());

            let result = model_state.full(params, &chunk_to_process[..]);

            if let Some(this) = this_weak.upgrade() {
                gst::debug!(CAT, imp = this, "Ran inference: {result:?}");
                if let Some(tx) = this.state.lock().unwrap().inference_tx.take() {
                    let _ = tx.send((model_state, result));
                }
            }
        }) {
            drop(state);
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["Failed to spawn inference thread: {err}"]
            );
            return Err(gst::FlowError::Error);
        }

        drop(state);

        let (model_state, result) = match inference_rx.recv() {
            Ok(res) => res,
            Err(_) => {
                return Err(gst::FlowError::Flushing);
            }
        };

        let mut state = self.state.lock().unwrap();

        state.model_state = Some(model_state);

        if let Err(err) = result {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["inference failed: {err}"]);
            return Err(gst::FlowError::Error);
        }

        if let Some(now) = now {
            let actual_processing_time = clock.unwrap().time().saturating_sub(now);
            gst::log!(
                CAT,
                imp = self,
                "Actual processing time: {}",
                actual_processing_time
            );
            if is_live && actual_processing_time > our_latency {
                gst::warning!(
                    CAT,
                    imp = self,
                    "actual processing time {actual_processing_time} greater than configured latency {our_latency}"
                );
            }
        }

        let had_previous_chunk = !state.previous_chunk.is_empty();

        let mut token_accumulator = TokenAccumulator {
            gst_dtw: gst::ClockTime::ZERO,
            data: None,
        };
        for segment in state.model_state.as_ref().unwrap().as_iter() {
            for idx in 0..segment.n_tokens() {
                let token = segment.get_token(idx).unwrap();
                let Ok(token_str) = token.to_str() else {
                    continue;
                };

                // Logic taken from whisper.cpp's print_special logic
                let is_special_token = token.token_id() >= state.token_eot.unwrap();
                if is_special_token {
                    continue;
                }
                let t_dtw_ms = token.token_data().t_dtw as u64 * 10;

                let is_continuation_token = !token_str.starts_with(' ');

                let out_of_bounds = if !drain {
                    if had_previous_chunk {
                        t_dtw_ms < chunk_duration_ms - live_edge_offset_ms
                            || t_dtw_ms >= chunk_duration_ms * 2 - live_edge_offset_ms
                    } else {
                        t_dtw_ms >= chunk_duration_ms - live_edge_offset_ms
                    }
                } else if had_previous_chunk {
                    t_dtw_ms < chunk_duration_ms - live_edge_offset_ms
                } else {
                    false
                };

                let gst_dtw = gst::ClockTime::from_mseconds(t_dtw_ms) + chunked_pts;

                gst::log!(
                    CAT,
                    imp = self,
                    "considering one token {token_str} with t_dtw_ms {t_dtw_ms}, out of bounds: {out_of_bounds}"
                );

                if !out_of_bounds || (token_accumulator.data.is_some() && is_continuation_token) {
                    if token_accumulator.data.is_none() && is_continuation_token {
                        continue;
                    }

                    if let Some(buffer) = token_accumulator.push(gst_dtw, token_str.to_string()) {
                        output.push(buffer);
                    }
                }

                if out_of_bounds
                    && !is_continuation_token
                    && let Some(buffer) = token_accumulator.drain(gst_dtw)
                {
                    output.push(buffer);
                }
            }
        }

        if let Some(buffer) = token_accumulator.drain(chunked_pts + processed_duration) {
            output.push(buffer);
        }

        if drain {
            state.current_chunk.clear();
            state.previous_chunk.clear();
            let _ = state.chunked_pts.take();
        }

        Ok(state)
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling {buffer:?}");

        if buffer.pts().is_none() {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["need timestamped buffers"]);
            return Err(gst::FlowError::Error);
        }

        let Ok(data) = buffer.map_readable() else {
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["failed to map buffer readable"]
            );
            return Err(gst::FlowError::Error);
        };

        let upstream_latency = self.upstream_latency();

        let (chunk_duration_ms, our_latency_ms) = {
            let settings = self.settings.lock().unwrap();
            (settings.chunk_duration_ms, settings.latency_ms)
        };

        if let Some((live, _, _)) = upstream_latency
            && live
            && our_latency_ms > chunk_duration_ms
        {
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["Upstream is live and configured latency larger than chunk-duration"]
            );
            return Err(gst::FlowError::Error);
        }

        let mut state = self.state.lock().unwrap();

        if state.chunked_pts.is_none() {
            state.chunked_pts = Some(buffer.pts().unwrap());
            state.out_pts = state.chunked_pts;
        }

        let mut output: Vec<gst::Buffer> = vec![];

        let mut offset = 0;
        // 16 samples per millisecond, 4 bytes per sample
        let chunk_size = chunk_duration_ms as usize * 16 * 4;
        while offset < data.len() {
            let end_offset =
                offset + (data.len() - offset).min(chunk_size - (state.current_chunk.len() * 4));
            state
                .current_chunk
                .extend(data[offset..end_offset].as_slice_of::<f32>().unwrap());

            assert!(state.current_chunk.len() * 4 <= chunk_size);

            if state.current_chunk.len() * 4 == chunk_size {
                state = self.run_inference(state, false, &mut output)?;

                state.chunked_pts = Some(
                    state.chunked_pts.unwrap()
                        + gst::ClockTime::SECOND
                            .mul_div_floor(state.previous_chunk.len() as u64, 16_000)
                            .unwrap(),
                );

                state.previous_chunk = state.current_chunk.clone();
                state.current_chunk = vec![];
            }

            offset = end_offset;
        }

        self.push(state, output)
    }

    fn prepare(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Preparing");

        gst::debug!(WHISPERLIB_CAT, imp = self, "Initializing context");

        let model_path = {
            let settings = self.settings.lock().unwrap();

            let Some(model_path) = settings.model_path.clone() else {
                return Err(gst::error_msg!(
                    gst::CoreError::Failed,
                    ["model-path property must be set"]
                ));
            };

            if settings.live_edge_offset_ms >= settings.chunk_duration_ms {
                return Err(gst::error_msg!(
                    gst::CoreError::Failed,
                    ["chunk-duration must be greater than live-edge-offset"]
                ));
            }

            model_path
        };

        let context_params = {
            let mut params = WhisperContextParameters::default();
            // TODO: allow for custom aheads
            let settings = self.settings.lock().unwrap();
            params.dtw_parameters.mode = whisper_rs::DtwMode::ModelPreset {
                model_preset: settings.model_preset.into(),
            };

            params.use_gpu = settings.use_gpu;
            params.gpu_device = settings.gpu_device_id;

            params
        };

        let (model_tx, model_rx) = mpsc::channel();

        let Ok(threadpool) = glib::ThreadPool::shared(None) else {
            return Err(gst::error_msg!(
                gst::CoreError::Failed,
                ["Failed to create whisper thread pool"]
            ));
        };

        self.post_start("loading", "loading model");

        let mut state = self.state.lock().unwrap();
        state.model_tx = Some(model_tx);
        state.model_rx = Some(model_rx);
        state.thread_pool = Some(threadpool);

        let this_weak = self.downgrade();
        if let Err(err) = state.thread_pool.as_ref().unwrap().push(move || {
            let ctx = match WhisperContext::new_with_params(&model_path, context_params) {
                Ok(ctx) => ctx,
                Err(err) => {
                    if let Some(this) = this_weak.upgrade() {
                        this.post_cancelled("loading", &format!("Failed to load model!: {}", err));
                    }

                    return;
                }
            };

            let model_state = ctx.create_state().expect("failed to create state");

            if let Some(this) = this_weak.upgrade() {
                let mut state = this.state.lock().unwrap();

                state.token_eot = Some(ctx.token_eot());

                let post_complete = if let Some(tx) = state.model_tx.take() {
                    let _ = tx.send(model_state);
                    true
                } else {
                    false
                };

                drop(state);

                if post_complete {
                    this.post_complete("loading", "loaded model");
                } else {
                    this.post_cancelled("loading", "model loading interrupted");
                }
            }
        }) {
            return Err(gst::error_msg!(
                gst::CoreError::Failed,
                ["Failed to spawn load_model thread: {err}"]
            ));
        }

        Ok(())
    }

    fn post_start(&self, code: &str, text: &str) {
        let obj = self.obj();
        let msg = gst::message::Progress::builder(gst::ProgressType::Start, code, text)
            .src(&*obj)
            .build();
        let _ = obj.post_message(msg);
    }

    fn post_complete(&self, code: &str, text: &str) {
        let obj = self.obj();
        let msg = gst::message::Progress::builder(gst::ProgressType::Complete, code, text)
            .src(&*obj)
            .build();
        let _ = obj.post_message(msg);
    }

    fn post_cancelled(&self, code: &str, text: &str) {
        let obj = self.obj();
        let msg = gst::message::Progress::builder(gst::ProgressType::Canceled, code, text)
            .src(&*obj)
            .build();
        let _ = obj.post_message(msg);
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Transcriber {
    const NAME: &'static str = "GstWhisperTranscriber";
    type Type = super::Transcriber;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::PadBuilder::<gst::Pad>::from_template(&templ)
            .query_function(|pad, parent, query| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.src_query(pad, query),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Default::default(),
            state: Default::default(),
        }
    }
}

impl ObjectImpl for Transcriber {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
            glib::ParamSpecUInt::builder("latency")
                .nick("Latency")
                .blurb("The expected processing latency. Will count towards total latency.")
                .default_value(Settings::default().latency_ms)
                .build(),
            glib::ParamSpecUInt::builder("chunk-duration")
                .nick("Chunk Duration")
                .blurb("The duration of chunks to accumulate for inference, in milliseconds. \
                    Will count towards total latency.")
                .default_value(Settings::default().chunk_duration_ms)
                .build(),
            glib::ParamSpecUInt::builder("live-edge-offset")
                .nick("Live Edge Offset")
                .blurb("The element will feed in the previous chunk when running inference, \
                    and output tokens that are contained within a sliding window that may overlap both chunks. \
                    This controls the duration (in milliseconds) of the overlap, and will leave time for tokens \
                    near the end of the current chunk to stabilize. Will count towards total latency.")
                .default_value(Settings::default().live_edge_offset_ms)
                .build(),
            glib::ParamSpecString::builder("model-path")
                .nick("Model Path")
                .blurb("Path to ggml-formatted whisper model (https://github.com/ggml-org/whisper.cpp?tab=readme-ov-file#ggml-format)")
                .default_value(None)
                .build(),
            glib::ParamSpecBoolean::builder("use-gpu")
                .nick("Use GPU")
                .blurb("Use GPU if available.")
                .default_value(Settings::default().use_gpu)
                .build(),
            glib::ParamSpecInt::builder("gpu-device-id")
                .nick("GPU Device ID")
                .blurb("GPU device id")
                .minimum(0)
                .default_value(Settings::default().gpu_device_id)
                .build(),
            glib::ParamSpecInt::builder("n-threads")
                .nick("Number of Threads")
                .blurb("Set the number of threads to use for decoding.")
                .minimum(0)
                .default_value(Settings::default().n_threads)
                .build(),
            glib::ParamSpecBoolean::builder("translate")
                .nick("Translate")
                .blurb("Whether to translate to English for multilingual models")
                .default_value(Settings::default().translate)
                .build(),
            glib::ParamSpecString::builder("language")
                .nick("Language")
                .blurb("The source language to translate from when translate is true")
                .default_value(None)
                .build(),
            glib::ParamSpecBoolean::builder("detect-language")
                .nick("Detect Language")
                .blurb("Auto-detect the source language when translate is true")
                .default_value(Settings::default().detect_language)
                .build(),
            glib::ParamSpecBoolean::builder("suppress-blank")
                .nick("Suppress Blank")
                .blurb("This will suppress blank outputs")
                .default_value(Settings::default().suppress_blank)
                .build(),
            glib::ParamSpecBoolean::builder("suppress-nst")
                .nick("Suppress NST")
                .blurb("This will suppress non-speech tokens")
                .default_value(Settings::default().suppress_nst)
                .build(),
            glib::ParamSpecBoolean::builder("debug-mode")
                .nick("Debug Mode")
                .blurb("Enables debug mode, such as dumping the log mel spectrogram.")
                .default_value(Settings::default().debug_mode)
                .build(),
            glib::ParamSpecFloat::builder("length-penalty")
                .nick("Length Penalty")
                .blurb("optional token length penalty coefficient (alpha) as in https://arxiv.org/abs/1609.08144, uses simple length normalization by default")
                .default_value(Settings::default().length_penalty)
                .build(),
            glib::ParamSpecFloat::builder("entropy-thold")
                .nick("Entropy Threshold")
                .blurb("If the gzip compression ratio is higher than this value, treat the decoding as failed")
                .default_value(Settings::default().entropy_thold)
                .build(),
            glib::ParamSpecFloat::builder("logprob-thold")
                .nick("Log Probability Threshold")
                .blurb("if the average log probability is lower than this value, treat the decoding as failed")
                .default_value(Settings::default().logprob_thold)
                .build(),
            glib::ParamSpecEnum::builder_with_default("model-preset", Settings::default().model_preset)
                .nick("Model Preset")
                .blurb("Defines how DTW token-level timestamps are gathered, MUST MATCH THE SPECIFIED MODEL")
                .build(),
            glib::ParamSpecEnum::builder_with_default("sampling-strategy", Settings::default().sampling_strategy)
                .nick("Sampling Strategy")
                .blurb("The sampling strategy to use to pick tokens from a list of likely possibilities")
                .build(),
            glib::ParamSpecInt::builder("greedy-best-of")
                .nick("Greedy Best Of")
                .blurb("Set the best_of value for sampling-strategy=greedy")
                .minimum(1)
                .default_value(Settings::default().greedy_best_of)
                .build(),
            glib::ParamSpecInt::builder("beam-search-size")
                .nick("Beam Search Size")
                .blurb("Set the beam_size value for sampling-strategy=beam-search")
                .minimum(1)
                .default_value(Settings::default().beam_search_size)
                .build(),
        ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "latency" => {
                self.settings.lock().unwrap().latency_ms = value.get().unwrap();
            }
            "chunk-duration" => {
                self.settings.lock().unwrap().chunk_duration_ms = value.get().unwrap();
            }
            "live-edge-offset" => {
                self.settings.lock().unwrap().live_edge_offset_ms = value.get().unwrap();
            }
            "model-path" => {
                self.settings.lock().unwrap().model_path = value.get().unwrap();
            }
            "use-gpu" => {
                self.settings.lock().unwrap().use_gpu = value.get().unwrap();
            }
            "gpu-device-id" => {
                self.settings.lock().unwrap().gpu_device_id = value.get().unwrap();
            }
            "n-threads" => {
                self.settings.lock().unwrap().n_threads = value.get().unwrap();
            }
            "translate" => {
                self.settings.lock().unwrap().translate = value.get().unwrap();
            }
            "language" => {
                self.settings.lock().unwrap().language = value.get().unwrap();
            }
            "detect-language" => {
                self.settings.lock().unwrap().detect_language = value.get().unwrap();
            }
            "suppress-blank" => {
                self.settings.lock().unwrap().suppress_blank = value.get().unwrap();
            }
            "suppress-nst" => {
                self.settings.lock().unwrap().suppress_nst = value.get().unwrap();
            }
            "debug-mode" => {
                self.settings.lock().unwrap().debug_mode = value.get().unwrap();
            }
            "length-penalty" => {
                self.settings.lock().unwrap().length_penalty = value.get().unwrap();
            }
            "entropy-thold" => {
                self.settings.lock().unwrap().entropy_thold = value.get().unwrap();
            }
            "logprob-thold" => {
                self.settings.lock().unwrap().logprob_thold = value.get().unwrap();
            }
            "model-preset" => {
                self.settings.lock().unwrap().model_preset = value.get().unwrap();
            }
            "sampling-strategy" => {
                self.settings.lock().unwrap().sampling_strategy = value.get().unwrap();
            }
            "greedy-best-of" => {
                self.settings.lock().unwrap().greedy_best_of = value.get().unwrap();
            }
            "beam-search-size" => {
                self.settings.lock().unwrap().beam_search_size = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "latency" => self.settings.lock().unwrap().latency_ms.to_value(),
            "chunk-duration" => self.settings.lock().unwrap().chunk_duration_ms.to_value(),
            "live-edge-offset" => self.settings.lock().unwrap().live_edge_offset_ms.to_value(),
            "model-path" => self.settings.lock().unwrap().model_path.to_value(),
            "use-gpu" => self.settings.lock().unwrap().use_gpu.to_value(),
            "gpu-device-id" => self.settings.lock().unwrap().gpu_device_id.to_value(),
            "n-threads" => self.settings.lock().unwrap().n_threads.to_value(),
            "translate" => self.settings.lock().unwrap().translate.to_value(),
            "language" => self.settings.lock().unwrap().language.to_value(),
            "detect-language" => self.settings.lock().unwrap().detect_language.to_value(),
            "suppress-blank" => self.settings.lock().unwrap().suppress_blank.to_value(),
            "suppress-nst" => self.settings.lock().unwrap().suppress_nst.to_value(),
            "debug-mode" => self.settings.lock().unwrap().debug_mode.to_value(),
            "length-penalty" => self.settings.lock().unwrap().length_penalty.to_value(),
            "entropy-thold" => self.settings.lock().unwrap().entropy_thold.to_value(),
            "logprob-thold" => self.settings.lock().unwrap().logprob_thold.to_value(),
            "model-preset" => self.settings.lock().unwrap().model_preset.to_value(),
            "sampling-strategy" => self.settings.lock().unwrap().sampling_strategy.to_value(),
            "greedy-best-of" => self.settings.lock().unwrap().greedy_best_of.to_value(),
            "beam-search-size" => self.settings.lock().unwrap().beam_search_size.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Transcriber {}

impl ElementImpl for Transcriber {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Transcriber",
                "Text/Audio/Filter",
                "Speech to Text filter, using Whisper",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            #[cfg(target_endian = "little")]
            let format = gst_audio::AudioFormat::F32le;
            #[cfg(target_endian = "big")]
            let format = gst_audio::AudioFormat::F32be;

            let sink_caps = gst_audio::AudioCapsBuilder::new()
                .format(format)
                .rate(16_000)
                .channels(1)
                .layout(gst_audio::AudioLayout::Interleaved)
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder_full()
                .structure(
                    gst::Structure::builder("text/x-raw")
                        .field("format", "utf8")
                        .build(),
                )
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::info!(CAT, imp = self, "Changing state {transition:?}");

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare().map_err(|err| {
                    self.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::ReadyToPaused => {}
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                let _ = state.inference_tx.take();
                let _ = state.model_tx.take();
            }
            _ => (),
        }

        self.parent_change_state(transition)
    }
}
