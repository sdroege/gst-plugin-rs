// Copyright (C) 2024 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::subclass::prelude::*;
use gst::{glib, prelude::*};

use std::default::Default;

use async_tungstenite::tungstenite::error::Error as WsError;
use async_tungstenite::{tokio::connect_async, tungstenite::Message};
use futures::channel::mpsc;
use futures::future::{abortable, AbortHandle};
use futures::prelude::*;
use http::Request;
use tokio::runtime;
use url::Url;

use std::collections::{BTreeSet, VecDeque};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use atomic_refcell::AtomicRefCell;

use std::sync::LazyLock;

#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct TranscriptMetadata {
    start_time: f32,
    end_time: f32,
    transcript: String,
}

#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct TranscriptDisplay {
    direction: String,
}

#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct TranscriptAlternative {
    content: String,
    confidence: f32,
    display: Option<TranscriptDisplay>,
    language: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct TranscriptResult {
    #[serde(rename = "type")]
    type_: String,
    start_time: f32,
    end_time: f32,
    #[serde(default)]
    is_eos: bool,
    #[serde(default)]
    alternatives: Vec<TranscriptAlternative>,
}

#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct Transcript {
    metadata: TranscriptMetadata,
    results: Vec<TranscriptResult>,
}

#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct TranslationResult {
    start_time: f32,
    end_time: f32,
    content: String,
}

#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct Translation {
    language: String,
    results: Vec<TranslationResult>,
}

#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct TranscriptError {
    code: Option<i32>,
    #[serde(rename = "type")]
    type_: String,
    reason: String,
}

#[derive(serde::Serialize, Debug)]
struct AudioType {
    #[serde(rename = "type")]
    type_: String,
    encoding: String,
    sample_rate: u32,
}

#[derive(serde::Serialize, Debug, Clone)]
struct Vocable {
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sounds_like: Vec<String>,
}

#[derive(serde::Serialize, Debug)]
struct TranscriptionConfig {
    language: String,
    enable_partials: bool,
    max_delay: f32,
    additional_vocab: Vec<Vocable>,
}

#[derive(serde::Serialize, Debug)]
struct TranslationConfig {
    target_languages: Vec<String>,
    enable_partials: bool,
}

#[derive(serde::Serialize, Debug)]
struct StartRecognition {
    message: String,
    audio_format: AudioType,
    transcription_config: TranscriptionConfig,
    translation_config: TranslationConfig,
}

#[derive(serde::Serialize, Debug)]
struct EndOfStream {
    message: String,
    last_seq_no: u64,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "speechmaticstranscribe",
        gst::DebugColorFlags::empty(),
        Some("Speechmatics Transcribe element"),
    )
});

static RUNTIME: LazyLock<runtime::Runtime> = LazyLock::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

const DEFAULT_LATENCY_MS: u32 = 8000;
const DEFAULT_LATENESS_MS: u32 = 0;
const GRANULARITY_MS: u32 = 100;

#[derive(Debug, Clone)]
struct Settings {
    latency_ms: u32,
    lateness_ms: u32,
    language_code: Option<String>,
    url: Option<String>,
    api_key: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            latency_ms: DEFAULT_LATENCY_MS,
            lateness_ms: DEFAULT_LATENESS_MS,
            language_code: Some("en".to_string()),
            url: Some("ws://0.0.0.0:9000".to_string()),
            api_key: None,
        }
    }
}

#[derive(Debug)]
struct ItemAccumulator {
    text: String,
    start_time: gst::ClockTime,
    end_time: gst::ClockTime,
}

impl From<ItemAccumulator> for gst::Buffer {
    fn from(acc: ItemAccumulator) -> Self {
        let mut buf = gst::Buffer::from_mut_slice(acc.text.into_bytes());

        {
            let buf = buf.get_mut().unwrap();
            buf.set_pts(acc.start_time);
            buf.set_duration(acc.end_time - acc.start_time);
        }

        buf
    }
}

struct State {
    connected: bool,
    recv_abort_handle: Option<AbortHandle>,
    send_abort_handle: Option<AbortHandle>,
    in_segment: gst::FormattedSegment<gst::ClockTime>,
    seq_no: u64,
    additional_vocabulary: Vec<Vocable>,
    discont_offset: gst::ClockTime,
    last_chained_buffer_rtime: Option<gst::ClockTime>,
    pad_serial: u32,
    srcpads: BTreeSet<super::TranscriberSrcPad>,
    start_time: Option<gst::ClockTime>,
}

impl State {}

impl Default for State {
    fn default() -> Self {
        Self {
            connected: false,
            recv_abort_handle: None,
            send_abort_handle: None,
            in_segment: gst::FormattedSegment::new(),
            seq_no: 0,
            additional_vocabulary: vec![],
            discont_offset: gst::ClockTime::ZERO,
            last_chained_buffer_rtime: gst::ClockTime::NONE,
            pad_serial: 0,
            srcpads: BTreeSet::new(),
            start_time: None,
        }
    }
}

type WsSink = Pin<Box<dyn Sink<Message, Error = WsError> + Send + Sync>>;

pub struct Transcriber {
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    ws_sink: AtomicRefCell<Option<WsSink>>,
}

impl TranscriberSrcPad {
    fn dequeue(&self) -> bool {
        let Some(parent) = self.obj().parent() else {
            return true;
        };

        let transcriber = parent
            .downcast::<super::Transcriber>()
            .expect("parent is transcriber");

        let Some((start_time, now)) = transcriber.imp().get_start_time_and_now() else {
            // Wait for the clock to be available
            return true;
        };

        let latency = gst::ClockTime::from_mseconds(
            transcriber.imp().settings.lock().unwrap().latency_ms as u64,
        );

        /* First, check our pending buffers */
        let mut items = vec![];

        let granularity = gst::ClockTime::from_mseconds(GRANULARITY_MS as u64);

        let (latency, now, mut last_position, send_eos, seqnum) = {
            let mut state = self.state.lock().unwrap();

            if let Some(ref mut accumulator_inner) = state.accumulator {
                if now.saturating_sub(accumulator_inner.start_time + start_time) + granularity
                    > latency
                {
                    gst::log!(CAT, "Finally draining accumulator");
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Item is ready: \"{:?}\", start_time: {}, end_time: {}",
                        accumulator_inner.text,
                        accumulator_inner.start_time,
                        accumulator_inner.end_time
                    );
                    let buf = state.accumulator.take().unwrap().into();
                    state.push_buffer(buf);
                }
            }

            let send_eos =
                state.send_eos && state.buffers.is_empty() && state.accumulator.is_none();

            while let Some(buf) = state.buffers.front() {
                if now.saturating_sub(buf.pts().unwrap() + start_time) + granularity > latency {
                    /* Safe unwrap, we know we have an item */
                    let mut buf = state.buffers.pop_front().unwrap();
                    {
                        let buf_mut = buf.make_mut();
                        let mut pts = buf_mut.pts().unwrap() + start_time;
                        let mut duration = buf_mut.duration().unwrap();
                        if let Some(position) = state.out_segment.position() {
                            if pts < position {
                                gst::debug!(
                                    CAT,
                                    imp = self,
                                    "Adjusting item timing({:?} < {:?})",
                                    pts,
                                    position,
                                );
                                duration = duration.saturating_sub(position - pts);
                                pts = position;
                            }
                        }

                        buf_mut.set_pts(pts);
                        buf_mut.set_duration(duration);
                    }
                    items.push(buf);
                } else {
                    break;
                }
            }

            (
                latency,
                now,
                state.out_segment.position(),
                send_eos,
                state.seqnum,
            )
        };

        /* We're EOS, we can pause and exit early */
        if send_eos {
            let _ = self.pause_task();

            return self
                .obj()
                .push_event(gst::event::Eos::builder().seqnum(seqnum).build());
        }

        for buf in items.drain(..) {
            let pts = buf.pts().unwrap();

            if let Some(last_position) = last_position {
                if pts > last_position {
                    let gap_event = gst::event::Gap::builder(last_position)
                        .duration(pts - last_position)
                        .seqnum(seqnum)
                        .build();
                    gst::log!(CAT, "Pushing gap:    {} -> {}", last_position, pts);
                    if !self.obj().push_event(gap_event) {
                        return false;
                    }
                }
            }

            let pts_end = if let Some(duration) = buf.duration() {
                pts + duration
            } else {
                pts
            };
            last_position = Some(pts_end);

            gst::debug!(CAT, imp = self, "Pushing buffer: {} -> {}", pts, pts_end,);

            if self.obj().push(buf).is_err() {
                return false;
            }
        }

        /* next, push a gap if we're lagging behind the target position */

        if let Some(last_position_) = last_position {
            if now >= last_position_ && now - last_position_ + granularity > latency {
                // Invent caps/segment events if none were produced yet
                let mut events = vec![];

                let state_guard = self.state.lock().unwrap();
                if !self.obj().has_current_caps() {
                    let caps = gst::Caps::builder("text/x-raw")
                        .field("format", "utf8")
                        .build();
                    events.push(
                        gst::event::Caps::builder(&caps)
                            .seqnum(state_guard.seqnum)
                            .build(),
                    );
                }

                if self.obj().sticky_event::<gst::event::Segment>(0).is_none() {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Constructing segment event from {:?}",
                        state_guard.out_segment
                    );
                    events.push(
                        gst::event::Segment::builder(&state_guard.out_segment)
                            .seqnum(state_guard.seqnum)
                            .build(),
                    );
                }

                drop(state_guard);

                for event in events {
                    gst::debug!(CAT, imp = self, "Pushing event {event:?}");
                    self.obj().push_event(event);
                }

                let duration = now - last_position_ + granularity - latency;

                let gap_event = gst::event::Gap::builder(last_position_)
                    .duration(duration)
                    .seqnum(seqnum)
                    .build();
                gst::log!(
                    CAT,
                    "Pushing gap:    {} -> {}",
                    last_position_,
                    last_position_ + duration
                );
                last_position = Some(last_position_ + duration);
                if !self.obj().push_event(gap_event) {
                    return false;
                }
            }

            self.state
                .lock()
                .unwrap()
                .out_segment
                .set_position(last_position);
        }

        true
    }

    fn enqueue_translation(&self, state: &mut TranscriberSrcPadState, translation: &Translation) {
        gst::log!(CAT, "Enqueuing {:?}", translation);
        for item in &translation.results {
            let start_time =
                gst::ClockTime::from_nseconds((item.start_time as f64 * 1_000_000_000.0) as u64);
            let end_time =
                gst::ClockTime::from_nseconds((item.end_time as f64 * 1_000_000_000.0) as u64);

            let mut buf = gst::Buffer::from_mut_slice(item.content.clone().into_bytes());

            {
                let buf = buf.get_mut().unwrap();
                buf.set_pts(start_time);
                buf.set_duration(end_time - start_time);
            }

            state.push_buffer(buf);
        }
    }

    fn enqueue_transcript(&self, state: &mut TranscriberSrcPadState, transcript: &Transcript) {
        gst::log!(CAT, "Enqueuing {:?}", transcript);
        for item in &transcript.results {
            if let Some(alternative) = item.alternatives.first() {
                let start_time = gst::ClockTime::from_nseconds(
                    (item.start_time as f64 * 1_000_000_000.0) as u64,
                );
                let end_time =
                    gst::ClockTime::from_nseconds((item.end_time as f64 * 1_000_000_000.0) as u64);

                if let Some(ref mut accumulator_inner) = state.accumulator {
                    if item.type_ == "punctuation" {
                        accumulator_inner.text.push_str(&alternative.content);
                        accumulator_inner.end_time = end_time;
                    } else {
                        gst::debug!(
                            CAT,
                            imp = self,
                            "Item is ready: \"{}\", start_time: {}, end_time: {}",
                            accumulator_inner.text,
                            accumulator_inner.start_time,
                            accumulator_inner.end_time
                        );

                        let buffer = state.accumulator.take().unwrap().into();

                        state.push_buffer(buffer);
                        state.accumulator = Some(ItemAccumulator {
                            text: alternative.content.clone(),
                            start_time,
                            end_time,
                        });
                    }
                } else {
                    state.accumulator = Some(ItemAccumulator {
                        text: alternative.content.clone(),
                        start_time,
                        end_time,
                    });
                }
            }
        }
    }

    fn loop_fn(&self, receiver: &mut mpsc::Receiver<Message>) -> Result<(), gst::ErrorMessage> {
        let future = async move {
            let msg = match receiver.next().await {
                Some(msg) => msg,
                /* Sender was closed */
                None => {
                    let _ = self.pause_task();
                    return Ok(());
                }
            };

            let language_code = self.settings.lock().unwrap().language_code.clone();

            match msg {
                Message::Text(text) => {
                    let mut json: serde_json::Value = serde_json::from_str(&text).unwrap();
                    let message_type = {
                        let obj = json.as_object_mut().expect("object");
                        let Some(message_type) = obj.remove("message") else {
                            return Err(gst::error_msg!(
                                gst::StreamError::Failed,
                                ["Missing message field in object: {}", text]
                            ));
                        };
                        if let serde_json::Value::String(s) = message_type {
                            s
                        } else {
                            panic!("message field not a string");
                        }
                    };

                    match message_type.as_str() {
                        "AddTranslation" => {
                            let Some(parent) = self.obj().parent() else {
                                return Ok(());
                            };

                            let transcriber = parent
                                .downcast::<super::Transcriber>()
                                .expect("parent is transcriber");

                            let Some(language_code) = language_code else {
                                return Ok(());
                            };

                            let mut translation: Translation = serde_json::from_value(json)
                                .map_err(|err| {
                                    gst::error_msg!(
                                        gst::StreamError::Failed,
                                        ["Unexpected message: {} ({})", text, err]
                                    )
                                })?;

                            if translation.language != language_code {
                                return Ok(());
                            }

                            gst::info!(CAT, imp = self, "Parsed translation {:?}", translation);

                            let lateness = (transcriber.imp().settings.lock().unwrap().lateness_ms
                                as f64
                                / 1_000.) as f32;
                            let discont_offset =
                                (transcriber
                                    .imp()
                                    .state
                                    .lock()
                                    .unwrap()
                                    .discont_offset
                                    .nseconds() as f64
                                    / 1_000_000_000.0) as f32;

                            gst::info!(
                                CAT,
                                imp = self,
                                "Introducing {} lateness and adding discont offset {}",
                                lateness,
                                discont_offset
                            );

                            for item in translation.results.iter_mut() {
                                item.start_time += lateness + discont_offset;
                                item.end_time += lateness + discont_offset;
                            }

                            if !translation.results.is_empty() {
                                let mut state = self.state.lock().unwrap();
                                self.enqueue_translation(&mut state, &translation);
                            }
                        }
                        "AddTranscript" | "AddPartialTranscript" => {
                            /* This pad outputs translations */
                            if language_code.is_some() {
                                return Ok(());
                            }
                            let Some(parent) = self.obj().parent() else {
                                return Ok(());
                            };

                            let transcriber = parent
                                .downcast::<super::Transcriber>()
                                .expect("parent is transcriber");

                            let is_partial = message_type == "AddPartialTranscript";
                            let mut transcript: Transcript =
                                serde_json::from_value(json).map_err(|err| {
                                    gst::error_msg!(
                                        gst::StreamError::Failed,
                                        ["Unexpected message: {} ({})", text, err]
                                    )
                                })?;

                            gst::info!(
                                CAT,
                                imp = self,
                                "Parsed {} transcript {:?}",
                                if is_partial { "partial" } else { "final" },
                                transcript
                            );

                            let lateness = (transcriber.imp().settings.lock().unwrap().lateness_ms
                                as f64
                                / 1_000.) as f32;
                            let discont_offset =
                                (transcriber
                                    .imp()
                                    .state
                                    .lock()
                                    .unwrap()
                                    .discont_offset
                                    .nseconds() as f64
                                    / 1_000_000_000.0) as f32;

                            gst::info!(
                                CAT,
                                imp = self,
                                "Introducing {} lateness and adding discont offset {}",
                                lateness,
                                discont_offset
                            );

                            transcript.metadata.start_time += lateness + discont_offset;
                            transcript.metadata.end_time += lateness + discont_offset;

                            for item in transcript.results.iter_mut() {
                                item.start_time += lateness + discont_offset;
                                item.end_time += lateness + discont_offset;
                            }

                            if !transcript.results.is_empty() {
                                let mut state = self.state.lock().unwrap();
                                self.enqueue_transcript(&mut state, &transcript);
                            }
                        }
                        "EndOfTranscript" => {
                            let mut state = self.state.lock().unwrap();
                            state.send_eos = true;
                        }
                        _ => (),
                    }

                    Ok(())
                }
                _ => Ok(()),
            }
        };

        /* Wrap in a timeout so we can push gaps regularly */
        let future = async move {
            match tokio::time::timeout(Duration::from_millis(GRANULARITY_MS.into()), future).await {
                Err(_) => {
                    if !self.dequeue() {
                        gst::info!(CAT, imp = self, "Failed to push gap event, pausing");

                        let _ = self.pause_task();
                    }
                    Ok(())
                }
                Ok(res) => {
                    if !self.dequeue() {
                        gst::info!(CAT, imp = self, "Failed to push gap event, pausing");

                        let _ = self.pause_task();
                    }
                    res
                }
            }
        };

        RUNTIME.block_on(future)
    }

    fn start_task(&self) -> Result<(), gst::LoggableError> {
        let this_weak = self.downgrade();
        let pad_weak = self.obj().downgrade();
        let (sender, mut receiver) = mpsc::channel(1);

        self.state.lock().unwrap().sender = Some(sender);

        let res = self.obj().start_task(move || {
            let Some(this) = this_weak.upgrade() else {
                if let Some(pad) = pad_weak.upgrade() {
                    let _ = pad.imp().pause_task();
                }
                return;
            };

            if let Err(err) = this.loop_fn(&mut receiver) {
                let parent = this
                    .obj()
                    .parent()
                    .and_downcast::<gst::Element>()
                    .expect("has parent");
                gst::element_error!(
                    parent,
                    gst::StreamError::Failed,
                    ["Streaming failed: {}", err]
                );
                let _ = this.pause_task();
            }
        });
        if res.is_err() {
            return Err(gst::loggable_error!(CAT, "Failed to start pad task"));
        }
        Ok(())
    }

    fn pause_task(&self) -> Result<(), glib::BoolError> {
        self.state.lock().unwrap().sender = None;

        self.obj().pause_task()
    }

    fn stop_task(&self) -> Result<(), glib::BoolError> {
        self.state.lock().unwrap().sender = None;

        self.obj().stop_task()
    }
}

impl Transcriber {
    fn src_activatemode(
        &self,
        pad: &super::TranscriberSrcPad,
        _mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if active {
            pad.imp().start_task()?;
        } else {
            pad.imp().stop_task()?;
        }

        Ok(())
    }

    fn src_query(&self, pad: &super::TranscriberSrcPad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling query {:?}", query);

        match query.view_mut() {
            gst::QueryViewMut::Latency(ref mut q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = self.sinkpad.peer_query(&mut peer_query);

                if ret {
                    let (_, min, _) = peer_query.result();
                    let our_latency = gst::ClockTime::from_mseconds(
                        self.settings.lock().unwrap().latency_ms as u64,
                    );
                    q.set(true, our_latency + min, gst::ClockTime::NONE);
                }
                ret
            }
            gst::QueryViewMut::Position(ref mut q) => {
                if q.format() == gst::Format::Time {
                    let sstate = pad.imp().state.lock().unwrap();
                    q.set(
                        sstate
                            .out_segment
                            .to_stream_time(sstate.out_segment.position()),
                    );
                    true
                } else {
                    false
                }
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::debug!(CAT, obj = pad, "Handling event {:?}", event);

        match event.view() {
            gst::EventView::Eos(_) => match self.handle_buffer(pad, None) {
                Err(err) => {
                    gst::error!(CAT, "Failed to send EOS: {}", err);
                    false
                }
                Ok(_) => true,
            },
            gst::EventView::FlushStart(_) => {
                gst::info!(CAT, imp = self, "Received flush start, disconnecting");
                match self.disconnect() {
                    Err(err) => {
                        self.post_error_message(err);
                        false
                    }
                    Ok(_) => {
                        let mut ret = gst::Pad::event_default(pad, Some(&*self.obj()), event);

                        let mut state = self.state.lock().unwrap();
                        for srcpad in &state.srcpads {
                            if let Err(err) = srcpad.imp().stop_task() {
                                gst::error!(CAT, imp = self, "Failed to stop srcpad task: {}", err);
                                ret = false;
                            }
                        }
                        state.start_time = None;

                        ret
                    }
                }
            }
            gst::EventView::FlushStop(_) => {
                gst::info!(CAT, imp = self, "Received flush stop, restarting task");

                if gst::Pad::event_default(pad, Some(&*self.obj()), event) {
                    let state = self.state.lock().unwrap();
                    for srcpad in &state.srcpads {
                        if let Err(err) = srcpad.imp().start_task() {
                            gst::error!(CAT, imp = self, "Failed to start srcpad task: {}", err);
                            return false;
                        }
                    }
                    true
                } else {
                    false
                }
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

                for srcpad in &state.srcpads {
                    let mut sstate = srcpad.imp().state.lock().unwrap();
                    sstate.out_segment.set_time(segment.time());
                    sstate.out_segment.set_position(gst::ClockTime::ZERO);
                    sstate.seqnum = e.seqnum();
                    srcpad.sticky_events_foreach(|e| {
                        if let gst::EventView::Segment(_) = e.view() {
                            std::ops::ControlFlow::Continue(gst::EventForeachAction::Remove)
                        } else {
                            std::ops::ControlFlow::Continue(gst::EventForeachAction::Keep)
                        }
                    });
                }

                state.in_segment = segment;

                true
            }
            gst::EventView::Tag(_) => true,
            gst::EventView::Caps(_) => true,
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    async fn sync_and_send(
        &self,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut delay = None;
        let mut n_chunks = 0;

        {
            let mut state = self.state.lock().unwrap();

            if let Some(ref buffer) = buffer {
                let running_time = state
                    .in_segment
                    .to_running_time(buffer.pts().expect("Checked in sink_chain()"));
                let now = self.obj().current_running_time().unwrap();

                if let Some(running_time) = running_time {
                    delay = running_time.checked_sub(now);

                    if buffer.flags().contains(gst::BufferFlags::DISCONT) {
                        for srcpad in &state.srcpads {
                            let mut sstate = srcpad.imp().state.lock().unwrap();
                            sstate.discont = true;
                        }
                        if let Some(last_chained_buffer_rtime) = state.last_chained_buffer_rtime {
                            state.discont_offset +=
                                running_time.saturating_sub(last_chained_buffer_rtime);
                        }
                    }

                    state.last_chained_buffer_rtime = Some(running_time);
                }
            }
        }

        if let Some(delay) = delay {
            tokio::time::sleep(Duration::from_nanos(delay.nseconds())).await;
        }

        if let Some(ws_sink) = self.ws_sink.borrow_mut().as_mut() {
            if let Some(buffer) = buffer {
                let data = buffer.map_readable().unwrap();
                for chunk in data.chunks(8192) {
                    ws_sink
                        .send(Message::Binary(chunk.to_vec()))
                        .await
                        .map_err(|err| {
                            gst::error!(CAT, imp = self, "Failed sending packet: {}", err);
                            gst::FlowError::Error
                        })?;
                    n_chunks += 1;
                }
            } else {
                let end_message = EndOfStream {
                    message: "EndOfStream".to_string(),
                    last_seq_no: self.state.lock().unwrap().seq_no,
                };
                let message = serde_json::to_string(&end_message).unwrap();

                ws_sink.send(Message::Text(message)).await.map_err(|err| {
                    gst::error!(CAT, imp = self, "Failed sending packet: {}", err);
                    gst::FlowError::Error
                })?;
            }
        }

        self.state.lock().unwrap().seq_no += n_chunks;

        Ok(gst::FlowSuccess::Ok)
    }

    fn handle_buffer(
        &self,
        _pad: &gst::Pad,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, imp = self, "Handling {:?}", buffer);

        self.ensure_connection().map_err(|err| {
            // No need to worry too much here, we didn't have a session to
            // terminate in the first place
            if buffer.is_none() {
                return gst::FlowError::Eos;
            }
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["Streaming failed: {}", err]
            );
            gst::FlowError::Error
        })?;

        let (future, abort_handle) = abortable(self.sync_and_send(buffer));

        self.state.lock().unwrap().send_abort_handle = Some(abort_handle);

        let res = RUNTIME.block_on(future);

        match res {
            Err(_) => Err(gst::FlowError::Flushing),
            Ok(res) => res,
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if buffer.pts().is_none() {
            gst::error!(CAT, imp = self, "Only buffers with PTS supported");
            return Err(gst::FlowError::Error);
        }

        self.handle_buffer(pad, Some(buffer))
    }

    fn ensure_connection(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        if state.connected {
            return Ok(());
        }

        let in_caps = self
            .sinkpad
            .current_caps()
            .ok_or_else(|| gst::error_msg!(gst::CoreError::Failed, ["No caps set on sinkpad"]))?;
        let s = in_caps.structure(0).unwrap();
        let sample_rate: i32 = s.get::<i32>("rate").unwrap();

        let settings = self.settings.lock().unwrap();

        gst::info!(CAT, imp = self, "Connecting ..");

        let url = match &settings.url {
            Some(url) => url.to_string(),
            None => "ws://0.0.0.0:9000".to_string(),
        };

        let uri = Url::parse(&url).map_err(|e| {
            gst::error_msg!(
                gst::CoreError::Failed,
                ["Failed to parse provided url: {}", e]
            )
        })?;
        let Some(api_key) = settings.api_key.clone() else {
            return Err(gst::error_msg!(
                gst::CoreError::Failed,
                ["An API key is required"]
            ));
        };
        let authority = uri.authority();
        let host = authority.splitn(2, '@').last().unwrap_or("");

        let request = Request::builder()
            .method("GET")
            .uri(&url)
            .header("Host", host)
            .header("Upgrade", "websocket")
            .header("Connection", "keep-alive, upgrade")
            .header(
                "Sec-Websocket-Key",
                async_tungstenite::tungstenite::handshake::client::generate_key(),
            )
            .header("Sec-Websocket-Version", "13")
            .header("Authorization", format!("Bearer {}", &api_key))
            .body(())
            .unwrap();

        let (ws, _) = RUNTIME.block_on(connect_async(request)).map_err(|err| {
            gst::error!(CAT, imp = self, "Failed to connect: {}", err);
            gst::error_msg!(gst::CoreError::Failed, ["Failed to connect: {}", err])
        })?;

        let (mut ws_sink, mut ws_stream) = ws.split();

        if settings.latency_ms + settings.lateness_ms < 4000 {
            gst::error!(
                CAT,
                imp = self,
                "latency + lateness must be superior to 4000 milliseconds"
            );
            return Err(gst::error_msg!(
                gst::LibraryError::Settings,
                ["latency + lateness must be superior to 4000 milliseconds"]
            ));
        }

        let translation_languages = state
            .srcpads
            .iter()
            .flat_map(|pad| pad.imp().settings.lock().unwrap().language_code.clone())
            .collect();
        gst::info!(
            CAT,
            "Translation languages: {:?} ({})",
            translation_languages,
            state.srcpads.len()
        );

        // Workaround for speechmatics sometimes outputting
        // final punctuation in the next transcript
        let max_delay = ((settings.latency_ms + settings.lateness_ms) as f32) / 2_000.;

        let start_message = StartRecognition {
            message: "StartRecognition".to_string(),
            audio_format: AudioType {
                type_: "raw".to_string(),
                encoding: "pcm_s16le".to_string(),
                sample_rate: sample_rate as u32,
            },
            transcription_config: TranscriptionConfig {
                language: settings
                    .language_code
                    .clone()
                    .unwrap_or_else(|| "en".to_string()),
                enable_partials: false,
                max_delay,
                additional_vocab: state.additional_vocabulary.clone(),
            },
            translation_config: TranslationConfig {
                target_languages: translation_languages,
                enable_partials: false,
            },
        };

        let message = serde_json::to_string(&start_message).unwrap();

        gst::trace!(CAT, imp = self, "Sending start message: {}", message);

        RUNTIME
            .block_on(ws_sink.send(Message::Text(message)))
            .map_err(|err| {
                gst::error!(CAT, imp = self, "Failed to send StartRecognition: {err}");
                gst::error_msg!(
                    gst::CoreError::Failed,
                    ["Failed to send StartRecognition: {err}"]
                )
            })?;

        loop {
            let res = RUNTIME
                .block_on(ws_stream.next())
                .ok_or_else(|| {
                    gst::error!(CAT, imp = self, "Connection closed unexpectedly");
                    gst::error_msg!(gst::CoreError::Failed, ["Connection closed unexpectedly"])
                })?
                .map_err(|err| {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Failed to receive RecognitionStarted: {err}"
                    );
                    gst::error_msg!(
                        gst::CoreError::Failed,
                        ["Failed to receive RecognitionStarted: {err}"]
                    )
                })?;

            let text = match res {
                Message::Text(text) => Ok(text),
                _ => {
                    gst::error!(CAT, imp = self, "Invalid message type: {}", res);
                    Err(gst::error_msg!(
                        gst::CoreError::Failed,
                        ["Invalid message type: {}", res]
                    ))
                }
            }?;

            let mut json: serde_json::Value = serde_json::from_str(&text).unwrap();
            let message_type = {
                let obj = json.as_object_mut().expect("object");
                let message_type = obj.remove("message").expect("`message` field");
                if let serde_json::Value::String(s) = message_type {
                    s
                } else {
                    panic!("message field not a string");
                }
            };

            match message_type.as_str() {
                "RecognitionStarted" => {
                    gst::info!(CAT, imp = self, "Recognition started!");
                    break;
                }
                "Error" => {
                    let error: TranscriptError = serde_json::from_value(json).map_err(|err| {
                        gst::error_msg!(
                            gst::StreamError::Failed,
                            ["Unexpected message: {} ({})", text, err]
                        )
                    })?;
                    gst::error!(
                        CAT,
                        imp = self,
                        "StartRecognition failed: {} ({})",
                        error.type_,
                        error.reason
                    );
                    Err(gst::error_msg!(
                        gst::CoreError::Failed,
                        ("StartRecognition failed"),
                        ["{} ({})", error.type_, error.reason]
                    ))
                }
                _ => {
                    continue;
                }
            }?;
        }

        *self.ws_sink.borrow_mut() = Some(Box::pin(ws_sink));

        let this_weak = self.downgrade();
        let future = async move {
            'outer: while let Some(this) = this_weak.upgrade() {
                let Some(msg) = ws_stream.next().await else {
                    let state = this.state.lock().unwrap();
                    for srcpad in &state.srcpads {
                        let mut sstate = srcpad.imp().state.lock().unwrap();
                        sstate.send_eos = true;
                    }
                    break;
                };

                let msg = match msg {
                    Ok(msg) => msg,
                    Err(err) => {
                        gst::error!(CAT, imp = this, "Failed to receive data: {}", err);
                        gst::element_imp_error!(
                            this,
                            gst::StreamError::Failed,
                            ["Streaming failed: {}", err]
                        );
                        break;
                    }
                };

                let srcpads = this.state.lock().unwrap().srcpads.clone();
                for srcpad in srcpads {
                    let mut sender = srcpad.imp().state.lock().unwrap().sender.clone();

                    if let Some(sender) = sender.as_mut() {
                        let msg = msg.clone();
                        if sender.send(msg).await.is_err() {
                            break 'outer;
                        }
                    }
                }
            }
        };

        let (future, abort_handle) = abortable(future);

        state.recv_abort_handle = Some(abort_handle);

        RUNTIME.spawn(future);

        state.connected = true;

        gst::info!(CAT, imp = self, "Connected");

        Ok(())
    }

    fn disconnect(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        gst::info!(CAT, imp = self, "Unpreparing");

        if let Some(abort_handle) = state.recv_abort_handle.take() {
            abort_handle.abort();
        }

        if let Some(abort_handle) = state.send_abort_handle.take() {
            abort_handle.abort();
        }

        let _ = self.sinkpad.stream_lock();

        if let Some(mut ws_sink) = self.ws_sink.borrow_mut().take() {
            RUNTIME.block_on(async {
                let _ = ws_sink.close().await;
            });
        }

        *state = State::default();

        gst::info!(
            CAT,
            imp = self,
            "Unprepared, connected: {}!",
            state.connected
        );

        Ok(())
    }

    fn get_start_time_and_now(&self) -> Option<(gst::ClockTime, gst::ClockTime)> {
        let now = self.obj().current_running_time()?;

        let mut state = self.state.lock().unwrap();

        if state.start_time.is_none() {
            state.start_time = Some(now);
            for pad in state.srcpads.iter() {
                let mut sstate = pad.imp().state.lock().unwrap();
                sstate.out_segment.set_position(now);
            }
        }

        Some((state.start_time.unwrap(), now))
    }
}

// Implementation of gst::ChildProxy virtual methods.
//
// This allows accessing the pads and their properties from e.g. gst-launch.
impl ChildProxyImpl for Transcriber {
    fn children_count(&self) -> u32 {
        let object = self.obj();
        object.num_pads() as u32
    }

    fn child_by_name(&self, name: &str) -> Option<glib::Object> {
        let object = self.obj();
        object
            .pads()
            .into_iter()
            .find(|p| p.name() == name)
            .map(|p| p.upcast())
    }

    fn child_by_index(&self, index: u32) -> Option<glib::Object> {
        let object = self.obj();
        object
            .pads()
            .into_iter()
            .nth(index as usize)
            .map(|p| p.upcast())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Transcriber {
    const NAME: &'static str = "GstSpeechmaticsTranscriber";
    type Type = super::Transcriber;
    type ParentType = gst::Element;
    type Interfaces = (gst::ChildProxy,);

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |transcriber| transcriber.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber| transcriber.sink_event(pad, event),
                )
            })
            .build();

        let settings = Mutex::new(Settings::default());

        Self {
            sinkpad,
            settings,
            state: Default::default(),
            ws_sink: Default::default(),
        }
    }
}

impl GstObjectImpl for Transcriber {}

impl ObjectImpl for Transcriber {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("language-code")
                    .nick("Language Code")
                    .blurb("The Language of the Stream, ISO code")
                    .default_value("en")
                    .build(),
                glib::ParamSpecUInt::builder("latency")
                    .nick("Latency")
                    .blurb("Amount of milliseconds to allow for transcription")
                    .default_value(DEFAULT_LATENCY_MS)
                    .build(),
                glib::ParamSpecUInt::builder("lateness")
                    .nick("Lateness")
                    .blurb("Amount of milliseconds to introduce as lateness")
                    .default_value(DEFAULT_LATENESS_MS)
                    .build(),
                glib::ParamSpecString::builder("url")
                    .nick("URL")
                    .blurb("URL of the transcription server")
                    .default_value("ws://0.0.0.0:9000")
                    .build(),
                gst::ParamSpecArray::builder("additional-vocabulary")
                    .nick("Additional Vocabulary")
                    .blurb("Additional vocabulary speechmatics should use")
                    .element_spec(
                        &glib::ParamSpecBoxed::builder::<gst::Structure>("vocable")
                            .nick("Vocable")
                            .blurb("A vocable in the vocabulary")
                            .build(),
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("api-key")
                    .nick("API Key")
                    .blurb("Speechmatics API Key")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();

        let templ = obj.class().pad_template("src").unwrap();
        let srcpad: super::TranscriberSrcPad = gst::PadBuilder::from_template(&templ)
            .activatemode_function(|pad, parent, mode, active| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || {
                        Err(gst::loggable_error!(
                            CAT,
                            "Panic activating src pad with mode"
                        ))
                    },
                    |transcriber| transcriber.src_activatemode(pad, mode, active),
                )
            })
            .query_function(|pad, parent, query| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber| transcriber.src_query(pad, query),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();
        obj.add_pad(&srcpad).unwrap();
        self.state.lock().unwrap().srcpads.insert(srcpad);
        obj.set_element_flags(gst::ElementFlags::PROVIDE_CLOCK | gst::ElementFlags::REQUIRE_CLOCK);
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "language-code" => {
                let mut settings = self.settings.lock().unwrap();
                settings.language_code = value.get().expect("type checked upstream");
            }
            "latency" => {
                let mut settings = self.settings.lock().unwrap();
                let old_latency_ms = settings.latency_ms;
                settings.latency_ms = value.get().expect("type checked upstream");
                if settings.latency_ms != old_latency_ms {
                    gst::debug!(CAT, imp = self, "Latency changed: {}", settings.latency_ms);
                    drop(settings);
                    let _ = self
                        .obj()
                        .post_message(gst::message::Latency::builder().src(&*self.obj()).build());
                }
            }
            "lateness" => {
                let mut settings = self.settings.lock().unwrap();
                settings.lateness_ms = value.get().expect("type checked upstream");
            }
            "url" => {
                let mut settings = self.settings.lock().unwrap();
                settings.url = value.get().expect("type checked upstream");
            }
            "additional-vocabulary" => {
                let mut state = self.state.lock().unwrap();
                state.additional_vocabulary = vec![];
                let vocables: gst::Array = value.get().expect("type checked upstream");
                for vocable in vocables.as_slice() {
                    let Some(s) = vocable
                        .get::<Option<gst::Structure>>()
                        .expect("type checked upstream")
                    else {
                        continue;
                    };

                    let Ok(content) = s.get::<String>("word") else {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "skipping vocable: {s}, expected word field",
                        );
                        continue;
                    };

                    let sounds_like: Vec<String> = match s.get::<gst::Array>("sounds_like") {
                        Ok(sounds_like) => sounds_like
                            .as_slice()
                            .iter()
                            .filter_map(|s| s.get::<Option<String>>().unwrap_or(None))
                            .collect(),
                        Err(_) => vec![],
                    };

                    state.additional_vocabulary.push(Vocable {
                        content,
                        sounds_like,
                    });
                }
            }
            "api-key" => {
                let mut settings = self.settings.lock().unwrap();
                settings.api_key = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "language-code" => {
                let settings = self.settings.lock().unwrap();
                settings.language_code.to_value()
            }
            "latency" => {
                let settings = self.settings.lock().unwrap();
                settings.latency_ms.to_value()
            }
            "lateness" => {
                let settings = self.settings.lock().unwrap();
                settings.lateness_ms.to_value()
            }
            "url" => {
                let settings = self.settings.lock().unwrap();
                settings.url.to_value()
            }
            "additional-vocabulary" => {
                let state = self.state.lock().unwrap();
                let mut additional_vocabulary = vec![];
                for vocable in &state.additional_vocabulary {
                    let mut s = gst::Structure::new_empty(&vocable.content);
                    if !vocable.sounds_like.is_empty() {
                        s.set(
                            "sounds_like",
                            gst::Array::new(
                                vocable.sounds_like.iter().map(|word| word.to_send_value()),
                            ),
                        );
                    }
                    additional_vocabulary.push(s.to_send_value());
                }
                gst::Array::new(additional_vocabulary).to_value()
            }
            "api-key" => {
                let settings = self.settings.lock().unwrap();
                settings.api_key.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for Transcriber {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Transcriber",
                "Audio/Text/Filter",
                "Speech to Text filter, using Speechmatics transcribe",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_caps = gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build();
            let src_pad_template = gst::PadTemplate::with_gtype(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
                super::TranscriberSrcPad::static_type(),
            )
            .unwrap();
            let req_src_pad_template = gst::PadTemplate::with_gtype(
                "translate_src_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Request,
                &src_caps,
                super::TranscriberSrcPad::static_type(),
            )
            .unwrap();

            let sink_caps = gst_audio::AudioCapsBuilder::new()
                .format(gst_audio::AUDIO_FORMAT_S16)
                .rate_range(8000..=48000)
                .channels(1)
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            vec![src_pad_template, req_src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        _name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let mut state = self.state.lock().unwrap();

        let pad: super::TranscriberSrcPad = gst::PadBuilder::from_template(templ)
            .activatemode_function(|pad, parent, mode, active| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || {
                        Err(gst::loggable_error!(
                            CAT,
                            "Panic activating src pad with mode"
                        ))
                    },
                    |transcriber| transcriber.src_activatemode(pad, mode, active),
                )
            })
            .query_function(|pad, parent, query| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber| transcriber.src_query(pad, query),
                )
            })
            .name(format!("translate_src_{}", state.pad_serial).as_str())
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        state.srcpads.insert(pad.clone());

        gst::info!(CAT, "New pad requested, {}", state.srcpads.len());

        state.pad_serial += 1;
        drop(state);

        self.obj().add_pad(&pad).unwrap();

        self.obj().child_added(&pad, &pad.name());

        Some(pad.upcast())
    }

    fn release_pad(&self, pad: &gst::Pad) {
        pad.set_active(false).unwrap();
        self.obj().remove_pad(pad).unwrap();

        self.obj().child_removed(pad, &pad.name());
        let _ = self
            .obj()
            .post_message(gst::message::Latency::builder().src(&*self.obj()).build());
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::info!(CAT, imp = self, "Changing state {:?}", transition);

        if transition == gst::StateChange::PausedToReady {
            self.disconnect().map_err(|err| {
                self.post_error_message(err);
                gst::StateChangeError
            })?;
        }

        let mut success = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            _ => (),
        }

        Ok(success)
    }

    fn provide_clock(&self) -> Option<gst::Clock> {
        Some(gst::SystemClock::obtain())
    }
}

#[derive(Debug, Default, Clone)]
struct TranscriberSrcPadSettings {
    language_code: Option<String>,
}

#[derive(Debug)]
struct TranscriberSrcPadState {
    sender: Option<mpsc::Sender<Message>>,
    accumulator: Option<ItemAccumulator>,
    buffers: VecDeque<gst::Buffer>,
    discont: bool,
    send_eos: bool,
    out_segment: gst::FormattedSegment<gst::ClockTime>,
    seqnum: gst::Seqnum,
}

impl Default for TranscriberSrcPadState {
    fn default() -> Self {
        Self {
            sender: None,
            accumulator: None,
            buffers: VecDeque::new(),
            discont: true,
            send_eos: false,
            out_segment: gst::FormattedSegment::new(),
            seqnum: gst::Seqnum::next(),
        }
    }
}

#[derive(Debug, Default)]
pub struct TranscriberSrcPad {
    settings: Mutex<TranscriberSrcPadSettings>,
    state: Mutex<TranscriberSrcPadState>,
}

impl TranscriberSrcPadState {
    fn push_buffer(&mut self, mut buf: gst::Buffer) {
        if self.discont {
            let buf = buf.make_mut();
            buf.set_flags(gst::BufferFlags::DISCONT);
            self.discont = false;
        }

        self.buffers.push_back(buf);
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TranscriberSrcPad {
    const NAME: &'static str = "GstSpeechmaticsTranscriberSrcPad";
    type Type = super::TranscriberSrcPad;
    type ParentType = gst::Pad;

    fn new() -> Self {
        Default::default()
    }
}

const OUTPUT_LANG_CODE_PROPERTY: &str = "language-code";
const DEFAULT_OUTPUT_LANG_CODE: Option<&str> = None;

impl ObjectImpl for TranscriberSrcPad {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![glib::ParamSpecString::builder(OUTPUT_LANG_CODE_PROPERTY)
                .nick("Language Code")
                .blurb("The Language the Stream must be translated to")
                .default_value(DEFAULT_OUTPUT_LANG_CODE)
                .mutable_ready()
                .build()]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            OUTPUT_LANG_CODE_PROPERTY => {
                self.settings.lock().unwrap().language_code = value.get().unwrap()
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            OUTPUT_LANG_CODE_PROPERTY => self.settings.lock().unwrap().language_code.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for TranscriberSrcPad {}

impl PadImpl for TranscriberSrcPad {}
