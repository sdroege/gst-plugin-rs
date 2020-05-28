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

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::default::Default;

use rusoto_core::Region;
use rusoto_credential::{EnvironmentProvider, ProvideAwsCredentials};

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
use std::time::Duration;

use crate::packet::*;

use serde_derive::Deserialize;

use once_cell::sync::Lazy;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct TranscriptItem {
    content: String,
    end_time: f32,
    start_time: f32,
    #[serde(rename = "Type")]
    type_: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct TranscriptAlternative {
    items: Vec<TranscriptItem>,
    transcript: String,
}

#[derive(Deserialize, Debug)]
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
    runtime::Builder::new()
        .threaded_scheduler()
        .enable_all()
        .core_threads(1)
        .build()
        .unwrap()
});

const DEFAULT_LATENCY_MS: u32 = 8000;
const DEFAULT_USE_PARTIAL_RESULTS: bool = true;
const GRANULARITY_MS: u32 = 100;

static PROPERTIES: [subclass::Property; 3] = [
    subclass::Property("language-code", |name| {
        glib::ParamSpec::string(
            name,
            "Language Code",
            "The Language of the Stream, see \
                <https://docs.aws.amazon.com/transcribe/latest/dg/how-streaming-transcription.html> \
                for an up to date list of allowed languages",
            Some("en-US"),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("use-partial-results", |name| {
        glib::ParamSpec::boolean(
            name,
            "Latency",
            "Whether partial results from AWS should be used",
            DEFAULT_USE_PARTIAL_RESULTS,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("latency", |name| {
        glib::ParamSpec::uint(
            name,
            "Latency",
            "Amount of milliseconds to allow AWS transcribe",
            2 * GRANULARITY_MS,
            std::u32::MAX,
            DEFAULT_LATENCY_MS,
            glib::ParamFlags::READWRITE,
        )
    }),
];

#[derive(Debug, Clone)]
struct Settings {
    latency_ms: u32,
    language_code: Option<String>,
    use_partial_results: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            latency_ms: DEFAULT_LATENCY_MS,
            language_code: Some("en-US".to_string()),
            use_partial_results: DEFAULT_USE_PARTIAL_RESULTS,
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
    last_partial_end_time: gst::ClockTime,
    partial_alternative: Option<TranscriptAlternative>,
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
            last_partial_end_time: gst::CLOCK_TIME_NONE,
            partial_alternative: None,
        }
    }
}

type WsSink = Pin<Box<dyn Sink<Message, Error = WsError> + Send>>;

struct Transcriber {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    ws_sink: Mutex<Option<WsSink>>,
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
    fn set_pad_functions(sinkpad: &gst::Pad, srcpad: &gst::Pad) {
        sinkpad.set_chain_function(|pad, parent, buffer| {
            Transcriber::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |transcriber, element| transcriber.sink_chain(pad, element, buffer),
            )
        });
        sinkpad.set_event_function(|pad, parent, event| {
            Transcriber::catch_panic_pad_function(
                parent,
                || false,
                |transcriber, element| transcriber.sink_event(pad, element, event),
            )
        });

        srcpad.set_activatemode_function(|pad, parent, mode, active| {
            Transcriber::catch_panic_pad_function(
                parent,
                || {
                    Err(gst_loggable_error!(
                        CAT,
                        "Panic activating src pad with mode"
                    ))
                },
                |transcriber, element| transcriber.src_activatemode(pad, element, mode, active),
            )
        });
        srcpad.set_query_function(|pad, parent, query| {
            Transcriber::catch_panic_pad_function(
                parent,
                || false,
                |transcriber, element| transcriber.src_query(pad, element, query),
            )
        });
    }

    fn dequeue(&self, element: &gst::Element) -> bool {
        /* First, check our pending buffers */
        let mut items = vec![];

        let (latency, now, mut last_position, send_eos, seqnum) = {
            let mut state = self.state.lock().unwrap();
            // Multiply GRANULARITY by 2 in order to not send buffers that
            // are less than GRANULARITY milliseconds away too late
            let latency: gst::ClockTime = (self.settings.lock().unwrap().latency_ms as u64
                - (2 * GRANULARITY_MS) as u64)
                * gst::MSECOND;
            let now = element.get_current_running_time();

            if let Some(alternative) = state.partial_alternative.take() {
                self.enqueue(element, &mut state, &alternative, true, latency, now);
                state.partial_alternative = Some(alternative);
            }
            let send_eos = state.send_eos && state.buffers.is_empty();

            while let Some(buf) = state.buffers.front() {
                if now - buf.get_pts() > latency {
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
                state.out_segment.get_position(),
                send_eos,
                state.seqnum,
            )
        };

        /* We're EOS, we can pause and exit early */
        if send_eos {
            let _ = self.srcpad.pause_task();

            return self
                .srcpad
                .push_event(gst::Event::new_eos().seqnum(seqnum).build());
        }

        for mut buf in items.drain(..) {
            if buf.get_pts() > last_position {
                let gap_event = gst::Event::new_gap(last_position, buf.get_pts() - last_position)
                    .seqnum(seqnum)
                    .build();
                gst_debug!(
                    CAT,
                    "Pushing gap:    {} -> {}",
                    last_position,
                    buf.get_pts()
                );
                if !self.srcpad.push_event(gap_event) {
                    return false;
                }
            }
            last_position = buf.get_pts() + buf.get_duration();
            {
                let buf = buf.get_mut().unwrap();
                buf.set_pts(buf.get_pts());
            }
            gst_debug!(
                CAT,
                "Pushing buffer: {} -> {}",
                buf.get_pts(),
                buf.get_pts() + buf.get_duration()
            );
            if self.srcpad.push(buf).is_err() {
                return false;
            }
        }

        /* next, push a gap if we're lagging behind the target position */

        if now - last_position > latency {
            let duration = now - last_position - latency;

            let gap_event = gst::Event::new_gap(last_position, duration)
                .seqnum(seqnum)
                .build();
            gst_debug!(
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
        element: &gst::Element,
        state: &mut State,
        alternative: &TranscriptAlternative,
        partial: bool,
        latency: gst::ClockTime,
        now: gst::ClockTime,
    ) {
        for item in &alternative.items {
            let mut start_time: gst::ClockTime =
                ((item.start_time as f64 * 1_000_000_000.0) as u64).into();
            let mut end_time: gst::ClockTime =
                ((item.end_time as f64 * 1_000_000_000.0) as u64).into();

            if start_time <= state.last_partial_end_time {
                /* Already sent (hopefully) */
                continue;
            } else if !partial || start_time + latency < now {
                /* Should be sent now */
                gst_debug!(CAT, obj: element, "Item is ready: {}", item.content);
                let mut buf = gst::Buffer::from_mut_slice(item.content.clone().into_bytes());
                state.last_partial_end_time = end_time;

                {
                    let buf = buf.get_mut().unwrap();

                    if state.discont {
                        buf.set_flags(gst::BufferFlags::DISCONT);
                        state.discont = false;
                    }

                    if start_time < state.out_segment.get_position() {
                        gst_debug!(
                            CAT,
                            obj: element,
                            "Adjusting item timing({:?} < {:?})",
                            start_time,
                            state.out_segment.get_position()
                        );
                        start_time = state.out_segment.get_position();
                        if end_time < start_time {
                            end_time = start_time;
                        }
                    }

                    buf.set_pts(start_time);
                    buf.set_duration(end_time - start_time);
                }

                state.buffers.push_back(buf);
            } else {
                /* Doesn't need to be sent yet */
                break;
            }
        }
    }

    fn loop_fn(
        &self,
        element: &gst::Element,
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
                        gst_error_msg!(
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
                                gst_error_msg!(
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

                        return Err(gst_error_msg!(
                            gst::StreamError::Failed,
                            ["AWS raised an error: {}", message.message]
                        ));
                    }

                    let mut transcript: Transcript =
                        serde_json::from_str(&payload).map_err(|err| {
                            gst_error_msg!(
                                gst::StreamError::Failed,
                                ["Unexpected binary message: {} ({})", payload, err]
                            )
                        })?;

                    if !transcript.transcript.results.is_empty() {
                        let mut result = transcript.transcript.results.remove(0);
                        let use_partial_results = self.settings.lock().unwrap().use_partial_results;
                        if !result.is_partial && !result.alternatives.is_empty() {
                            if !use_partial_results {
                                let alternative = result.alternatives.remove(0);
                                gst_info!(
                                    CAT,
                                    obj: element,
                                    "Transcript: {}",
                                    alternative.transcript
                                );

                                let mut start_time: gst::ClockTime =
                                    ((result.start_time as f64 * 1_000_000_000.0) as u64).into();
                                let end_time: gst::ClockTime =
                                    ((result.end_time as f64 * 1_000_000_000.0) as u64).into();

                                let mut state = self.state.lock().unwrap();
                                let position = state.out_segment.get_position();

                                if end_time < position {
                                    gst_warning!(CAT, obj: element,
                                                 "Received transcript is too late by {:?}, dropping, consider increasing the latency",
                                                 position - start_time);
                                } else {
                                    if start_time < position {
                                        gst_warning!(CAT, obj: element,
                                                     "Received transcript is too late by {:?}, clipping, consider increasing the latency",
                                                     position - start_time);
                                        start_time = position;
                                    }

                                    let mut buf = gst::Buffer::from_mut_slice(
                                        alternative.transcript.into_bytes(),
                                    );

                                    {
                                        let buf = buf.get_mut().unwrap();

                                        if state.discont {
                                            buf.set_flags(gst::BufferFlags::DISCONT);
                                            state.discont = false;
                                        }

                                        buf.set_pts(start_time);
                                        buf.set_duration(end_time - start_time);
                                    }

                                    gst_debug!(
                                        CAT,
                                        obj: element,
                                        "Adding pending buffer: {:?}",
                                        buf
                                    );

                                    state.buffers.push_back(buf);
                                }
                            } else {
                                let alternative = result.alternatives.remove(0);
                                let mut state = self.state.lock().unwrap();
                                self.enqueue(
                                    element,
                                    &mut state,
                                    &alternative,
                                    false,
                                    0.into(),
                                    0.into(),
                                );
                                state.partial_alternative = None;
                            }
                        } else if !result.alternatives.is_empty() && use_partial_results {
                            let mut state = self.state.lock().unwrap();
                            state.partial_alternative = Some(result.alternatives.remove(0));
                        }
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

        RUNTIME.enter(|| futures::executor::block_on(future))
    }

    fn start_task(&self, element: &gst::Element) -> Result<(), gst::LoggableError> {
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
                    gst_element_error!(
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
            return Err(gst_loggable_error!(CAT, "Failed to start pad task"));
        }
        Ok(())
    }

    fn src_activatemode(
        &self,
        _pad: &gst::Pad,
        element: &gst::Element,
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

    fn src_query(&self, pad: &gst::Pad, element: &gst::Element, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                let mut peer_query = gst::query::Query::new_latency();

                let ret = self.sinkpad.peer_query(&mut peer_query);

                if ret {
                    let (_, min, _) = peer_query.get_result();
                    let our_latency: gst::ClockTime =
                        self.settings.lock().unwrap().latency_ms as u64 * gst::MSECOND;
                    q.set(true, our_latency + min, gst::CLOCK_TIME_NONE);
                }
                ret
            }
            QueryView::Position(ref mut q) => {
                if q.get_format() == gst::Format::Time {
                    let state = self.state.lock().unwrap();
                    q.set(
                        state
                            .out_segment
                            .to_stream_time(state.out_segment.get_position()),
                    );
                    true
                } else {
                    false
                }
            }
            _ => pad.query_default(Some(element), query),
        }
    }

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_debug!(CAT, obj: pad, "Handling event {:?}", event);

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
                match self.disconnect(element) {
                    Err(err) => {
                        element.post_error_message(&err);
                        false
                    }
                    Ok(_) => {
                        let mut ret = pad.event_default(Some(element), event);

                        match self.srcpad.stop_task() {
                            Err(err) => {
                                gst_error!(
                                    CAT,
                                    obj: element,
                                    "Failed to stop srcpad task: {}",
                                    err
                                );
                                ret = false;
                            }
                            Ok(_) => (),
                        };

                        ret
                    }
                }
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
                let segment = match e.get_segment().clone().downcast::<gst::ClockTime>() {
                    Err(segment) => {
                        gst_element_error!(
                            element,
                            gst::StreamError::Format,
                            [
                                "Only Time segments supported, got {:?}",
                                segment.get_format(),
                            ]
                        );
                        return false;
                    }
                    Ok(segment) => segment,
                };

                let event = {
                    let mut state = self.state.lock().unwrap();

                    state.out_segment.set_time(segment.get_time());
                    state
                        .out_segment
                        .set_position(gst::ClockTime::from_nseconds(0));

                    state.in_segment = segment;
                    state.seqnum = e.get_seqnum();
                    gst::Event::new_segment(&state.out_segment)
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
                    .push_event(gst::Event::new_caps(&caps).seqnum(seqnum).build())
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    async fn sync_and_send(
        &self,
        element: &gst::Element,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut delay = None;

        {
            let state = self.state.lock().unwrap();

            if let Some(buffer) = &buffer {
                let running_time = state.in_segment.to_running_time(buffer.get_pts());
                let now = element.get_current_running_time();

                if now.is_some() && now < running_time {
                    delay = Some(running_time - now);
                }
            }
        }

        if let Some(delay) = delay {
            tokio::time::delay_for(Duration::from_nanos(delay.nseconds().unwrap())).await;
        }

        if let Some(ws_sink) = self.ws_sink.lock().unwrap().as_mut() {
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
        element: &gst::Element,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_debug!(CAT, obj: element, "Handling {:?}", buffer);

        self.ensure_connection(element).map_err(|err| {
            gst_element_error!(
                &element,
                gst::StreamError::Failed,
                ["Streaming failed: {}", err]
            );
            gst::FlowError::Error
        })?;

        let (future, abort_handle) = abortable(self.sync_and_send(element, buffer));

        self.state.lock().unwrap().send_abort_handle = Some(abort_handle);

        match RUNTIME.enter(|| futures::executor::block_on(future)) {
            Err(_) => Err(gst::FlowError::Flushing),
            Ok(res) => res,
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.handle_buffer(pad, element, Some(buffer))
    }

    fn ensure_connection(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        if state.connected {
            return Ok(());
        }

        let in_caps = self.sinkpad.get_current_caps().unwrap();
        let s = in_caps.get_structure(0).unwrap();
        let sample_rate: i32 = s.get("rate").unwrap().unwrap();

        let settings = self.settings.lock().unwrap();

        gst_info!(CAT, obj: element, "Connecting ..");

        let creds = RUNTIME
            .enter(|| futures::executor::block_on(EnvironmentProvider::default().credentials()))
            .map_err(|err| {
                gst_error!(CAT, obj: element, "Failed to generate credentials: {}", err);
                gst_error_msg!(
                    gst::CoreError::Failed,
                    ["Failed to generate credentials: {}", err]
                )
            })?;

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
        let url = signed.generate_presigned_url(&creds, &std::time::Duration::from_secs(60), true);

        let (ws, _) = RUNTIME
            .enter(|| futures::executor::block_on(connect_async(format!("wss{}", &url[5..]))))
            .map_err(|err| {
                gst_error!(CAT, obj: element, "Failed to connect: {}", err);
                gst_error_msg!(gst::CoreError::Failed, ["Failed to connect: {}", err])
            })?;

        let (ws_sink, mut ws_stream) = ws.split();

        *self.ws_sink.lock().unwrap() = Some(Box::pin(ws_sink));

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
                        gst_element_error!(
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

    fn disconnect(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
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

        Ok(())
    }
}

impl ObjectSubclass for Transcriber {
    const NAME: &'static str = "RsAwsTranscriber";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, Some("sink"));
        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::new_from_template(&templ, Some("src"));

        srcpad.use_fixed_caps();

        Transcriber::set_pad_functions(&sinkpad, &srcpad);
        let settings = Mutex::new(Settings::default());

        Self {
            srcpad,
            sinkpad,
            settings,
            state: Mutex::new(State::default()),
            ws_sink: Mutex::new(None),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Transcriber",
            "Audio/Text/Filter",
            "Speech to Text filter, using AWS transcribe",
            "Jordan Petridis <jordan@centricular.com>, Mathieu Duponchelle <mathieu@centricular.com>",
        );

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
        klass.add_pad_template(src_pad_template);

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
        klass.add_pad_template(sink_pad_template);
        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for Transcriber {
    glib_object_impl!();

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sinkpad).unwrap();
        element.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("language_code", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.language_code = value.get().expect("type checked upstream");
            }
            subclass::Property("latency", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.latency_ms = value.get_some().expect("type checked upstream");
            }
            subclass::Property("use-partial-results", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.use_partial_results = value.get_some().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("language-code", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.language_code.to_value())
            }
            subclass::Property("latency", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.latency_ms.to_value())
            }
            subclass::Property("use-partial-results", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.use_partial_results.to_value())
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for Transcriber {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_info!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::PausedToReady => {
                self.disconnect(element).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
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
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "awstranscriber",
        gst::Rank::None,
        Transcriber::get_type(),
    )
}
