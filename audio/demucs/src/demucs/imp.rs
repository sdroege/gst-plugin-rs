// Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-demucs
 *
 * Music source separation element using [demucs].
 *
 * By default the element makes use of the (demucs)[pip-demucs] Python module, which must be
 * available locally. It is possible to install the module via `pip` or use a virtualenv via [uv]
 * or similar.
 *
 * Alternatively, the element can connect to a small Python service that does the actual
 * processing. The service is provided as part of the plugin and has to be started separately,
 * either on the same machine or another machine. It can handle multiple sessions from multiple
 * plugin instances in parallel.
 *
 * The service can be run with [uv] in a virtualenv via `uv run service/main.py`. See `uv run
 * service/main.py --help` for more configuration options.
 *
 * [demucs]: https://github.com/adefossez/demucs
 * [pip-demucs]: https://pypi.org/project/demucs/
 * [uv]: https://docs.astral.sh/uv/
 *
 * ## Example pipeline outputting only vocals
 *
 * |[
 * gst-launch-1.0 uridecodebin uri=file:///path/to/music/file ! audioconvert ! demucs name=demucs   demucs.src_vocals ! queue ! audioconvert ! autoaudiosink
 * ]| This will separate the vocals from the audio input and only play back the vocals.
 *
 * ## Example pipeline outputting the original audio without vocals
 *
 * |[
 * gst-launch-1.0 uridecodebin uri=file:///path/to/music/file ! audioconvert ! tee name=t ! queue max-size-time=0 max-size-bytes=0 max-size-buffers=2 ! demucs name=demucs model-name=htdemucs   demucs.src_vocals ! queue ! audioamplify amplification=-1 ! mixer.sink_0   t. ! queue max-size-time=9000000000 max-size-bytes=0 max-size-buffers=0 ! mixer.sink_1   audiomixer name=mixer ! audioconvert ! autoaudiosink
 * ]| This will separate the vocals from the audio input and then subtract it from the input
 * audio by using `audioamplify` to invert the samples and `audiomixer` to mix it into the input.
 *
 * Since: plugins-rs-0.15.0
 */
use gst::{glib, prelude::*, subclass::prelude::*};

use std::{
    cmp, mem,
    ops::ControlFlow,
    sync::{Condvar, LazyLock, Mutex, MutexGuard},
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "demucs",
        gst::DebugColorFlags::empty(),
        Some("Demucs Element"),
    )
});

#[cfg(feature = "websocket")]
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

pub struct Demucs {
    sinkpad: gst::Pad,

    state: Mutex<State>,
    state_cond: Condvar,
    settings: Mutex<Settings>,
}

#[cfg(feature = "websocket")]
type WsSink = std::pin::Pin<
    Box<
        dyn futures::Sink<
                async_tungstenite::tungstenite::Message,
                Error = async_tungstenite::tungstenite::Error,
            > + Send
            + Sync,
    >,
>;

struct State {
    srcpads: Vec<gst::Pad>,
    latency: Option<gst::ClockTime>,
    flow_combiner: gst_base::UniqueFlowCombiner,
    last_flow: Result<gst::FlowSuccess, gst::FlowError>,
    info: Option<gst_audio::AudioInfo>,
    segment: Option<gst::FormattedSegment<gst::ClockTime>>,
    segment_seqnum: Option<gst::Seqnum>,
    in_position: Option<gst::ClockTime>,
    out_position_base: Option<gst::ClockTime>,
    out_n_samples: u64,
    out_position: Option<gst::ClockTime>,
    draining: bool,

    initialized: bool,

    #[cfg(feature = "inprocess")]
    demucs: Option<python::Session>,
    #[cfg(feature = "inprocess")]
    in_flight_samples: u64,
    #[cfg(feature = "inprocess")]
    max_in_flight_samples: u64,
    #[cfg(feature = "inprocess")]
    write_abort_handle: Option<futures::future::AbortHandle>,

    #[cfg(feature = "websocket")]
    read_task_handle: Option<tokio::task::JoinHandle<()>>,
    #[cfg(feature = "websocket")]
    send_task_handle: Option<tokio::task::AbortHandle>,
    #[cfg(feature = "websocket")]
    ws_sink: Option<WsSink>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            srcpads: Vec::new(),
            latency: None,
            flow_combiner: gst_base::UniqueFlowCombiner::default(),
            last_flow: Err(gst::FlowError::Flushing),
            info: None,
            segment: None,
            segment_seqnum: None,
            in_position: None,
            out_position_base: None,
            out_n_samples: 0,
            out_position: None,
            draining: false,

            initialized: false,

            #[cfg(feature = "inprocess")]
            demucs: None,
            #[cfg(feature = "inprocess")]
            in_flight_samples: 0,
            #[cfg(feature = "inprocess")]
            max_in_flight_samples: 0,
            #[cfg(feature = "inprocess")]
            write_abort_handle: None,

            #[cfg(feature = "websocket")]
            read_task_handle: None,
            #[cfg(feature = "websocket")]
            send_task_handle: None,
            #[cfg(feature = "websocket")]
            ws_sink: None,
        }
    }
}

#[derive(Clone)]
struct Settings {
    url: Option<url::Url>,
    chunk_duration: gst::ClockTime,
    overlap: f32,
    processing_latency: gst::ClockTime,
    model_name: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            url: None,
            chunk_duration: gst::ClockTime::from_seconds(3),
            overlap: 0.25,
            processing_latency: gst::ClockTime::from_seconds(1),
            model_name: String::from("htdemucs"),
        }
    }
}

impl Demucs {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.lock().unwrap();
        if buffer.flags().contains(gst::BufferFlags::DISCONT) {
            gst::debug!(CAT, imp = self, "Got discont buffer");
            state = self.drain(state);
        }

        let (state, res) = self.init_demucs(state);
        if let Err(err) = res {
            self.post_error_message(err);
            return Err(gst::FlowError::Error);
        }

        if buffer.pts().is_none() {
            gst::error!(CAT, imp = self, "Need buffers with PTS");
            return Err(gst::FlowError::Error);
        }

        let Ok(buffer) = buffer.into_mapped_buffer_readable() else {
            gst::error!(CAT, imp = self, "Failed to map buffer readable");
            return Err(gst::FlowError::Error);
        };

        self.handle_buffer(state, Some(buffer)).1
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling event {:?}", event);

        match event.view() {
            gst::EventView::Eos(_) => {
                let state = self.state.lock().unwrap();
                let (mut state, res) = self.handle_buffer(state, None);
                if res.is_ok() {
                    gst::debug!(CAT, imp = self, "Draining asynchronously");
                    // EOS is forwarded later once the last samples are processed
                    true
                } else {
                    gst::debug!(CAT, imp = self, "Failed to drain");
                    // On error just shut down and forward EOS immediately
                    state = self.deinit_demucs(state);

                    // Stop pad tasks now so we can forward EOS safely from here
                    let srcpads = state.srcpads.clone();
                    drop(state);

                    for pad in srcpads {
                        let _ = pad.stop_task();
                    }
                    gst::Pad::event_default(pad, Some(&*self.obj()), event)
                }
            }
            gst::EventView::FlushStart(_) => {
                gst::info!(CAT, imp = self, "Received flush start, disconnecting");

                let mut state = self.state.lock().unwrap();
                state.last_flow = Err(gst::FlowError::Flushing);

                state = self.deinit_demucs(state);
                drop(state);

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            gst::EventView::FlushStop(_) => {
                gst::info!(CAT, imp = self, "Received flush stop, resetting state");

                let mut state = self.state.lock().unwrap();
                state.flow_combiner.reset();
                state.last_flow = Ok(gst::FlowSuccess::Ok);
                state.segment = None;
                state.segment_seqnum = None;
                state.in_position = None;
                state.out_position_base = None;
                state.out_n_samples = 0;
                state.out_position = None;
                state.draining = false;
                drop(state);

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            gst::EventView::Segment(e) => {
                let segment = match e.segment().clone().downcast::<gst::ClockTime>() {
                    Err(segment) => {
                        gst::element_imp_error!(
                            self,
                            gst::StreamError::Format,
                            ["Only Time segments supported, got {:?}", segment.format(),]
                        );
                        return false;
                    }
                    Ok(segment) => segment,
                };

                let mut state = self.state.lock().unwrap();

                // If segment changes in any other way than the position
                // we'll have to drain first.
                if let Some(ref old_segment) = state.segment {
                    let mut compare_segment = segment.clone();
                    compare_segment.set_position(old_segment.position());
                    if state.segment != Some(compare_segment) {
                        state = self.drain(state);
                    }
                }

                state.segment = Some(segment);
                state.segment_seqnum = Some(e.seqnum());
                let srcpads = state.srcpads.clone();
                drop(state);

                let mut res = true;
                for pad in srcpads {
                    res &= pad.push_event(event.clone());
                }

                res
            }
            gst::EventView::StreamStart(e) => {
                let mut res = true;
                let srcpads = self.state.lock().unwrap().srcpads.clone();
                for pad in srcpads {
                    let pad_name = pad.name();
                    let stream_id = pad_name.strip_prefix("src_").unwrap();
                    let stream_id = pad.create_stream_id(&*self.obj(), Some(stream_id));
                    let event = gst::event::StreamStart::builder(&stream_id)
                        .flags(e.stream_flags())
                        .seqnum(e.seqnum())
                        .group_id(e.group_id().unwrap_or(gst::GroupId::next()))
                        .stream_if_some(e.stream())
                        .build();
                    res &= pad.push_event(event);
                }
                res
            }
            gst::EventView::Caps(e) => {
                let mut state = self.state.lock().unwrap();
                let info = gst_audio::AudioInfo::from_caps(e.caps()).unwrap();

                if state
                    .info
                    .as_ref()
                    .is_some_and(|old_info| old_info != &info)
                {
                    state = self.drain(state);
                }

                state.info = Some(info);
                let srcpads = state.srcpads.clone();
                drop(state);

                let mut res = true;
                for pad in srcpads {
                    res &= pad.push_event(event.clone());
                }
                res
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling query {:?}", query);

        match query.view_mut() {
            gst::QueryViewMut::Latency(ref mut q) => {
                let mut peer_query = gst::query::Latency::new();

                let mut ret = self.sinkpad.peer_query(&mut peer_query);

                if ret {
                    let (live, min, max) = peer_query.result();

                    let state = self.state.lock().unwrap();
                    if let Some(latency) = state.latency {
                        let settings = self.settings.lock().unwrap();
                        let our_min_latency = latency + settings.processing_latency;
                        // The server blocks once 10s of samples or 3 chunks or 2 times latency are
                        // queued, which is also the same we use for max_in_flight_samples in
                        // in-process mode
                        let our_max_latency = cmp::max(
                            cmp::max(
                                gst::ClockTime::from_mseconds(10_000),
                                3 * settings.chunk_duration,
                            ),
                            2 * latency,
                        );
                        q.set(live, our_min_latency + min, max.opt_add(our_max_latency));
                    } else {
                        ret = false;
                    }
                }
                ret
            }
            gst::QueryViewMut::Position(ref mut q) => {
                if q.format() == gst::Format::Time {
                    let state = self.state.lock().unwrap();
                    if let Some(ref segment) = state.segment {
                        q.set(segment.to_stream_time(state.out_position));
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }

    fn init_demucs<'a>(
        &'a self,
        mut state: MutexGuard<'a, State>,
    ) -> (MutexGuard<'a, State>, Result<(), gst::ErrorMessage>) {
        if state.initialized {
            return (state, Ok(()));
        }

        let Some(ref info) = state.info else {
            gst::error!(CAT, imp = self, "No caps yet");
            return (
                state,
                Err(gst::error_msg!(gst::CoreError::Failed, ["No caps yet"])),
            );
        };

        let settings = self.settings.lock().unwrap().clone();

        if let Some(ref url) = settings.url {
            #[cfg(feature = "websocket")]
            {
                gst::info!(CAT, imp = self, "Connecting ...");

                let authority = url.authority();
                let host = authority
                    .find('@')
                    .map(|idx| authority.split_at(idx + 1).1)
                    .unwrap_or_else(|| authority);
                assert!(!host.is_empty());

                let mut url = url.clone();
                url.query_pairs_mut()
                    .append_pair("rate", &info.rate().to_string())
                    .append_pair(
                        "chunk-duration",
                        &settings.chunk_duration.mseconds().to_string(),
                    )
                    .append_pair("overlap", &settings.overlap.to_string())
                    .append_pair("model-name", &settings.model_name);

                gst::info!(CAT, imp = self, "Connecting to URL {}", url.as_str());

                let req = http::Request::builder()
                    .method(http::Method::GET)
                    .header("Host", host)
                    .header("Connection", "Upgrade")
                    .header("Upgrade", "websocket")
                    .header("Sec-WebSocket-Version", "13")
                    .header(
                        "Sec-WebSocket-Key",
                        async_tungstenite::tungstenite::handshake::client::generate_key(),
                    )
                    .header("Sec-WebSocket-Protocol", "gst-demucs")
                    .uri(url.as_str())
                    .body(())
                    .unwrap();

                drop(state);

                let res = RUNTIME
                    .block_on(async_tungstenite::tokio::connect_async(req))
                    .map_err(|err| {
                        gst::error!(CAT, imp = self, "Failed to connect: {err}");
                        gst::error_msg!(gst::CoreError::Failed, ["Failed to connect: {err}"])
                    });

                state = self.state.lock().unwrap();
                let (ws, _) = match res {
                    Err(err) => {
                        return (state, Err(err));
                    }
                    Ok(res) => res,
                };

                let (ws_sink, mut ws_stream) = ws.split();
                state.ws_sink = Some(Box::pin(ws_sink));

                let this_weak = self.downgrade();
                let read_task = async move {
                    let mut senders = Vec::new();
                    while let Some(this) = this_weak.upgrade() {
                        if this
                            .ws_read_loop_fn(&mut ws_stream, &mut senders)
                            .await
                            .is_break()
                        {
                            break;
                        }
                    }

                    for sender in senders {
                        sender.close();
                    }
                };

                state.read_task_handle = Some(RUNTIME.spawn(read_task));
                state.initialized = true;

                gst::info!(CAT, imp = self, "Connected");
            }
            #[cfg(not(feature = "websocket"))]
            {
                gst::error!(CAT, imp = self, "URL {url} set but no websocket support");
                return (
                    state,
                    Err(gst::error_msg!(
                        gst::CoreError::Failed,
                        ["URL {url} set but no websocket support"]
                    )),
                );
            }
        } else {
            #[cfg(not(feature = "inprocess"))]
            {
                gst::error!(CAT, imp = self, "No URL set");
                return (
                    state,
                    Err(gst::error_msg!(gst::CoreError::Failed, ["No URL set"])),
                );
            }
            #[cfg(feature = "inprocess")]
            {
                use std::sync::Arc;

                gst::debug!(CAT, imp = self, "Creating local instance");

                let rate = info.rate();

                let senders_storage =
                    Arc::new(Mutex::new(Vec::<async_channel::Sender<gst::Buffer>>::new()));

                let res = python::Session::new(
                    &settings.model_name,
                    rate,
                    settings.chunk_duration.mseconds(),
                    settings.overlap,
                    {
                        let this = self.downgrade();
                        let senders_storage = senders_storage.clone();
                        move |b| {
                            let senders = senders_storage.lock().unwrap();
                            assert!(!senders.is_empty());

                            let Some(this) = this.upgrade() else {
                                for sender in &*senders {
                                    sender.close();
                                }
                                return;
                            };

                            if this.demucs_write_fn(&senders, b).is_break() {
                                for sender in &*senders {
                                    sender.close();
                                }
                            }
                        }
                    },
                );

                let session = match res {
                    Ok(session) => session,
                    Err(err) => {
                        gst::error!(CAT, imp = self, "Failed to initialize demucs: {err}");
                        return (
                            state,
                            Err(gst::error_msg!(
                                gst::CoreError::Failed,
                                ["Failed to initialize demucs: {err}"]
                            )),
                        );
                    }
                };

                let latency = gst::ClockTime::SECOND
                    .mul_div_ceil(session.latency_samples(), rate as u64)
                    .unwrap();

                let res = self.create_pads(
                    state,
                    &settings.model_name,
                    session.model_sources(),
                    latency,
                );
                state = res.0;
                let senders = match res.1 {
                    Ok(senders) => senders,
                    Err(err) => {
                        return (state, Err(err));
                    }
                };

                *senders_storage.lock().unwrap() = senders;

                state.in_flight_samples = 0;
                state.max_in_flight_samples = std::cmp::max(
                    std::cmp::max(10 * rate as u64, 3 * session.chunk_samples()),
                    2 * session.latency_samples(),
                );
                state.latency = Some(latency);
                state.demucs = Some(session);
                state.initialized = true;

                gst::debug!(CAT, imp = self, "Created local instance");
            }
        }

        (state, Ok(()))
    }

    fn create_pads<'a>(
        &'a self,
        mut state: MutexGuard<'a, State>,
        model_name: &str,
        model_sources: &[String],
        latency: gst::ClockTime,
    ) -> (
        MutexGuard<'a, State>,
        Result<Vec<async_channel::Sender<gst::Buffer>>, gst::ErrorMessage>,
    ) {
        gst::debug!(
            CAT,
            imp = self,
            "Model {model_name} has sources {model_sources:?} and latency {latency}"
        );

        let mut old_pads = Vec::new();
        let mut srcpads = Vec::new();
        let mut receivers = Vec::new();
        let mut add_pads = false;

        let mut senders = Vec::new();

        state.latency = Some(latency);

        if !state.srcpads.is_empty() {
            if model_sources.len() != state.srcpads.len()
                || !Iterator::zip(model_sources.iter(), state.srcpads.iter())
                    .all(|(s, p)| p.name() == format!("src_{s}"))
            {
                gst::debug!(CAT, imp = self, "Got new model info, replacing source pads");
                old_pads = mem::take(&mut state.srcpads);

                for srcpad in &old_pads {
                    state.flow_combiner.remove_pad(srcpad);
                }
            } else {
                // Just start the pad tasks again after copying over
                // the current sticky events
            }
        }

        // Need to create pads
        if state.srcpads.is_empty() {
            add_pads = true;
            let templ = self.obj().class().pad_template("src_%s").unwrap();
            for source in model_sources {
                let srcpad = gst::Pad::builder_from_template(&templ)
                    .name(format!("src_{source}"))
                    .query_function(|pad, parent, query| {
                        Demucs::catch_panic_pad_function(
                            parent,
                            || false,
                            |demucs| demucs.src_query(pad, query),
                        )
                    })
                    .flags(gst::PadFlags::FIXED_CAPS)
                    .build();

                let _ = srcpad.set_active(true);

                // Just copy over all sticky events
                self.sinkpad.sticky_events_foreach(|event| {
                    if event.type_() == gst::EventType::StreamStart {
                        let pad_name = srcpad.name();
                        let stream_id = pad_name.strip_prefix("src_").unwrap();
                        let stream_id = srcpad.create_stream_id(&*self.obj(), Some(stream_id));

                        let mut event = event.copy();
                        {
                            let event = event.get_mut().unwrap();
                            let s = event.structure_mut();
                            s.set("stream-id", stream_id);
                        }
                        let _ = srcpad.store_sticky_event(&event);
                    } else if event.type_() != gst::EventType::Eos {
                        let _ = srcpad.store_sticky_event(event);
                    }

                    ControlFlow::Continue(gst::EventForeachAction::Keep)
                });

                state.flow_combiner.add_pad(&srcpad);
                srcpads.push(srcpad);
            }
            state.srcpads = srcpads.clone();
        } else {
            srcpads = state.srcpads.clone();
        }

        for _ in model_sources {
            let (sender, receiver) = async_channel::bounded(1);
            senders.push(sender);
            receivers.push(receiver);
        }

        drop(state);

        // Start pad tasks and add pads
        assert_eq!(srcpads.len(), receivers.len());
        for (pad, mut receiver) in Iterator::zip(srcpads.into_iter(), receivers.into_iter()) {
            if add_pads {
                let _ = self.obj().add_pad(&pad);
            }

            let pad_weak = pad.downgrade();
            let res = pad.start_task(move || {
                let Some(pad) = pad_weak.upgrade() else {
                    receiver.close();
                    return;
                };

                let Some(this) = pad
                    .parent()
                    .and_then(|parent| parent.downcast::<super::Demucs>().ok())
                else {
                    receiver.close();
                    let _ = pad.pause_task();
                    return;
                };

                if let Err(err) = this.imp().source_pad_loop_fn(&pad, &mut receiver) {
                    receiver.close();
                    if ![gst::FlowError::Flushing, gst::FlowError::Eos].contains(&err) {
                        gst::element_error!(
                            this,
                            gst::StreamError::Failed,
                            ["Streaming failed: {}", err]
                        );
                    }
                    let _ = pad.pause_task();
                }
            });

            if res.is_err() {
                gst::error!(CAT, imp = self, "Failed to start pad task");
                state = self.state.lock().unwrap();

                return (
                    state,
                    Err(gst::error_msg!(
                        gst::StreamError::Failed,
                        ["Failed to start pads"]
                    )),
                );
            }
        }

        for pad in old_pads {
            let _ = pad.stop_task();
            let _ = pad.set_active(false);
            let _ = self.obj().remove_pad(&pad);
        }

        if add_pads {
            self.obj().no_more_pads();
        }

        gst::debug!(CAT, imp = self, "Added all source pads");

        state = self.state.lock().unwrap();

        (state, Ok(senders))
    }

    fn update_position(
        &self,
        state: &mut State,
        n_bytes: usize,
        n_sources: usize,
    ) -> Result<(gst::ClockTime, gst::ClockTime, usize), ControlFlow<()>> {
        if n_bytes == 0 {
            gst::debug!(CAT, imp = self, "Finished");
            return Err(ControlFlow::Break(()));
        }

        if !n_bytes.is_multiple_of(n_sources) {
            gst::warning!(CAT, imp = self, "Received wrongly sized message");
            return Err(ControlFlow::Continue(()));
        }

        let chunk_size = n_bytes / n_sources;

        if !state.initialized {
            gst::debug!(CAT, imp = self, "Flushed");
            return Err(ControlFlow::Break(()));
        }

        let Some(pts_base) = state.out_position_base else {
            gst::debug!(CAT, imp = self, "Have no output position base");
            return Err(ControlFlow::Break(()));
        };
        let Some(info) = state.info.as_ref() else {
            gst::debug!(CAT, imp = self, "Have no caps, shutting down");
            return Err(ControlFlow::Break(()));
        };

        let n_samples = chunk_size as u64 / info.bpf() as u64;
        let pts_start = pts_base
            + gst::ClockTime::from_nseconds(
                state
                    .out_n_samples
                    .mul_div_ceil(gst::ClockTime::SECOND.nseconds(), info.rate() as u64)
                    .unwrap(),
            );
        let pts_end = pts_base
            + gst::ClockTime::from_nseconds(
                (state.out_n_samples + n_samples)
                    .mul_div_ceil(gst::ClockTime::SECOND.nseconds(), info.rate() as u64)
                    .unwrap(),
            );
        let duration = pts_end - pts_start;
        state.out_n_samples += n_samples;
        state.out_position = Some(pts_end);

        if let Some((in_position, out_position)) =
            Option::zip(state.in_position, state.out_position)
        {
            gst::trace!(
                CAT,
                imp = self,
                "Currently {} of latency at output (in: {in_position}, out: {out_position})",
                in_position.saturating_sub(out_position),
            );
        }

        #[cfg(feature = "inprocess")]
        if state.demucs.is_some() {
            state.in_flight_samples -= n_samples;
        }

        self.state_cond.notify_one();

        Ok((pts_start, duration, chunk_size))
    }

    #[cfg(feature = "inprocess")]
    fn demucs_write_fn(
        &self,
        senders: &[async_channel::Sender<gst::Buffer>],
        mut b: &[u8],
    ) -> ControlFlow<()> {
        let mut state = self.state.lock().unwrap();
        let (pts, duration, chunk_size) =
            match self.update_position(&mut state, b.len(), senders.len()) {
                Ok(res) => res,
                Err(err) => return err,
            };

        let mut futures = Vec::new();
        for sender in senders {
            let (head, tail) = b.split_at(chunk_size);
            let mut buffer = gst::Buffer::from_mut_slice(head.to_vec());

            {
                let buffer = buffer.get_mut().unwrap();
                buffer.set_pts(pts);
                buffer.set_duration(duration);
            }

            futures.push(sender.send(buffer));
            b = tail;
        }

        let (future, abort_handle) = futures::future::abortable(futures::future::join_all(futures));
        state.write_abort_handle = Some(abort_handle);
        drop(state);

        let res = match futures::executor::block_on(future) {
            Ok(res) => {
                let all_disconnected = !res.iter().any(|r| r.is_ok());
                if all_disconnected {
                    gst::debug!(CAT, imp = self, "All source pads stopped");
                }
                if all_disconnected {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            }
            Err(_) => {
                gst::debug!(CAT, imp = self, "Cancelled");
                ControlFlow::Break(())
            }
        };

        state = self.state.lock().unwrap();
        state.write_abort_handle = None;

        res
    }

    #[cfg(feature = "websocket")]
    async fn ws_read_loop_fn(
        &self,
        ws_stream: &mut (
                 impl futures::Stream<
            Item = Result<
                async_tungstenite::tungstenite::Message,
                async_tungstenite::tungstenite::Error,
            >,
        > + Unpin
             ),
        senders: &mut Vec<async_channel::Sender<gst::Buffer>>,
    ) -> ControlFlow<(), ()> {
        use futures::prelude::*;

        let Some(msg) = ws_stream.next().await else {
            gst::info!(CAT, imp = self, "WebSocket connection closed");
            return ControlFlow::Break(());
        };

        let msg = match msg {
            Ok(msg) => msg,
            Err(err) => {
                gst::error!(CAT, imp = self, "Failed to receive data: {err}");
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Failed,
                    ["Streaming failed: {err}"]
                );
                return ControlFlow::Break(());
            }
        };

        use async_tungstenite::tungstenite::Message;

        match msg {
            Message::Text(s) => {
                #[derive(Debug, serde::Deserialize)]
                #[serde(rename_all = "snake_case")]
                enum DemucsMessage {
                    Error(String),
                    ModelInfo {
                        model_name: String,
                        sources: Vec<String>,
                        latency: u32,
                    },
                }

                let msg = match serde_json::from_str::<DemucsMessage>(s.as_str()) {
                    Ok(msg) => msg,
                    Err(err) => {
                        gst::warning!(CAT, imp = self, "Failed deserializing message: {err}");
                        return ControlFlow::Continue(());
                    }
                };

                gst::debug!(CAT, imp = self, "Received server message: {msg:?}");

                match msg {
                    DemucsMessage::Error(s) => {
                        gst::error!(CAT, imp = self, "Got error: {s}");
                        gst::element_imp_error!(
                            self,
                            gst::StreamError::Failed,
                            ["Server error: {s}"]
                        );
                        return ControlFlow::Break(());
                    }
                    DemucsMessage::ModelInfo {
                        model_name,
                        sources: model_sources,
                        latency,
                    } => {
                        let latency = gst::ClockTime::from_mseconds(latency as u64);

                        // Create pads and start pad tasks as needed
                        let res = self
                            .obj()
                            .call_async_future(move |this| {
                                let imp = this.imp();

                                let state = imp.state.lock().unwrap();
                                let (state, res) =
                                    imp.create_pads(state, &model_name, &model_sources, latency);
                                drop(state);

                                match res {
                                    Err(err) => {
                                        imp.post_error_message(err);
                                        ControlFlow::Break(())
                                    }
                                    Ok(senders) => ControlFlow::Continue(senders),
                                }
                            })
                            .await;

                        match res {
                            ControlFlow::Continue(new_senders) => {
                                *senders = new_senders;
                            }
                            ControlFlow::Break(_) => return ControlFlow::Break(()),
                        }
                    }
                }
            }
            Message::Binary(mut b) => {
                gst::debug!(CAT, imp = self, "Received message of {} bytes", b.len());

                let (pts, duration, chunk_size) = {
                    let mut state = self.state.lock().unwrap();

                    let (pts, duration, chunk_size) =
                        match self.update_position(&mut state, b.len(), senders.len()) {
                            Ok(res) => res,
                            Err(err) => return err,
                        };

                    (pts, duration, chunk_size)
                };

                let mut futures = Vec::new();
                for sender in senders {
                    let mut buffer = gst::Buffer::from_slice(b.split_to(chunk_size));

                    {
                        let buffer = buffer.get_mut().unwrap();
                        buffer.set_pts(pts);
                        buffer.set_duration(duration);
                    }

                    futures.push(sender.send(buffer));
                }

                let res = futures::future::join_all(futures).await;

                let all_disconnected = !res.iter().any(|r| r.is_ok());
                if all_disconnected {
                    gst::debug!(CAT, imp = self, "All source pads stopped");
                    return ControlFlow::Break(());
                }
            }
            Message::Close(_) => {
                gst::debug!(CAT, imp = self, "Connection closed");
                return ControlFlow::Break(());
            }
            _ => {}
        }

        ControlFlow::Continue(())
    }

    fn source_pad_loop_fn(
        &self,
        pad: &gst::Pad,
        receiver: &mut async_channel::Receiver<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let res = receiver.recv_blocking();

        if let Ok(buffer) = res {
            gst::trace!(CAT, obj = pad, "Forwarding buffer {buffer:?}");
            let res = pad.push(buffer);
            gst::trace!(CAT, obj = pad, "Pad returned {res:?}");

            let mut state = self.state.lock().unwrap();
            let combined_res = state.flow_combiner.update_pad_flow(pad, res);
            state.last_flow = combined_res;
            drop(state);

            combined_res
        } else {
            gst::debug!(CAT, obj = pad, "End of stream");
            let mut state = self.state.lock().unwrap();
            if state.draining {
                gst::debug!(CAT, obj = pad, "Draining finished");
                // drain() waits until the task is shut down, which happens
                // here right after returning.
                self.state_cond.notify_one();
            } else {
                let combined_res = state
                    .flow_combiner
                    .update_pad_flow(pad, Err(gst::FlowError::Eos));
                state.last_flow = combined_res;
                let seqnum = state.segment_seqnum;
                drop(state);

                let _ = pad.push_event(gst::event::Eos::builder().seqnum_if_some(seqnum).build());
            }
            Err(gst::FlowError::Eos)
        }
    }

    fn drain<'a>(&'a self, mut state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        if !state.initialized {
            return state;
        }

        state.draining = true;
        gst::debug!(CAT, imp = self, "Draining");
        let srcpads = state.srcpads.clone();

        let mut pads_stopped = false;

        let (mut state, res) = self.handle_buffer(state, None);
        if res.is_ok() {
            // Wait for source pads to drain
            while !srcpads
                .iter()
                .all(|pad| pad.task_state() != gst::TaskState::Started)
                && state.draining
                && state.initialized
            {
                state = self.state_cond.wait(state).unwrap();
            }
            drop(state);
            pads_stopped = true;
            for pad in &srcpads {
                let _ = pad.stop_task();
            }
            state = self.state.lock().unwrap();
        }

        state = self.deinit_demucs(state);

        if !pads_stopped {
            drop(state);
            for pad in srcpads {
                let _ = pad.stop_task();
            }
            state = self.state.lock().unwrap();
        }

        state.draining = false;
        state.out_position_base = None;
        state.out_n_samples = 0;
        state.out_position = None;
        state
    }

    #[allow(dropping_copy_types)]
    fn deinit_demucs<'a>(&'a self, mut state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        gst::info!(CAT, imp = self, "Deinitializing");

        self.state_cond.notify_one();

        let inprocess_drop;
        let websocket_drop;

        #[cfg(feature = "inprocess")]
        {
            if let Some(write_abort_handle) = state.write_abort_handle.take() {
                write_abort_handle.abort();
            }

            struct D {
                demucs: Option<python::Session>,
            }
            impl Drop for D {
                fn drop(&mut self) {
                    if let Some(demucs) = self.demucs.take() {
                        let _ = demucs.cancel();
                    }
                }
            }

            let demucs = state.demucs.take();

            inprocess_drop = D { demucs };
        }
        #[cfg(not(feature = "inprocess"))]
        {
            inprocess_drop = ();
        }

        #[cfg(feature = "websocket")]
        {
            if let Some(abort_handle) = state.read_task_handle.take() {
                abort_handle.abort();
            }

            if let Some(abort_handle) = state.send_task_handle.take() {
                abort_handle.abort();
            }

            struct D {
                ws_sink: Option<WsSink>,
            }
            impl Drop for D {
                fn drop(&mut self) {
                    if let Some(mut ws_sink) = self.ws_sink.take() {
                        RUNTIME.block_on(async {
                            use futures::prelude::*;

                            let _ = ws_sink.close().await;
                        });
                    }
                }
            }

            let ws_sink = state.ws_sink.take();
            websocket_drop = D { ws_sink }
        }
        #[cfg(not(feature = "websocket"))]
        {
            websocket_drop = ();
        }

        state.initialized = false;
        drop(state);

        let _ = self.sinkpad.stream_lock();

        drop(inprocess_drop);
        drop(websocket_drop);

        gst::info!(CAT, imp = self, "Deinitialized");

        self.state.lock().unwrap()
    }

    fn handle_buffer<'a>(
        &'a self,
        mut state: MutexGuard<'a, State>,
        buffer: Option<gst::MappedBuffer<gst::buffer::Readable>>,
    ) -> (
        MutexGuard<'a, State>,
        Result<gst::FlowSuccess, gst::FlowError>,
    ) {
        if !state.initialized {
            return (state, Err(gst::FlowError::Flushing));
        }

        if let Some(ref buffer) = buffer {
            state.in_position = buffer.buffer().pts();
            if state.out_position_base.is_none() {
                state.out_position_base = buffer.buffer().pts();
            }

            if let Some((in_position, out_position)) =
                Option::zip(state.in_position, state.out_position)
            {
                gst::trace!(
                    CAT,
                    imp = self,
                    "Currently {} of latency at input (in: {in_position}, out: {out_position})",
                    in_position.saturating_sub(out_position),
                );
            }
            gst::trace!(CAT, imp = self, "Writing buffer of {} bytes", buffer.len());
        } else {
            gst::trace!(CAT, imp = self, "Finishing");
        }

        #[cfg(feature = "inprocess")]
        {
            if state.demucs.is_some() {
                let info = state.info.as_ref().unwrap();
                let n_samples =
                    buffer.as_ref().map(|b| b.len() as u64).unwrap_or(0) / info.bpf() as u64;
                state.in_flight_samples += n_samples;
            }
        }

        #[cfg(feature = "inprocess")]
        if let Some(ref demucs) = state.demucs {
            let data = buffer.as_deref().unwrap_or(&[]);

            if let Err(err) = demucs.push_data(data) {
                gst::error!(CAT, imp = self, "Failed writing audio buffer: {err}");
                return (state, Err(gst::FlowError::Error));
            }

            // Wait for output if too many samples are in flight right now
            while state.in_flight_samples >= state.max_in_flight_samples && state.demucs.is_some() {
                gst::trace!(
                    CAT,
                    imp = self,
                    "{} samples in flight -- waiting",
                    state.in_flight_samples
                );
                state = self.state_cond.wait(state).unwrap();
            }

            let res = state.last_flow;
            return (state, res);
        }

        #[cfg(feature = "websocket")]
        {
            use async_tungstenite::tungstenite::Bytes;
            use futures::prelude::*;

            let Some(mut ws_sink) = state.ws_sink.take() else {
                return (state, Err(gst::FlowError::Flushing));
            };

            let join_handle = if let Some(buffer) = buffer {
                RUNTIME.spawn(async move {
                    let message =
                        async_tungstenite::tungstenite::Message::Binary(Bytes::from_owner(buffer));
                    let ret = ws_sink.send(message).await;
                    (ret, ws_sink)
                })
            } else {
                RUNTIME.spawn(async move {
                    let message =
                        async_tungstenite::tungstenite::Message::Binary(Bytes::from_static(&[]));
                    let ret = ws_sink.send(message).await;
                    (ret, ws_sink)
                })
            };
            state.send_task_handle = Some(join_handle.abort_handle());
            drop(state);

            let ret = RUNTIME.block_on(join_handle);

            state = self.state.lock().unwrap();
            state.send_task_handle = None;
            match ret {
                Ok((ret, ws_sink)) => {
                    state.ws_sink = Some(ws_sink);
                    if let Err(err) = ret {
                        gst::error!(CAT, imp = self, "Failed writing audio buffer: {err}");
                        return (state, Err(gst::FlowError::Error));
                    }
                }
                Err(err) if err.is_cancelled() => {
                    gst::debug!(CAT, imp = self, "Cancelled writing");
                    return (state, Err(gst::FlowError::Flushing));
                }
                Err(err) => {
                    gst::error!(CAT, imp = self, "Failed joining write task: {err}");
                    return (state, Err(gst::FlowError::Error));
                }
            }
        }

        let res = state.last_flow;
        (state, res)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Demucs {
    const NAME: &'static str = "GstDemucs";
    type Type = super::Demucs;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Demucs::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |demucs| demucs.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Demucs::catch_panic_pad_function(
                    parent,
                    || false,
                    |demucs| demucs.sink_event(pad, event),
                )
            })
            .build();

        Self {
            sinkpad,
            state: Mutex::new(State::default()),
            state_cond: Condvar::new(),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for Demucs {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("url")
                    .nick("URL")
                    .blurb("URL for the demucs service, NULL runs it locally")
                    .default_value(Settings::default().url.as_ref().map(|url| url.as_str()))
                    .build(),
                glib::ParamSpecUInt64::builder("chunk-duration")
                    .nick("Chunk Duration")
                    .blurb("Duration of each chunk passed to demucs")
                    .default_value(Settings::default().chunk_duration.nseconds())
                    .build(),
                glib::ParamSpecFloat::builder("overlap")
                    .nick("Overlap between chunks")
                    .blurb("Overlap between chunks passed to demucs")
                    .default_value(Settings::default().overlap)
                    .build(),
                glib::ParamSpecUInt64::builder("processing-latency")
                    .nick("Processing Latency")
                    .blurb("Latency introduced by the service and network for one chunk")
                    .default_value(Settings::default().processing_latency.nseconds())
                    .build(),
                glib::ParamSpecString::builder("model-name")
                    .nick("Model Name")
                    .blurb("Demucs model name")
                    .default_value(Settings::default().model_name.as_str())
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "url" => {
                let mut settings = self.settings.lock().unwrap();
                let url = value.get().unwrap();

                if let Some(url) = url {
                    let url = match url::Url::parse(url) {
                        Ok(url) => url,
                        Err(err) => {
                            gst::error!(CAT, imp = self, "Failed to parse URL '{url}': {err}");
                            return;
                        }
                    };
                    settings.url = Some(url);
                } else {
                    settings.url = None;
                }
            }
            "chunk-duration" => {
                let mut settings = self.settings.lock().unwrap();
                settings.chunk_duration = value.get().unwrap();
            }
            "overlap" => {
                let mut settings = self.settings.lock().unwrap();
                settings.overlap = value.get().unwrap();
            }
            "processing-latency" => {
                let mut settings = self.settings.lock().unwrap();
                settings.processing_latency = value.get().unwrap();
            }
            "model-name" => {
                let mut settings = self.settings.lock().unwrap();
                settings.model_name = value.get().unwrap();
            }
            _ => unreachable!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "url" => {
                let settings = self.settings.lock().unwrap();
                settings.url.as_ref().map(|url| url.as_str()).to_value()
            }
            "chunk-duration" => {
                let settings = self.settings.lock().unwrap();
                settings.chunk_duration.to_value()
            }
            "overlap" => {
                let settings = self.settings.lock().unwrap();
                settings.overlap.to_value()
            }
            "processing-latency" => {
                let settings = self.settings.lock().unwrap();
                settings.processing_latency.to_value()
            }
            "model-name" => {
                let settings = self.settings.lock().unwrap();
                settings.model_name.to_value()
            }
            _ => unreachable!(),
        }
    }
}

impl GstObjectImpl for Demucs {}

impl ElementImpl for Demucs {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Demucs",
                "Filter/Audio",
                "Demucs Music Source Separation",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_audio::AudioCapsBuilder::new_interleaved()
                .format(gst_audio::AudioFormat::F32le)
                .rate_range(32_000..=48_000)
                .channels(2)
                .build();

            let src_pad_template = gst::PadTemplate::new(
                "src_%s",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &caps,
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                #[cfg(feature = "inprocess")]
                {
                    let load_python = self.settings.lock().unwrap().url.is_none();
                    if load_python && let Err(err) = python::init() {
                        gst::error!(CAT, imp = self, "Failed to load Python module: {err}");
                        return Err(gst::StateChangeError);
                    }
                }
            }
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.lock().unwrap();
                state.flow_combiner.reset();
                state.last_flow = Ok(gst::FlowSuccess::Ok);
                state.info = None;
                state.segment = None;
                state.segment_seqnum = None;
                state.in_position = None;
                state.out_position = None;
                state.out_position_base = None;
                state.out_n_samples = 0;
                state.draining = false;
            }
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                state.last_flow = Err(gst::FlowError::Flushing);
                state = self.deinit_demucs(state);
                drop(state);
            }
            _ => (),
        }

        let res = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                state.flow_combiner.reset();
                state.last_flow = Err(gst::FlowError::Flushing);
                state.info = None;
                state.segment = None;
                state.segment_seqnum = None;
                state.in_position = None;
                state.out_position = None;
                state.out_position_base = None;
                state.out_n_samples = 0;
                state.draining = false;
                let srcpads = mem::take(&mut state.srcpads);
                drop(state);

                for pad in srcpads {
                    let _ = pad.stop_task();
                    let _ = pad.set_active(false);
                    let _ = self.obj().remove_pad(&pad);
                }
            }
            _ => (),
        }

        Ok(res)
    }
}

#[cfg(feature = "inprocess")]
mod python {
    use pyo3::{ffi::c_str, prelude::*};

    static SESSION: &std::ffi::CStr = pyo3::ffi::c_str!(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/service/session.py"
    )));

    pub fn init() -> Result<(), anyhow::Error> {
        use std::sync::OnceLock;

        static LIBPYTHON: OnceLock<bool> = OnceLock::new();

        let have_python = LIBPYTHON.get_or_init(|| {
            // Make sure to load libpython with RTLD_GLOBAL or else Python will fail loading various
            // modules because of unresolved symbols. GStreamer plugins are loaded with RTLD_LOCAL.

            #[cfg(not(windows))]
            unsafe {
                use std::{mem, ptr};

                let handle = libc::dlopen(ptr::null_mut(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
                if !libc::dlsym(handle, c_str!("_Py_NoneStruct").as_ptr()).is_null() {
                    gst::debug!(super::CAT, "libpython already loaded");
                    return true;
                }

                let mut info = mem::zeroed();
                let res = libc::dladdr(pyo3::ffi::Py_IsInitialized as *const _, &mut info);
                if res == 0 {
                    gst::warning!(super::CAT, "Failed to find libpython");
                    return false;
                }

                gst::debug!(
                    super::CAT,
                    "Loading libpython from {:?}",
                    std::ffi::CStr::from_ptr(info.dli_fname)
                );
                let handle = libc::dlopen(info.dli_fname, libc::RTLD_NOW | libc::RTLD_GLOBAL);
                if handle.is_null() {
                    gst::warning!(super::CAT, "Failed to load libpython");
                    return false;
                }

                true
            }

            // Windows doesn't have such complications
            #[cfg(windows)]
            {
                true
            }
        });

        if !*have_python {
            gst::warning!(
                super::CAT,
                "Can't find libpython, this will probably not work"
            );
        }

        let model_device = std::env::var("GST_DEMUCS_MODULE_DEVICE").unwrap_or(String::from("cpu"));
        let model_repo = std::env::var("GST_DEMUCS_MODULE_REPO").ok();

        Python::attach(|py| -> PyResult<()> {
            let module = PyModule::from_code(py, SESSION, c_str!("session.py"), c_str!("session"))?;
            let fun = module.getattr("init")?;
            fun.call1((&model_device, model_repo.as_ref()))?;
            Ok(())
        })?;

        Ok(())
    }

    pub struct Session {
        obj: Py<PyAny>,
        latency_samples: u64,
        chunk_samples: u64,
        model_sources: Vec<String>,
    }

    impl Session {
        pub fn new(
            model_name: &str,
            rate: u32,
            chunk_duration: u64,
            overlap: f32,
            output: impl Fn(&[u8]) + Send + Sync + 'static,
        ) -> Result<Session, anyhow::Error> {
            #[pyclass]
            #[allow(clippy::type_complexity)]
            struct Fun(Box<dyn Fn(&[u8]) + Send + Sync + 'static>);

            #[pymethods]
            impl Fun {
                fn __call__(&self, py: Python<'_>, bytes: &Bound<'_, pyo3::types::PyBytes>) {
                    let bytes = bytes.as_bytes();
                    py.detach(move || {
                        (self.0)(bytes);
                    });
                }
            }

            Python::attach(|py| -> Result<_, anyhow::Error> {
                let module =
                    PyModule::from_code(py, SESSION, c_str!("session.py"), c_str!("session"))?;
                let session_class = module.getattr("Session")?;
                let obj = session_class.call1((
                    model_name,
                    rate,
                    chunk_duration,
                    overlap,
                    Fun(Box::new(output)),
                ))?;

                let latency_samples = obj.getattr("latency_samples")?.extract::<u64>()?;
                let chunk_samples = obj.getattr("chunk_samples")?.extract::<u64>()?;
                let model_sources = obj.getattr("model_sources")?.extract::<Vec<String>>()?;

                Ok(Session {
                    obj: obj.into(),
                    latency_samples,
                    chunk_samples,
                    model_sources,
                })
            })
        }

        pub fn cancel(&self) -> Result<(), anyhow::Error> {
            Python::attach(|py| -> PyResult<()> {
                self.obj.call_method1(py, "cancel", (true,))?;
                Ok(())
            })?;

            Ok(())
        }

        pub fn push_data(&self, data: &[u8]) -> Result<(), anyhow::Error> {
            Python::attach(|py| -> PyResult<()> {
                self.obj.call_method1(py, "push_data", (data,))?;
                Ok(())
            })?;

            Ok(())
        }

        pub fn latency_samples(&self) -> u64 {
            self.latency_samples
        }

        pub fn chunk_samples(&self) -> u64 {
            self.chunk_samples
        }

        pub fn model_sources(&self) -> &[String] {
            &self.model_sources
        }
    }
}
