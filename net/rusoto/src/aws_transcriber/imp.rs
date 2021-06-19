// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{
    element_error, error_msg, gst_debug, gst_error, gst_info, gst_log, gst_trace, loggable_error,
};

use std::default::Default;

use rusoto_core::Region;
use rusoto_credential::{ChainProvider, ProvideAwsCredentials};

use rusoto_signature::signature::SignedRequest;

use async_tungstenite::tungstenite::error::Error as WsError;
use async_tungstenite::{tokio::connect_async, tungstenite::Message};
use futures::channel::mpsc;
use futures::future::{abortable, AbortHandle};
use futures::prelude::*;
use tokio::runtime;

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
const DEFAULT_STABILITY: AwsTranscriberResultStability = AwsTranscriberResultStability::Low;
const GRANULARITY: gst::ClockTime = gst::ClockTime::from_mseconds(100);

#[derive(Debug, Clone)]
struct Settings {
    latency: gst::ClockTime,
    language_code: Option<String>,
    vocabulary: Option<String>,
    session_id: Option<String>,
    results_stability: AwsTranscriberResultStability,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            latency: DEFAULT_LATENCY,
            language_code: Some("en-US".to_string()),
            vocabulary: None,
            session_id: None,
            results_stability: DEFAULT_STABILITY,
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

        let (latency, now, mut last_position, send_eos, seqnum) = {
            let mut state = self.state.lock().unwrap();

            // Multiply GRANULARITY by 2 in order to not send buffers that
            // are less than GRANULARITY away too late
            let latency = self.settings.lock().unwrap().latency - 2 * GRANULARITY;
            let now = element.current_running_time();

            let send_eos = state.send_eos && state.buffers.is_empty();

            while let Some(buf) = state.buffers.front() {
                if now
                    .zip(buf.pts())
                    .map_or(false, |(now, pts)| now - pts > latency)
                {
                    /* Safe unwrap, we know we have an item */
                    let buf = state.buffers.pop_front().unwrap();
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
            let _ = self.srcpad.pause_task();

            return self
                .srcpad
                .push_event(gst::event::Eos::builder().seqnum(seqnum).build());
        }

        for mut buf in items.drain(..) {
            let delta = buf
                .pts()
                .zip(last_position)
                .map(|(pts, last_pos)| pts.checked_sub(last_pos));
            if let Some(delta) = delta {
                let last_pos = last_position.expect("defined since delta could be computed");

                let gap_event = gst::event::Gap::builder(last_pos)
                    .duration(delta)
                    .seqnum(seqnum)
                    .build();
                gst_log!(
                    CAT,
                    "Pushing gap:    {} -> {}",
                    last_pos,
                    buf.pts().display()
                );
                if !self.srcpad.push_event(gap_event) {
                    return false;
                }
            }
            last_position = buf
                .pts()
                .zip(buf.duration())
                .map(|(pts, duration)| pts + duration);
            {
                let buf = buf.get_mut().unwrap();
                buf.set_pts(buf.pts());
            }
            gst_log!(
                CAT,
                "Pushing buffer: {} -> {}",
                buf.pts().display(),
                buf.pts()
                    .zip(buf.duration())
                    .map(|(pts, duration)| pts + duration)
                    .display(),
            );
            if self.srcpad.push(buf).is_err() {
                return false;
            }
        }

        /* next, push a gap if we're lagging behind the target position */

        let duration = now
            .zip(last_position)
            .and_then(|(now, last_position)| now.checked_sub(last_position))
            .and_then(|delta| delta.checked_sub(latency));
        if let Some(duration) = duration {
            let last_pos = last_position.expect("defined since duration could be computed");
            let gap_event = gst::event::Gap::builder(last_pos)
                .duration(duration)
                .seqnum(seqnum)
                .build();
            let next_position = last_pos + duration;
            gst_log!(CAT, "Pushing gap:    {} -> {}", last_pos, next_position,);
            last_position = Some(next_position);
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
        for item in &alternative.items[state.partial_index..] {
            let mut start_time =
                gst::ClockTime::from_nseconds((item.start_time as f64 * 1_000_000_000.0) as u64);
            let mut end_time =
                gst::ClockTime::from_nseconds((item.end_time as f64 * 1_000_000_000.0) as u64);

            if !item.stable {
                break;
            }

            /* Should be sent now */
            gst_debug!(CAT, obj: element, "Item is ready: {}", item.content);
            let mut buf = gst::Buffer::from_mut_slice(item.content.clone().into_bytes());

            {
                let buf = buf.get_mut().unwrap();

                if state.discont {
                    buf.set_flags(gst::BufferFlags::DISCONT);
                    state.discont = false;
                }

                if state
                    .out_segment
                    .position()
                    .map_or(false, |pos| start_time < pos)
                {
                    let pos = state
                        .out_segment
                        .position()
                        .expect("position checked above");
                    gst_debug!(
                        CAT,
                        obj: element,
                        "Adjusting item timing({} < {})",
                        start_time,
                        pos,
                    );
                    start_time = pos;
                    if end_time < start_time {
                        end_time = start_time;
                    }
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
                            serde_json::from_str(&payload).map_err(|err| {
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

                    let transcript: Transcript = serde_json::from_str(&payload).map_err(|err| {
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

            let transcribe = Self::from_instance(&element);
            match transcribe.loop_fn(&element, &mut receiver) {
                Err(err) => {
                    element_error!(
                        &element,
                        gst::StreamError::Failed,
                        ["Streaming failed: {}", err]
                    );
                    let _ = transcribe.srcpad.pause_task();
                }
                Ok(_) => (),
            };
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
                self.disconnect(element);
                let mut ret = pad.event_default(Some(element), event);

                match self.srcpad.stop_task() {
                    Err(err) => {
                        gst_error!(CAT, obj: element, "Failed to stop srcpad task: {}", err);
                        ret = false;
                    }
                    Ok(_) => (),
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

                let event = {
                    let mut state = self.state.lock().unwrap();

                    state.out_segment.set_time(segment.time());
                    state.out_segment.set_position(gst::ClockTime::ZERO);

                    state.in_segment = segment;
                    state.seqnum = e.seqnum();
                    gst::event::Segment::builder(&state.out_segment)
                        .seqnum(state.seqnum)
                        .build()
                };

                gst_info!(CAT, "Sending our own segment: {:?}", event);

                pad.event_default(Some(element), event)
            }
            EventView::Tag(_) => true,
            EventView::Caps(e) => {
                gst_info!(CAT, "Received caps {:?}", e);

                let caps = gst::Caps::builder("text/x-raw")
                    .field("format", &"utf8")
                    .build();
                let seqnum = self.state.lock().unwrap().seqnum;
                self.srcpad
                    .push_event(gst::event::Caps::builder(&caps).seqnum(seqnum).build())
            }
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

                delay = running_time
                    .zip(now)
                    .and_then(|(running_time, now)| running_time.checked_sub(now));
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
                &element,
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
        let mut state = self.state.lock().unwrap();

        if state.connected {
            return Ok(());
        }

        let in_caps = self.sinkpad.current_caps().unwrap();
        let s = in_caps.structure(0).unwrap();
        let sample_rate = s.get::<i32>("rate").unwrap();

        let settings = self.settings.lock().unwrap();

        gst_info!(CAT, obj: element, "Connecting ..");

        let creds = {
            let _enter = RUNTIME.enter();
            futures::executor::block_on(ChainProvider::new().credentials()).map_err(|err| {
                gst_error!(CAT, obj: element, "Failed to generate credentials: {}", err);
                error_msg!(
                    gst::CoreError::Failed,
                    ["Failed to generate credentials: {}", err]
                )
            })?
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

        let url = signed.generate_presigned_url(&creds, &std::time::Duration::from_secs(60), true);

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
                let transcribe = Self::from_instance(&element);
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
                glib::ParamSpec::new_string(
                    "language-code",
                    "Language Code",
                    "The Language of the Stream, see \
                        <https://docs.aws.amazon.com/transcribe/latest/dg/how-streaming-transcription.html> \
                        for an up to date list of allowed languages",
                    Some("en-US"),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_uint(
                    "latency",
                    "Latency",
                    "Amount of milliseconds to allow AWS transcribe",
                    2 * GRANULARITY.mseconds() as u32,
                    std::u32::MAX,
                    DEFAULT_LATENCY.mseconds() as u32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_string(
                    "vocabulary-name",
                    "Vocabulary Name",
                    "The name of a custom vocabulary, see \
                        <https://docs.aws.amazon.com/transcribe/latest/dg/how-vocabulary.html> \
                        for more information",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_string(
                    "session-id",
                    "Session ID",
                    "The ID of the transcription session, must be length 36",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_enum(
                    "results-stability",
                    "Results stability",
                    "Defines how fast results should stabilize",
                    AwsTranscriberResultStability::static_type(),
                    DEFAULT_STABILITY as i32,
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
            _ => unimplemented!(),
        }
    }
}

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
                .field("format", &"utf8")
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            let sink_caps = gst::Caps::builder("audio/x-raw")
                .field("format", &"S16LE")
                .field("rate", &gst::IntRange::<i32>::new(8000, 48000))
                .field("channels", &1)
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

        match transition {
            gst::StateChange::PausedToReady => {
                self.disconnect(element);
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

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

    fn provide_clock(&self, _element: &Self::Type) -> Option<gst::Clock> {
        Some(gst::SystemClock::obtain())
    }
}
