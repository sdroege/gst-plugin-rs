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
use gst::{element_imp_error, error_msg, loggable_error};

use std::default::Default;

use aws_config::default_provider::credentials::DefaultCredentialsChain;
use aws_sig_auth::signer::{self, HttpSignatureType, OperationSigningConfig, RequestConfig};
use aws_smithy_http::body::SdkBody;
use aws_types::credentials::ProvideCredentials;
use aws_types::region::{Region, SigningRegion};
use aws_types::{Credentials, SigningService};
use std::time::{Duration, SystemTime};

use chrono::prelude::*;
use http::Uri;

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

use super::{AwsTranscriberResultStability, AwsTranscriberVocabularyFilterMethod};

const DEFAULT_TRANSCRIBER_REGION: &str = "us-east-1";

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
const DEFAULT_LATENESS: gst::ClockTime = gst::ClockTime::ZERO;
const DEFAULT_STABILITY: AwsTranscriberResultStability = AwsTranscriberResultStability::Low;
const DEFAULT_VOCABULARY_FILTER_METHOD: AwsTranscriberVocabularyFilterMethod =
    AwsTranscriberVocabularyFilterMethod::Mask;
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
    session_token: Option<String>,
    vocabulary_filter: Option<String>,
    vocabulary_filter_method: AwsTranscriberVocabularyFilterMethod,
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
            session_token: None,
            vocabulary_filter: None,
            vocabulary_filter_method: DEFAULT_VOCABULARY_FILTER_METHOD,
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
    fn dequeue(&self) -> bool {
        /* First, check our pending buffers */
        let mut items = vec![];

        let now = match self.instance().current_running_time() {
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
            gst::trace!(
                CAT,
                imp: self,
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
                    gst::log!(CAT, "Pushing gap:    {} -> {}", last_position, pts);
                    if !self.srcpad.push_event(gap_event) {
                        return false;
                    }
                }
                Ordering::Less => {
                    let delta = last_position - pts;

                    gst::warning!(
                        CAT,
                        imp: self,
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

            gst::debug!(CAT, "Pushing buffer: {} -> {}", pts, pts + duration);

            if self.srcpad.push(buf).is_err() {
                return false;
            }
        }

        /* next, push a gap if we're lagging behind the target position */
        gst::trace!(
            CAT,
            imp: self,
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

            gst::log!(
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

    fn enqueue(&self, state: &mut State, alternative: &TranscriptAlternative, partial: bool) {
        let lateness = self.settings.lock().unwrap().lateness;

        if alternative.items.len() <= state.partial_index {
            gst::error!(
                CAT,
                imp: self,
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
                ((item.start_time as f64 * 1_000_000_000.0) as u64).nseconds() + lateness;
            let end_time = ((item.end_time as f64 * 1_000_000_000.0) as u64).nseconds() + lateness;

            if !item.stable {
                break;
            }

            /* Should be sent now */
            gst::debug!(
                CAT,
                imp: self,
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

    fn loop_fn(&self, receiver: &mut mpsc::Receiver<Message>) -> Result<(), gst::ErrorMessage> {
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
            gst::info!(CAT, imp: self, "Sending {:?}", event);
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
                        gst::error!(CAT, imp: self, "Failed to parse packet: {}", err);
                        error_msg!(
                            gst::StreamError::Failed,
                            ["Failed to parse packet: {}", err]
                        )
                    })?;

                    let payload = std::str::from_utf8(pkt.payload).unwrap();

                    if packet_is_exception(&pkt) {
                        let message: ExceptionMessage =
                            serde_json::from_str(payload).map_err(|err| {
                                gst::error!(
                                    CAT,
                                    imp: self,
                                    "Unexpected exception message: {} ({})",
                                    payload,
                                    err
                                );
                                error_msg!(
                                    gst::StreamError::Failed,
                                    ["Unexpected exception message: {} ({})", payload, err]
                                )
                            })?;
                        gst::error!(CAT, imp: self, "AWS raised an error: {}", message.message);

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
                        gst::trace!(
                            CAT,
                            imp: self,
                            "result: {}",
                            serde_json::to_string_pretty(&result).unwrap(),
                        );

                        if let Some(alternative) = result.alternatives.get(0) {
                            let mut state = self.state.lock().unwrap();

                            self.enqueue(&mut state, alternative, result.is_partial)
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
                    if !self.dequeue() {
                        gst::info!(CAT, imp: self, "Failed to push gap event, pausing");

                        let _ = self.srcpad.pause_task();
                    }
                    Ok(())
                }
                Ok(res) => {
                    if !self.dequeue() {
                        gst::info!(CAT, imp: self, "Failed to push gap event, pausing");

                        let _ = self.srcpad.pause_task();
                    }
                    res
                }
            }
        };

        let _enter = RUNTIME.enter();
        futures::executor::block_on(future)
    }

    fn start_task(&self) -> Result<(), gst::LoggableError> {
        let (sender, mut receiver) = mpsc::channel(1);

        {
            let mut state = self.state.lock().unwrap();
            state.sender = Some(sender);
        }

        let imp = self.ref_counted();
        let res = self.srcpad.start_task(move || {
            if let Err(err) = imp.loop_fn(&mut receiver) {
                element_imp_error!(imp, gst::StreamError::Failed, ["Streaming failed: {}", err]);
                let _ = imp.srcpad.pause_task();
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
        _mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if active {
            self.start_task()?;
        } else {
            {
                let mut state = self.state.lock().unwrap();
                state.sender = None;
            }

            let _ = self.srcpad.stop_task();
        }

        Ok(())
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        gst::log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryViewMut::Latency(q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = self.sinkpad.peer_query(&mut peer_query);

                if ret {
                    let (_, min, _) = peer_query.result();
                    let our_latency = self.settings.lock().unwrap().latency;
                    q.set(true, our_latency + min, gst::ClockTime::NONE);
                }
                ret
            }
            QueryViewMut::Position(q) => {
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
            _ => gst::Pad::query_default(pad, Some(&*self.instance()), query),
        }
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Eos(_) => match self.handle_buffer(pad, None) {
                Err(err) => {
                    gst::error!(CAT, "Failed to send EOS to AWS: {}", err);
                    false
                }
                Ok(_) => true,
            },
            EventView::FlushStart(_) => {
                gst::info!(CAT, imp: self, "Received flush start, disconnecting");
                let mut ret = gst::Pad::event_default(pad, Some(&*self.instance()), event);

                match self.srcpad.stop_task() {
                    Err(err) => {
                        gst::error!(CAT, imp: self, "Failed to stop srcpad task: {}", err);

                        self.disconnect();

                        ret = false;
                    }
                    Ok(_) => {
                        self.disconnect();
                    }
                };

                ret
            }
            EventView::FlushStop(_) => {
                gst::info!(CAT, imp: self, "Received flush stop, restarting task");

                if gst::Pad::event_default(pad, Some(&*self.instance()), event) {
                    match self.start_task() {
                        Err(err) => {
                            gst::error!(CAT, imp: self, "Failed to start srcpad task: {}", err);
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
                        element_imp_error!(
                            self,
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
                gst::info!(CAT, "Received caps {:?}", e);
                true
            }
            EventView::StreamStart(_) => true,
            _ => gst::Pad::event_default(pad, Some(&*self.instance()), event),
        }
    }

    async fn sync_and_send(
        &self,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut delay = None;

        {
            let state = self.state.lock().unwrap();

            if let Some(buffer) = &buffer {
                let running_time = state.in_segment.to_running_time(buffer.pts());
                let now = self.instance().current_running_time();

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
                        gst::error!(CAT, imp: self, "Failed sending packet: {}", err);
                        gst::FlowError::Error
                    })?;
                }
            } else {
                // EOS
                let packet = build_packet(&[]);
                ws_sink.send(Message::Binary(packet)).await.map_err(|err| {
                    gst::error!(CAT, imp: self, "Failed sending packet: {}", err);
                    gst::FlowError::Error
                })?;
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn handle_buffer(
        &self,
        _pad: &gst::Pad,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, imp: self, "Handling {:?}", buffer);

        self.ensure_connection().map_err(|err| {
            element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["Streaming failed: {}", err]
            );
            gst::FlowError::Error
        })?;

        let (future, abort_handle) = abortable(self.sync_and_send(buffer));

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
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.handle_buffer(pad, Some(buffer))
    }

    fn ensure_connection(&self) -> Result<(), gst::ErrorMessage> {
        let state = self.state.lock().unwrap();

        if state.connected {
            return Ok(());
        }

        let in_caps = self.sinkpad.current_caps().unwrap();
        let s = in_caps.structure(0).unwrap();
        let sample_rate = s.get::<i32>("rate").unwrap();

        let settings = self.settings.lock().unwrap();

        if settings.latency + settings.lateness <= 2 * GRANULARITY {
            gst::error!(
                CAT,
                imp: self,
                "latency + lateness must be greater than 200 milliseconds"
            );
            return Err(error_msg!(
                gst::LibraryError::Settings,
                ["latency + lateness must be greater than 200 milliseconds"]
            ));
        }

        gst::info!(CAT, imp: self, "Connecting ..");

        let region = Region::new(DEFAULT_TRANSCRIBER_REGION);
        let access_key = settings.access_key.as_ref();
        let secret_access_key = settings.secret_access_key.as_ref();
        let session_token = settings.session_token.clone();

        let credentials = match (access_key, secret_access_key) {
            (Some(key), Some(secret_key)) => {
                gst::debug!(
                    CAT,
                    imp: self,
                    "Using provided access and secret access key"
                );
                Ok(Credentials::new(
                    key.clone(),
                    secret_key.clone(),
                    session_token,
                    None,
                    "transcribe",
                ))
            }
            _ => {
                gst::debug!(CAT, imp: self, "Using default AWS credentials");
                let cred_future = async {
                    let cred = DefaultCredentialsChain::builder()
                        .region(region.clone())
                        .build()
                        .await;
                    cred.provide_credentials().await
                };

                RUNTIME.block_on(cred_future)
            }
        };

        if let Err(e) = credentials {
            return Err(error_msg!(
                gst::LibraryError::Settings,
                ["Failed to retrieve credentials with error {}", e]
            ));
        }

        let current_time = Utc::now();

        let mut query_params = String::from("/stream-transcription-websocket?");

        let language_code = settings
            .language_code
            .as_ref()
            .expect("Language code is required");

        query_params.push_str(
            format!(
                "language-code={}&media-encoding=pcm&sample-rate={}",
                language_code,
                &sample_rate.to_string(),
            )
            .as_str(),
        );

        if let Some(ref vocabulary) = settings.vocabulary {
            query_params.push_str(format!("&vocabulary-name={}", vocabulary).as_str());
        }

        if let Some(ref vocabulary_filter) = settings.vocabulary_filter {
            query_params
                .push_str(format!("&vocabulary-filter-name={}", vocabulary_filter).as_str());

            query_params.push_str(
                format!(
                    "&vocabulary-filter-method={}",
                    match settings.vocabulary_filter_method {
                        AwsTranscriberVocabularyFilterMethod::Mask => "mask",
                        AwsTranscriberVocabularyFilterMethod::Remove => "remove",
                        AwsTranscriberVocabularyFilterMethod::Tag => "tag",
                    }
                )
                .as_str(),
            );
        }

        if let Some(ref session_id) = settings.session_id {
            gst::debug!(CAT, imp: self, "Using session ID: {}", session_id);
            query_params.push_str(format!("&session-id={}", session_id).as_str());
        }

        query_params.push_str("&enable-partial-results-stabilization=true");

        query_params.push_str(
            format!(
                "&partial-results-stability={}",
                match settings.results_stability {
                    AwsTranscriberResultStability::High => "high",
                    AwsTranscriberResultStability::Medium => "medium",
                    AwsTranscriberResultStability::Low => "low",
                }
            )
            .as_str(),
        );

        drop(settings);
        drop(state);

        let signer = signer::SigV4Signer::new();
        let mut operation_config = OperationSigningConfig::default_config();
        operation_config.signature_type = HttpSignatureType::HttpRequestQueryParams;
        operation_config.expires_in = Some(Duration::from_secs(5 * 60)); // See commit a3db85d.

        let request_config = RequestConfig {
            request_ts: SystemTime::from(current_time),
            region: &SigningRegion::from(region.clone()),
            service: &SigningService::from_static("transcribe"),
            payload_override: None,
        };
        let transcribe_uri = Uri::builder()
            .scheme("https")
            .authority(format!("transcribestreaming.{}.amazonaws.com:8443", region).as_str())
            .path_and_query(query_params.clone())
            .build()
            .map_err(|err| {
                gst::error!(CAT, imp: self, "Failed to build HTTP request URI: {}", err);
                error_msg!(
                    gst::CoreError::Failed,
                    ["Failed to build HTTP request URI: {}", err]
                )
            })?;
        let mut request = http::Request::builder()
            .uri(transcribe_uri)
            .body(SdkBody::empty())
            .expect("Failed to build valid request");
        let _signature = signer
            .sign(
                &operation_config,
                &request_config,
                &credentials.unwrap(),
                &mut request,
            )
            .map_err(|err| {
                gst::error!(CAT, imp: self, "Failed to sign HTTP request: {}", err);
                error_msg!(
                    gst::CoreError::Failed,
                    ["Failed to sign HTTP request: {}", err]
                )
            })?;
        let url = request.uri().to_string();

        let (ws, _) = {
            let _enter = RUNTIME.enter();
            futures::executor::block_on(connect_async(format!("wss{}", &url[5..]))).map_err(
                |err| {
                    gst::error!(CAT, imp: self, "Failed to connect: {}", err);
                    error_msg!(gst::CoreError::Failed, ["Failed to connect: {}", err])
                },
            )?
        };

        let (ws_sink, mut ws_stream) = ws.split();

        *self.ws_sink.borrow_mut() = Some(Box::pin(ws_sink));

        let imp_weak = self.downgrade();
        let future = async move {
            while let Some(transcribe) = imp_weak.upgrade() {
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
                        gst::error!(CAT, imp: transcribe, "Failed to receive data: {}", err);
                        element_imp_error!(
                            transcribe,
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

        gst::info!(CAT, imp: self, "Connected");

        Ok(())
    }

    fn disconnect(&self) {
        let mut state = self.state.lock().unwrap();

        gst::info!(CAT, imp: self, "Unpreparing");

        if let Some(abort_handle) = state.recv_abort_handle.take() {
            abort_handle.abort();
        }

        if let Some(abort_handle) = state.send_abort_handle.take() {
            abort_handle.abort();
        }

        *state = State::default();

        gst::info!(
            CAT,
            imp: self,
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

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .activatemode_function(|pad, parent, mode, active| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || Err(loggable_error!(CAT, "Panic activating src pad with mode")),
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
                glib::ParamSpecString::builder("language-code")
                    .nick("Language Code")
                    .blurb("The Language of the Stream, see \
                        <https://docs.aws.amazon.com/transcribe/latest/dg/how-streaming-transcription.html> \
                        for an up to date list of allowed languages")
                    .default_value(Some("en-US"))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("latency")
                    .nick("Latency")
                    .blurb("Amount of milliseconds to allow AWS transcribe")
                    .default_value(DEFAULT_LATENCY.mseconds() as u32)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("lateness")
                    .nick("Lateness")
                    .blurb("Amount of milliseconds to introduce as lateness")
                    .default_value(DEFAULT_LATENESS.mseconds() as u32)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("vocabulary-name")
                    .nick("Vocabulary Name")
                    .blurb("The name of a custom vocabulary, see \
                        <https://docs.aws.amazon.com/transcribe/latest/dg/how-vocabulary.html> \
                        for more information")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("session-id")
                    .nick("Session ID")
                    .blurb("The ID of the transcription session, must be length 36")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder::<AwsTranscriberResultStability>("results-stability", DEFAULT_STABILITY)
                    .nick("Results stability")
                    .blurb("Defines how fast results should stabilize")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("access-key")
                    .nick("Access Key")
                    .blurb("AWS Access Key")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("secret-access-key")
                    .nick("Secret Access Key")
                    .blurb("AWS Secret Access Key")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("session-token")
                    .nick("Session Token")
                    .blurb("AWS temporary Session Token from STS")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("vocabulary-filter-name")
                    .nick("Vocabulary Filter Name")
                    .blurb("The name of a custom filter vocabulary, see \
                        <https://docs.aws.amazon.com/transcribe/latest/help-panel/vocab-filter.html> \
                        for more information")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder::<AwsTranscriberVocabularyFilterMethod>("vocabulary-filter-method", DEFAULT_VOCABULARY_FILTER_METHOD)
                    .nick("Vocabulary Filter Method")
                    .blurb("Defines how filtered words will be edited, has no effect when vocabulary-filter-name isn't set")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.instance();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
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
            "session-token" => {
                let mut settings = self.settings.lock().unwrap();
                settings.session_token = value.get().expect("type checked upstream");
            }
            "vocabulary-filter-name" => {
                let mut settings = self.settings.lock().unwrap();
                settings.vocabulary_filter = value.get().expect("type checked upstream");
            }
            "vocabulary-filter-method" => {
                let mut settings = self.settings.lock().unwrap();
                settings.vocabulary_filter_method = value
                    .get::<AwsTranscriberVocabularyFilterMethod>()
                    .expect("type checked upstream");
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
            "session-token" => {
                let settings = self.settings.lock().unwrap();
                settings.session_token.to_value()
            }
            "vocabulary-filter-name" => {
                let settings = self.settings.lock().unwrap();
                settings.vocabulary_filter.to_value()
            }
            "vocabulary-filter-method" => {
                let settings = self.settings.lock().unwrap();
                settings.vocabulary_filter_method.to_value()
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

            let sink_caps = gst_audio::AudioCapsBuilder::new()
                .format(gst_audio::AudioFormat::S16le)
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

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::info!(CAT, imp: self, "Changing state {:?}", transition);

        let mut success = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                self.disconnect();
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

    fn provide_clock(&self) -> Option<gst::Clock> {
        Some(gst::SystemClock::obtain())
    }
}
