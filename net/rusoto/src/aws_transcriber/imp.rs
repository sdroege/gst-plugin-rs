// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{
    element_error, error_msg, gst_debug, gst_error, gst_info, gst_log, gst_trace, gst_warning,
    loggable_error,
};

use std::default::Default;

use rusoto_core::Region;
use rusoto_credential::{ChainProvider, ProvideAwsCredentials, StaticProvider};

use rusoto_signature::signature::SignedRequest;

use async_tungstenite::tungstenite::error::Error as WsError;
use async_tungstenite::{tokio::connect_async, tungstenite::Message};
use futures::channel::mpsc;
use futures::future::{abortable, AbortHandle};
use futures::prelude::*;
use tokio::runtime;

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Mutex;

use atomic_refcell::AtomicRefCell;

use super::packet::*;

use serde_derive::{Deserialize, Serialize};

use once_cell::sync::Lazy;

use super::AwsTranscriberResultStability;

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct TranscriptItem {
    content: String,
    end_time: f32,
    start_time: f32,
    #[serde(rename = "Type")]
    type_: String,
    stable: bool,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct TranscriptAlternative {
    items: Vec<TranscriptItem>,
    transcript: String,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct TranscriptResult {
    alternatives: Vec<TranscriptAlternative>,
    end_time: f32,
    start_time: f32,
    is_partial: bool,
    result_id: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct TranscriptTranscript {
    results: Vec<TranscriptResult>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct Transcript {
    transcript: TranscriptTranscript,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct ExceptionMessage {
    message: String,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "awstranscribe",
        gst::DebugColorFlags::empty(),
        Some("AWS Transcribe element"),
    )
});

static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::from_seconds(8);
const DEFAULT_LATENESS: gst::ClockTime = gst::ClockTime::from_seconds(0);
const DEFAULT_STABILITY: AwsTranscriberResultStability = AwsTranscriberResultStability::Low;
const GRANULARITY: gst::ClockTime = gst::ClockTime::from_mseconds(100);

#[derive(Debug, Clone)]
struct Settings {
    latency: gst::ClockTime,
    lateness: gst::ClockTime,
    language_code: Option<String>,
    vocabulary: Option<String>,
    session_id: Option<String>,
    results_stability: AwsTranscriberResultStability,
    access_key: Option<String>,
    secret_access_key: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            latency: DEFAULT_LATENCY,
            lateness: DEFAULT_LATENESS,
            language_code: Some("en-US".to_string()),
            vocabulary: None,
            session_id: None,
            results_stability: DEFAULT_STABILITY,
            access_key: None,
            secret_access_key: None,
        }
    }
}

struct State {
    connected: bool,
    sender: Option<mpsc::Sender<Message>>,
    recv_abort_handle: Option<AbortHandle>,
    send_abort_handle: Option<AbortHandle>,
    in_segment: gst::FormattedSegment<gst::ClockTime>,
    out_segment: gst::FormattedSegment<gst::ClockTime>,
    seqnum: gst::Seqnum,
    buffers: VecDeque<gst::Buffer>,
    send_eos: bool,
    discont: bool,
    partial_index: usize,
    send_events: bool,
    start_time: Option<gst::ClockTime>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            connected: false,
            sender: None,
            recv_abort_handle: None,
            send_abort_handle: None,
            in_segment: gst::FormattedSegment::new(),
            out_segment: gst::FormattedSegment::new(),
            seqnum: gst::Seqnum::next(),
            buffers: VecDeque::new(),
            send_eos: false,
            discont: true,
            partial_index: 0,
            send_events: true,
            start_time: None,
        }
    }
}

type WsSink = Pin<Box<dyn Sink<Message, Error = WsError> + Send + Sync>>;

pub struct Transcriber {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    ws_sink: AtomicRefCell<Option<WsSink>>,
}

fn build_packet(payload: &[u8]) -> Vec<u8> {
    let headers = [
        Header {
            name: ":event-type".into(),
            value: "AudioEvent".into(),
            value_type: 7,
        },
        Header {
            name: ":content-type".into(),
            value: "application/octet-stream".into(),
            value_type: 7,
        },
        Header {
            name: ":message-type".into(),
            value: "event".into(),
            value_type: 7,
        },
    ];

    encode_packet(payload, &headers).expect("foobar")
}

impl Transcriber {
    fn dequeue(&self, element: &super::Transcriber) -> bool {
        /* First, check our pending buffers */
        let mut items = vec![];

        let now = match element.current_running_time() {
            Some(now) => now,
            None => {
                return true;
            }
        };

        let latency = self.settings.lock().unwrap().latency;

        let mut state = self.state.lock().unwrap();

        if state.start_time.is_none() {
            state.start_time = Some(now);
            state.out_segment.set_position(now);
        }

        let start_time = state.start_time.unwrap();
        let mut last_position = state.out_segment.position().unwrap();

        let send_eos = state.send_eos && state.buffers.is_empty();

        while let Some(buf) = state.buffers.front() {
            let pts = buf.pts().unwrap();
            gst_trace!(
                CAT,
                obj: element,
                "Checking now {} if item is ready for dequeuing, PTS {}, threshold {} vs {}",
                now,
                pts,
                pts + latency.saturating_sub(3 * GRANULARITY),
                now - start_time
            );

            if pts + latency.saturating_sub(3 * GRANULARITY) < now - start_time {
                /* Safe unwrap, we know we have an item */
                let mut buf = state.buffers.pop_front().unwrap();

                {
                    let buf_mut = buf.get_mut().unwrap();

                    buf_mut.set_pts(start_time + pts);
                }

                items.push(buf);
            } else {
                break;
            }
        }

        let seqnum = state.seqnum;

        drop(state);

        /* We're EOS, we can pause and exit early */
        if send_eos {
            let _ = self.srcpad.pause_task();

            return self
                .srcpad
                .push_event(gst::event::Eos::builder().seqnum(seqnum).build());
        }

        for mut buf in items.drain(..) {
            let mut pts = buf.pts().unwrap();
            let mut duration = buf.duration().unwrap();

            match pts.cmp(&last_position) {
                Ordering::Greater => {
                    let gap_event = gst::event::Gap::builder(last_position)
                        .duration(pts - last_position)
                        .seqnum(seqnum)
                        .build();
                    gst_log!(CAT, "Pushing gap:    {} -> {}", last_position, pts);
                    if !self.srcpad.push_event(gap_event) {
                        return false;
                    }
                }
                Ordering::Less => {
                    let delta = last_position - pts;

                    gst_warning!(
                        CAT,
                        obj: element,
                        "Updating item PTS ({} < {}), consider increasing latency",
                        pts,
                        last_position
                    );

                    pts = last_position;
                    duration = duration.saturating_sub(delta);

                    {
                        let buf_mut = buf.get_mut().unwrap();

                        buf_mut.set_pts(pts);
                        buf_mut.set_duration(duration);
                    }
                }
                _ => (),
            }

            last_position = pts + duration;

            gst_debug!(CAT, "Pushing buffer: {} -> {}", pts, pts + duration);

            if self.srcpad.push(buf).is_err() {
                return false;
            }
        }

        /* next, push a gap if we're lagging behind the target position */
        gst_trace!(
            CAT,
            obj: element,
            "Checking now: {} if we need to push a gap, last_position: {}, threshold: {}",
            now,
            last_position,
            last_position + latency.saturating_sub(GRANULARITY)
        );

        if now > last_position + latency.saturating_sub(GRANULARITY) {
            let duration = now - last_position - latency.saturating_sub(GRANULARITY);

            let gap_event = gst::event::Gap::builder(last_position)
                .duration(duration)
                .seqnum(seqnum)
                .build();

            gst_log!(
                CAT,
                "Pushing gap:    {} -> {}",
                last_position,
                last_position + duration
            );

            last_position += duration;

            if !self.srcpad.push_event(gap_event) {
                return false;
            }
        }

        self.state
            .lock()
            .unwrap()
            .out_segment
            .set_position(last_position);

        true
    }

    fn enqueue(
        &self,
        element: &super::Transcriber,
        state: &mut State,
        alternative: &TranscriptAlternative,
        partial: bool,
    ) {
        let lateness = self.settings.lock().unwrap().lateness;

        if alternative.items.len() <= state.partial_index {
            gst_error!(
                CAT,
                obj: element,
                "sanity check failed, alternative length {} < partial_index {}",
                alternative.items.len(),
                state.partial_index
            );

            if !partial {
                state.partial_index = 0;
            }

            return;
        }

        for item in &alternative.items[state.partial_index..] {
            let start_time =
                gst::ClockTime::from_nseconds((item.start_time as f64 * 1_000_000_000.0) as u64)
                    + lateness;
            let end_time =
                gst::ClockTime::from_nseconds((item.end_time as f64 * 1_000_000_000.0) as u64)
                    + lateness;

            if !item.stable {
                break;
            }

            /* Should be sent now */
            gst_debug!(
                CAT,
                obj: element,
                "Item is ready for queuing: {}, PTS {}",
                item.content,
                start_time
            );
            let mut buf = gst::Buffer::from_mut_slice(item.content.clone().into_bytes());

            {
                let buf = buf.get_mut().unwrap();

                if state.discont {
                    buf.set_flags(gst::BufferFlags::DISCONT);
                    state.discont = false;
                }

                buf.set_pts(start_time);
                buf.set_duration(end_time - start_time);
            }

            state.partial_index += 1;

            state.buffers.push_back(buf);
        }

        if !partial {
            state.partial_index = 0;
        }
    }

    fn loop_fn(
        &self,
        element: &super::Transcriber,
        receiver: &mut mpsc::Receiver<Message>,
    ) -> Result<(), gst::ErrorMessage> {
        let mut events = {
            let mut events = vec![];

            let mut state = self.state.lock().unwrap();

            if state.send_events {
                events.push(
                    gst::event::StreamStart::builder("transcription")
                        .seqnum(state.seqnum)
                        .build(),
                );

                let caps = gst::Caps::builder("text/x-raw")
                    .field("format", "utf8")
                    .build();
                events.push(
                    gst::event::Caps::builder(&caps)
                        .seqnum(state.seqnum)
                        .build(),
                );

                events.push(
                    gst::event::Segment::builder(&state.out_segment)
                        .seqnum(state.seqnum)
                        .build(),
                );

                state.send_events = false;
            }

            events
        };

        for event in events.drain(..) {
            gst_info!(CAT, obj: element, "Sending {:?}", event);
            self.srcpad.push_event(event);
        }

        let future = async move {
            let msg = match receiver.next().await {
                Some(msg) => msg,
                /* Sender was closed */
                None => {
                    let _ = self.srcpad.pause_task();
                    return Ok(());
                }
            };

            match msg {
                Message::Binary(buf) => {
                    let (_, pkt) = parse_packet(&buf).map_err(|err| {
                        gst_error!(CAT, obj: element, "Failed to parse packet: {}", err);
                        error_msg!(
                            gst::StreamError::Failed,
                            ["Failed to parse packet: {}", err]
                        )
                    })?;

                    let payload = std::str::from_utf8(pkt.payload).unwrap();

                    if packet_is_exception(&pkt) {
                        let message: ExceptionMessage =
                            serde_json::from_str(payload).map_err(|err| {
                                gst_error!(
                                    CAT,
                                    obj: element,
                                    "Unexpected exception message: {} ({})",
                                    payload,
                                    err
                                );
                                error_msg!(
                                    gst::StreamError::Failed,
                                    ["Unexpected exception message: {} ({})", payload, err]
                                )
                            })?;
                        gst_error!(
                            CAT,
                            obj: element,
                            "AWS raised an error: {}",
                            message.message
                        );

                        return Err(error_msg!(
                            gst::StreamError::Failed,
                            ["AWS raised an error: {}", message.message]
                        ));
                    }

                    let transcript: Transcript = serde_json::from_str(payload).map_err(|err| {
                        error_msg!(
                            gst::StreamError::Failed,
                            ["Unexpected binary message: {} ({})", payload, err]
                        )
                    })?;

                    if let Some(result) = transcript.transcript.results.get(0) {
                        gst_trace!(
                            CAT,
                            obj: element,
                            "result: {}",
                            serde_json::to_string_pretty(&result).unwrap(),
                        );

                        if let Some(alternative) = result.alternatives.get(0) {
                            let mut state = self.state.lock().unwrap();

                            self.enqueue(element, &mut state, alternative, result.is_partial)
                        }
                    }

                    Ok(())
                }

                _ => Ok(()),
            }
        };

        /* Wrap in a timeout so we can push gaps regularly */
        let future = async move {
            match tokio::time::timeout(GRANULARITY.into(), future).await {
                Err(_) => {
                    if !self.dequeue(element) {
                        gst_info!(CAT, obj: element, "Failed to push gap event, pausing");

                        let _ = self.srcpad.pause_task();
                    }
                    Ok(())
                }
                Ok(res) => {
                    if !self.dequeue(element) {
                        gst_info!(CAT, obj: element, "Failed to push gap event, pausing");

                        let _ = self.srcpad.pause_task();
                    }
                    res
                }
            }
        };

        let _enter = RUNTIME.enter();
        futures::executor::block_on(future)
    }

    fn start_task(&self, element: &super::Transcriber) -> Result<(), gst::LoggableError> {
        let element_weak = element.downgrade();
        let pad_weak = self.srcpad.downgrade();
        let (sender, mut receiver) = mpsc::channel(1);

        {
            let mut state = self.state.lock().unwrap();
            state.sender = Some(sender);
        }

        let res = self.srcpad.start_task(move || {
            let element = match element_weak.upgrade() {
                Some(element) => element,
                None => {
                    if let Some(pad) = pad_weak.upgrade() {
                        let _ = pad.pause_task();
                    }
                    return;
                }
            };

            let transcribe = element.imp();
            if let Err(err) = transcribe.loop_fn(&element, &mut receiver) {
                element_error!(
                    &element,
                    gst::StreamError::Failed,
                    ["Streaming failed: {}", err]
                );
                let _ = transcribe.srcpad.pause_task();
            }
        });
        if res.is_err() {
            return Err(loggable_error!(CAT, "Failed to start pad task"));
        }
        Ok(())
    }

    fn src_activatemode(
        &self,
        _pad: &gst::Pad,
        element: &super::Transcriber,
        _mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if active {
            self.start_task(element)?;
        } else {
            {
                let mut state = self.state.lock().unwrap();
                state.sender = None;
            }

            let _ = self.srcpad.stop_task();
        }

        Ok(())
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        element: &super::Transcriber,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = self.sinkpad.peer_query(&mut peer_query);

                if ret {
                    let (_, min, _) = peer_query.result();
                    let our_latency = self.settings.lock().unwrap().latency;
                    q.set(true, our_latency + min, gst::ClockTime::NONE);
                }
                ret
            }
            QueryView::Position(ref mut q) => {
                if q.format() == gst::Format::Time {
                    let state = self.state.lock().unwrap();
                    q.set(
                        state
                            .out_segment
                            .to_stream_time(state.out_segment.position()),
                    );
                    true
                } else {
                    false
                }
            }
            _ => pad.query_default(Some(element), query),
        }
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::Transcriber, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Eos(_) => match self.handle_buffer(pad, element, None) {
                Err(err) => {
                    gst_error!(CAT, "Failed to send EOS to AWS: {}", err);
                    false
                }
                Ok(_) => true,
            },
            EventView::FlushStart(_) => {
                gst_info!(CAT, obj: element, "Received flush start, disconnecting");
                let mut ret = pad.event_default(Some(element), event);

                match self.srcpad.stop_task() {
                    Err(err) => {
                        gst_error!(CAT, obj: element, "Failed to stop srcpad task: {}", err);

                        self.disconnect(element);

                        ret = false;
                    }
                    Ok(_) => {
                        self.disconnect(element);
                    }
                };

                ret
            }
            EventView::FlushStop(_) => {
                gst_info!(CAT, obj: element, "Received flush stop, restarting task");

                if pad.event_default(Some(element), event) {
                    match self.start_task(element) {
                        Err(err) => {
                            gst_error!(CAT, obj: element, "Failed to start srcpad task: {}", err);
                            false
                        }
                        Ok(_) => true,
                    }
                } else {
                    false
                }
            }
            EventView::Segment(e) => {
                let segment = match e.segment().clone().downcast::<gst::ClockTime>() {
                    Err(segment) => {
                        element_error!(
                            element,
                            gst::StreamError::Format,
                            ["Only Time segments supported, got {:?}", segment.format(),]
                        );
                        return false;
                    }
                    Ok(segment) => segment,
                };

                let mut state = self.state.lock().unwrap();

                state.in_segment = segment;
                state.seqnum = e.seqnum();

                true
            }
            EventView::Tag(_) => true,
            EventView::Caps(e) => {
                gst_info!(CAT, "Received caps {:?}", e);
                true
            }
            EventView::StreamStart(_) => true,
            _ => pad.event_default(Some(element), event),
        }
    }

    async fn sync_and_send(
        &self,
        element: &super::Transcriber,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut delay = None;

        {
            let state = self.state.lock().unwrap();

            if let Some(buffer) = &buffer {
                let running_time = state.in_segment.to_running_time(buffer.pts());
                let now = element.current_running_time();

                delay = running_time.opt_checked_sub(now).ok().flatten();
            }
        }

        if let Some(delay) = delay {
            tokio::time::sleep(delay.into()).await;
        }

        if let Some(ws_sink) = self.ws_sink.borrow_mut().as_mut() {
            if let Some(buffer) = buffer {
                let data = buffer.map_readable().unwrap();
                for chunk in data.chunks(8192) {
                    let packet = build_packet(chunk);
                    ws_sink.send(Message::Binary(packet)).await.map_err(|err| {
                        gst_error!(CAT, obj: element, "Failed sending packet: {}", err);
                        gst::FlowError::Error
                    })?;
                }
            } else {
                // EOS
                let packet = build_packet(&[]);
                ws_sink.send(Message::Binary(packet)).await.map_err(|err| {
                    gst_error!(CAT, obj: element, "Failed sending packet: {}", err);
                    gst::FlowError::Error
                })?;
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn handle_buffer(
        &self,
        _pad: &gst::Pad,
        element: &super::Transcriber,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: element, "Handling {:?}", buffer);

        self.ensure_connection(element).map_err(|err| {
            element_error!(
                element,
                gst::StreamError::Failed,
                ["Streaming failed: {}", err]
            );
            gst::FlowError::Error
        })?;

        let (future, abort_handle) = abortable(self.sync_and_send(element, buffer));

        self.state.lock().unwrap().send_abort_handle = Some(abort_handle);

        let res = {
            let _enter = RUNTIME.enter();
            futures::executor::block_on(future)
        };

        match res {
            Err(_) => Err(gst::FlowError::Flushing),
            Ok(res) => res,
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::Transcriber,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.handle_buffer(pad, element, Some(buffer))
    }

    fn ensure_connection(&self, element: &super::Transcriber) -> Result<(), gst::ErrorMessage> {
        let state = self.state.lock().unwrap();

        if state.connected {
            return Ok(());
        }

        let in_caps = self.sinkpad.current_caps().unwrap();
        let s = in_caps.structure(0).unwrap();
        let sample_rate = s.get::<i32>("rate").unwrap();

        let settings = self.settings.lock().unwrap();

        if settings.latency + settings.lateness <= 2 * GRANULARITY {
            gst_error!(
                CAT,
                obj: element,
                "latency + lateness must be greater than 200 milliseconds"
            );
            return Err(error_msg!(
                gst::LibraryError::Settings,
                ["latency + lateness must be greater than 200 milliseconds"]
            ));
        }

        gst_info!(CAT, obj: element, "Connecting ..");

        let creds = match (
            settings.access_key.as_ref(),
            settings.secret_access_key.as_ref(),
        ) {
            (Some(access_key), Some(secret_access_key)) => {
                let _enter = RUNTIME.enter();
                futures::executor::block_on(
                    StaticProvider::new_minimal(access_key.clone(), secret_access_key.clone())
                        .credentials()
                        .map_err(|err| {
                            gst_error!(
                                CAT,
                                obj: element,
                                "Failed to generate credentials: {}",
                                err
                            );
                            error_msg!(
                                gst::CoreError::Failed,
                                ["Failed to generate credentials: {}", err]
                            )
                        }),
                )?
            }
            _ => {
                let _enter = RUNTIME.enter();
                futures::executor::block_on(ChainProvider::new().credentials()).map_err(|err| {
                    gst_error!(CAT, obj: element, "Failed to generate credentials: {}", err);
                    error_msg!(
                        gst::CoreError::Failed,
                        ["Failed to generate credentials: {}", err]
                    )
                })?
            }
        };

        let language_code = settings
            .language_code
            .as_ref()
            .expect("Language code is required");

        let region = Region::UsEast1;

        let mut signed = SignedRequest::new(
            "GET",
            "transcribe",
            &region,
            "/stream-transcription-websocket",
        );
        signed.set_hostname(Some(format!(
            "transcribestreaming.{}.amazonaws.com:8443",
            region.name()
        )));
        signed.add_param("language-code", language_code);
        signed.add_param("media-encoding", "pcm");
        signed.add_param("sample-rate", &sample_rate.to_string());

        if let Some(ref vocabulary) = settings.vocabulary {
            signed.add_param("vocabulary-name", vocabulary);
        }

        if let Some(ref session_id) = settings.session_id {
            gst_debug!(CAT, obj: element, "Using session ID: {}", session_id);
            signed.add_param("session-id", session_id);
        }

        signed.add_param("enable-partial-results-stabilization", "true");
        signed.add_param(
            "partial-results-stability",
            match settings.results_stability {
                AwsTranscriberResultStability::High => "high",
                AwsTranscriberResultStability::Medium => "medium",
                AwsTranscriberResultStability::Low => "low",
            },
        );

        drop(settings);
        drop(state);

        let url =
            signed.generate_presigned_url(&creds, &std::time::Duration::from_secs(5 * 60), true);

        let (ws, _) = {
            let _enter = RUNTIME.enter();
            futures::executor::block_on(connect_async(format!("wss{}", &url[5..]))).map_err(
                |err| {
                    gst_error!(CAT, obj: element, "Failed to connect: {}", err);
                    error_msg!(gst::CoreError::Failed, ["Failed to connect: {}", err])
                },
            )?
        };

        let (ws_sink, mut ws_stream) = ws.split();

        *self.ws_sink.borrow_mut() = Some(Box::pin(ws_sink));

        let element_weak = element.downgrade();
        let future = async move {
            while let Some(element) = element_weak.upgrade() {
                let transcribe = element.imp();
                let msg = match ws_stream.next().await {
                    Some(msg) => msg,
                    None => {
                        let mut state = transcribe.state.lock().unwrap();
                        state.send_eos = true;
                        break;
                    }
                };

                let msg = match msg {
                    Ok(msg) => msg,
                    Err(err) => {
                        gst_error!(CAT, "Failed to receive data: {}", err);
                        element_error!(
                            element,
                            gst::StreamError::Failed,
                            ["Streaming failed: {}", err]
                        );
                        break;
                    }
                };

                let mut sender = transcribe.state.lock().unwrap().sender.clone();

                if let Some(sender) = sender.as_mut() {
                    if sender.send(msg).await.is_err() {
                        break;
                    }
                }
            }
        };

        let mut state = self.state.lock().unwrap();

        let (future, abort_handle) = abortable(future);

        state.recv_abort_handle = Some(abort_handle);

        RUNTIME.spawn(future);

        state.connected = true;

        gst_info!(CAT, obj: element, "Connected");

        Ok(())
    }

    fn disconnect(&self, element: &super::Transcriber) {
        let mut state = self.state.lock().unwrap();

        gst_info!(CAT, obj: element, "Unpreparing");

        if let Some(abort_handle) = state.recv_abort_handle.take() {
            abort_handle.abort();
        }

        if let Some(abort_handle) = state.send_abort_handle.take() {
            abort_handle.abort();
        }

        *state = State::default();

        gst_info!(
            CAT,
            obj: element,
            "Unprepared, connected: {}!",
            state.connected
        );
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Transcriber {
    const NAME: &'static str = "RsAwsTranscriber";
    type Type = super::Transcriber;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |transcriber, element| transcriber.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber, element| transcriber.sink_event(pad, element, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .activatemode_function(|pad, parent, mode, active| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || Err(loggable_error!(CAT, "Panic activating src pad with mode")),
                    |transcriber, element| transcriber.src_activatemode(pad, element, mode, active),
                )
            })
            .query_function(|pad, parent, query| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber, element| transcriber.src_query(pad, element, query),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        let settings = Mutex::new(Settings::default());

        Self {
            srcpad,
            sinkpad,
            settings,
            state: Mutex::new(State::default()),
            ws_sink: AtomicRefCell::new(None),
        }
    }
}

impl ObjectImpl for Transcriber {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::new(
                    "language-code",
                    "Language Code",
                    "The Language of the Stream, see \
                        <https://docs.aws.amazon.com/transcribe/latest/dg/how-streaming-transcription.html> \
                        for an up to date list of allowed languages",
                    Some("en-US"),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt::new(
                    "latency",
                    "Latency",
                    "Amount of milliseconds to allow AWS transcribe",
                    0,
                    std::u32::MAX,
                    DEFAULT_LATENCY.mseconds() as u32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt::new(
                    "lateness",
                    "Lateness",
                    "Amount of milliseconds to introduce as lateness",
                    0,
                    std::u32::MAX,
                    DEFAULT_LATENESS.mseconds() as u32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "vocabulary-name",
                    "Vocabulary Name",
                    "The name of a custom vocabulary, see \
                        <https://docs.aws.amazon.com/transcribe/latest/dg/how-vocabulary.html> \
                        for more information",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "session-id",
                    "Session ID",
                    "The ID of the transcription session, must be length 36",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecEnum::new(
                    "results-stability",
                    "Results stability",
                    "Defines how fast results should stabilize",
                    AwsTranscriberResultStability::static_type(),
                    DEFAULT_STABILITY as i32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "access-key",
                    "Access Key",
                    "AWS Access Key",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "secret-access-key",
                    "Secret Access Key",
                    "AWS Secret Access Key",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
        obj.set_element_flags(gst::ElementFlags::PROVIDE_CLOCK | gst::ElementFlags::REQUIRE_CLOCK);
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "language_code" => {
                let mut settings = self.settings.lock().unwrap();
                settings.language_code = value.get().expect("type checked upstream");
            }
            "latency" => {
                let mut settings = self.settings.lock().unwrap();
                settings.latency = gst::ClockTime::from_mseconds(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            "lateness" => {
                let mut settings = self.settings.lock().unwrap();
                settings.lateness = gst::ClockTime::from_mseconds(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            "vocabulary-name" => {
                let mut settings = self.settings.lock().unwrap();
                settings.vocabulary = value.get().expect("type checked upstream");
            }
            "session-id" => {
                let mut settings = self.settings.lock().unwrap();
                settings.session_id = value.get().expect("type checked upstream");
            }
            "results-stability" => {
                let mut settings = self.settings.lock().unwrap();
                settings.results_stability = value
                    .get::<AwsTranscriberResultStability>()
                    .expect("type checked upstream");
            }
            "access-key" => {
                let mut settings = self.settings.lock().unwrap();
                settings.access_key = value.get().expect("type checked upstream");
            }
            "secret-access-key" => {
                let mut settings = self.settings.lock().unwrap();
                settings.secret_access_key = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "language-code" => {
                let settings = self.settings.lock().unwrap();
                settings.language_code.to_value()
            }
            "latency" => {
                let settings = self.settings.lock().unwrap();
                (settings.latency.mseconds() as u32).to_value()
            }
            "lateness" => {
                let settings = self.settings.lock().unwrap();
                (settings.lateness.mseconds() as u32).to_value()
            }
            "vocabulary-name" => {
                let settings = self.settings.lock().unwrap();
                settings.vocabulary.to_value()
            }
            "session-id" => {
                let settings = self.settings.lock().unwrap();
                settings.session_id.to_value()
            }
            "results-stability" => {
                let settings = self.settings.lock().unwrap();
                settings.results_stability.to_value()
            }
            "access-key" => {
                let settings = self.settings.lock().unwrap();
                settings.access_key.to_value()
            }
            "secret-access-key" => {
                let settings = self.settings.lock().unwrap();
                settings.secret_access_key.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Transcriber {}

impl ElementImpl for Transcriber {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
            "Transcriber",
            "Audio/Text/Filter",
            "Speech to Text filter, using AWS transcribe",
            "Jordan Petridis <jordan@centricular.com>, Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_caps = gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            let sink_caps = gst::Caps::builder("audio/x-raw")
                .field("format", "S16LE")
                .field("rate", gst::IntRange::new(8000i32, 48000))
                .field("channels", 1)
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_info!(CAT, obj: element, "Changing state {:?}", transition);

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                self.disconnect(element);
            }
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

    fn provide_clock(&self, _element: &Self::Type) -> Option<gst::Clock> {
        Some(gst::SystemClock::obtain())
    }
}
