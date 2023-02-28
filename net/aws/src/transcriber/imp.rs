// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
// Copyright (C) 2023 François Laignel <francois@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use aws_sdk_transcribestreaming as aws_transcribe;
use aws_sdk_transcribestreaming::model;

use futures::channel::mpsc;
use futures::prelude::*;
use tokio::{runtime, task};

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use super::{AwsTranscriberResultStability, AwsTranscriberVocabularyFilterMethod};

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

const DEFAULT_TRANSCRIBER_REGION: &str = "us-east-1";
const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::from_seconds(8);
const DEFAULT_LATENESS: gst::ClockTime = gst::ClockTime::ZERO;
const DEFAULT_LANGUAGE_CODE: &str = "en-US";
const DEFAULT_STABILITY: AwsTranscriberResultStability = AwsTranscriberResultStability::Low;
const DEFAULT_VOCABULARY_FILTER_METHOD: AwsTranscriberVocabularyFilterMethod =
    AwsTranscriberVocabularyFilterMethod::Mask;
const GRANULARITY: gst::ClockTime = gst::ClockTime::from_mseconds(100);

#[derive(Debug, Clone)]
struct Settings {
    latency: gst::ClockTime,
    lateness: gst::ClockTime,
    language_code: String,
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
            language_code: DEFAULT_LANGUAGE_CODE.to_string(),
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

#[derive(Debug)]
struct TranscriptionSettings {
    lang_code: model::LanguageCode,
    sample_rate: i32,
    vocabulary: Option<String>,
    vocabulary_filter: Option<String>,
    vocabulary_filter_method: model::VocabularyFilterMethod,
    session_id: Option<String>,
    results_stability: model::PartialResultsStability,
}

impl TranscriptionSettings {
    fn from(settings: &Settings, sample_rate: i32) -> Self {
        TranscriptionSettings {
            lang_code: settings.language_code.as_str().into(),
            sample_rate,
            vocabulary: settings.vocabulary.clone(),
            vocabulary_filter: settings.vocabulary_filter.clone(),
            vocabulary_filter_method: settings.vocabulary_filter_method.into(),
            session_id: settings.session_id.clone(),
            results_stability: settings.results_stability.into(),
        }
    }
}

struct State {
    client: Option<aws_transcribe::Client>,
    buffer_tx: Option<mpsc::Sender<gst::Buffer>>,
    transcript_notif_tx: Option<mpsc::Sender<()>>,
    ws_loop_handle: Option<task::JoinHandle<Result<(), gst::ErrorMessage>>>,
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
            client: None,
            buffer_tx: None,
            transcript_notif_tx: None,
            ws_loop_handle: None,
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

pub struct Transcriber {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl Transcriber {
    fn dequeue(&self) -> bool {
        /* First, check our pending buffers */
        let mut items = vec![];

        let Some(now) = self.obj().current_running_time() else { return true };

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
                "Checking now {now} if item is ready for dequeuing, PTS {pts}, threshold {} vs {}",
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
                    gst::log!(CAT, "Pushing gap:    {last_position} -> {pts}");
                    if !self.srcpad.push_event(gap_event) {
                        return false;
                    }
                }
                Ordering::Less => {
                    let delta = last_position - pts;

                    gst::warning!(
                        CAT,
                        imp: self,
                        "Updating item PTS ({pts} < {last_position}), consider increasing latency",
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

            gst::debug!(CAT, "Pushing buffer: {pts} -> {}", pts + duration);

            if self.srcpad.push(buf).is_err() {
                return false;
            }
        }

        /* next, push a gap if we're lagging behind the target position */
        gst::trace!(
            CAT,
            imp: self,
            "Checking now: {now} if we need to push a gap, last_position: {last_position}, threshold: {}",
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
                "Pushing gap:    {last_position} -> {}",
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

    /// Enqueues a buffer for each of the provided stable items.
    ///
    /// Returns `true` if at least one buffer was enqueued.
    fn enqueue(&self, items: &[model::Item], partial: bool, lateness: gst::ClockTime) -> bool {
        let mut state = self.state.lock().unwrap();

        if items.len() <= state.partial_index {
            gst::error!(
                CAT,
                imp: self,
                "sanity check failed, alternative length {} < partial_index {}",
                items.len(),
                state.partial_index
            );

            if !partial {
                state.partial_index = 0;
            }

            return false;
        }

        let mut enqueued = false;

        for item in &items[state.partial_index..] {
            if !item.stable().unwrap_or(false) {
                break;
            }

            let Some(content) = item.content() else { continue };

            let start_time = ((item.start_time * 1_000_000_000.0) as u64).nseconds() + lateness;
            let end_time = ((item.end_time * 1_000_000_000.0) as u64).nseconds() + lateness;

            /* Should be sent now */
            gst::debug!(
                CAT,
                imp: self,
                "Item is ready for queuing: {content}, PTS {start_time}",
            );

            let mut buf = gst::Buffer::from_mut_slice(content.to_string().into_bytes());
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
            enqueued = true;
        }

        if !partial {
            state.partial_index = 0;
        }

        enqueued
    }

    fn pad_loop_fn(&self, transcript_notif_rx: &mut mpsc::Receiver<()>) {
        let mut events = {
            let mut events = vec![];

            let state = self.state.lock().unwrap();
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
            }

            events
        };

        if !events.is_empty() {
            for event in events.drain(..) {
                gst::info!(CAT, imp: self, "Sending {event:?}");
                self.srcpad.push_event(event);
            }

            self.state.lock().unwrap().send_events = false;
        }

        let future = async move {
            let timeout = tokio::time::sleep(GRANULARITY.into()).fuse();
            futures::pin_mut!(timeout);

            futures::select! {
                notif = transcript_notif_rx.next() => {
                    if notif.is_none() {
                        // Transcriber loop terminated
                        self.state.lock().unwrap().send_eos = true;
                        return;
                    };
                }
                _ = timeout => (),
            };

            if !self.dequeue() {
                gst::info!(CAT, imp: self, "Failed to dequeue buffer, pausing");
                let _ = self.srcpad.pause_task();
            }
        };

        let _enter = RUNTIME.enter();
        futures::executor::block_on(future)
    }

    fn start_task(&self) -> Result<(), gst::LoggableError> {
        let mut state = self.state.lock().unwrap();

        let (transcript_notif_tx, mut transcript_notif_rx) = mpsc::channel(1);

        let imp = self.ref_counted();
        let res = self
            .srcpad
            .start_task(move || imp.pad_loop_fn(&mut transcript_notif_rx));

        if res.is_err() {
            state.transcript_notif_tx = None;
            return Err(gst::loggable_error!(CAT, "Failed to start pad task"));
        }

        state.transcript_notif_tx = Some(transcript_notif_tx);

        Ok(())
    }

    fn stop_task(&self) {
        let mut state = self.state.lock().unwrap();

        let _ = self.srcpad.stop_task();

        if let Some(ws_loop_handle) = state.ws_loop_handle.take() {
            ws_loop_handle.abort();
        }

        state.transcript_notif_tx = None;
        state.buffer_tx = None;
    }

    fn stop_ws_loop(&self) {
        let mut state = self.state.lock().unwrap();

        if let Some(ws_loop_handle) = state.ws_loop_handle.take() {
            ws_loop_handle.abort();
        }

        state.buffer_tx = None;
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
            self.stop_task();
        }

        Ok(())
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj: pad, "Handling query {query:?}");

        use gst::QueryViewMut::*;
        match query.view_mut() {
            Latency(q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = self.sinkpad.peer_query(&mut peer_query);

                if ret {
                    let (_, min, _) = peer_query.result();
                    let our_latency = self.settings.lock().unwrap().latency;
                    q.set(true, our_latency + min, gst::ClockTime::NONE);
                }
                ret
            }
            Position(q) => {
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
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj: pad, "Handling event {event:?}");

        use gst::EventView::*;
        match event.view() {
            Eos(_) => {
                self.stop_ws_loop();

                true
            }
            FlushStart(_) => {
                gst::info!(CAT, imp: self, "Received flush start, disconnecting");
                let ret = gst::Pad::event_default(pad, Some(&*self.obj()), event);
                self.stop_task();

                ret
            }
            FlushStop(_) => {
                gst::info!(CAT, imp: self, "Received flush stop, restarting task");

                if gst::Pad::event_default(pad, Some(&*self.obj()), event) {
                    match self.start_task() {
                        Err(err) => {
                            gst::error!(CAT, imp: self, "Failed to start srcpad task: {err}");
                            false
                        }
                        Ok(_) => true,
                    }
                } else {
                    false
                }
            }
            Segment(e) => {
                let segment = match e.segment().clone().downcast::<gst::ClockTime>() {
                    Err(segment) => {
                        gst::element_imp_error!(
                            self,
                            gst::StreamError::Format,
                            ["Only Time segments supported, got {:?}", segment.format()]
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
            Tag(_) => true,
            Caps(c) => {
                gst::info!(CAT, "Received caps {c:?}");
                true
            }
            StreamStart(_) => true,
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj: pad, "Handling {buffer:?}");

        self.ensure_connection().map_err(|err| {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["Streaming failed: {err}"]);
            gst::FlowError::Error
        })?;

        let Some(mut buffer_tx) = self.state.lock().unwrap().buffer_tx.take() else {
            gst::log!(CAT, obj: pad, "Flushing");
            return Err(gst::FlowError::Flushing);
        };

        futures::executor::block_on(buffer_tx.send(buffer)).map_err(|err| {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["Streaming failed: {err}"]);
            gst::FlowError::Error
        })?;

        self.state.lock().unwrap().buffer_tx = Some(buffer_tx);

        Ok(gst::FlowSuccess::Ok)
    }

    fn ensure_connection(&self) -> Result<(), gst::ErrorMessage> {
        enum ClientStage {
            Ready(aws_transcribe::Client),
            NotReady {
                access_key: Option<String>,
                secret_access_key: Option<String>,
                session_token: Option<String>,
            },
        }

        let (client_stage, transcription_settings, lateness, transcript_notif_tx);
        {
            let mut state = self.state.lock().unwrap();

            if let Some(ref ws_loop_handle) = state.ws_loop_handle {
                if ws_loop_handle.is_finished() {
                    state.ws_loop_handle = None;

                    const ERR: &str = "ws loop terminated unexpectedly";
                    gst::error!(CAT, imp: self, "{ERR}");
                    return Err(gst::error_msg!(gst::LibraryError::Failed, ["{ERR}"]));
                }

                return Ok(());
            }

            transcript_notif_tx = state
                .transcript_notif_tx
                .take()
                .expect("attempting to spawn the ws loop, but the srcpad task hasn't been started");

            let settings = self.settings.lock().unwrap();

            lateness = settings.lateness;
            if settings.latency + lateness <= 2 * GRANULARITY {
                const ERR: &str = "latency + lateness must be greater than 200 milliseconds";
                gst::error!(CAT, imp: self, "{ERR}");
                return Err(gst::error_msg!(gst::LibraryError::Settings, ["{ERR}"]));
            }

            let in_caps = self.sinkpad.current_caps().unwrap();
            let s = in_caps.structure(0).unwrap();
            let sample_rate = s.get::<i32>("rate").unwrap();

            transcription_settings = TranscriptionSettings::from(&settings, sample_rate);

            client_stage = if let Some(client) = state.client.take() {
                ClientStage::Ready(client)
            } else {
                ClientStage::NotReady {
                    access_key: settings.access_key.to_owned(),
                    secret_access_key: settings.secret_access_key.to_owned(),
                    session_token: settings.session_token.to_owned(),
                }
            };
        };

        let client = match client_stage {
            ClientStage::Ready(client) => client,
            ClientStage::NotReady {
                access_key,
                secret_access_key,
                session_token,
            } => {
                gst::info!(CAT, imp: self, "Connecting...");
                let _enter_guard = RUNTIME.enter();

                let config_loader = match (access_key, secret_access_key) {
                    (Some(key), Some(secret_key)) => {
                        gst::debug!(CAT, imp: self, "Using settings credentials");
                        aws_config::ConfigLoader::default().credentials_provider(
                            aws_transcribe::Credentials::new(
                                key,
                                secret_key,
                                session_token,
                                None,
                                "translate",
                            ),
                        )
                    }
                    _ => {
                        gst::debug!(CAT, imp: self, "Attempting to get credentials from env...");
                        aws_config::from_env()
                    }
                };

                let config_loader = config_loader.region(
                    aws_config::meta::region::RegionProviderChain::default_provider()
                        .or_else(DEFAULT_TRANSCRIBER_REGION),
                );
                let config = futures::executor::block_on(config_loader.load());
                gst::debug!(CAT, imp: self, "Using region {}", config.region().unwrap());

                aws_transcribe::Client::new(&config)
            }
        };

        let mut state = self.state.lock().unwrap();

        let (buffer_tx, buffer_rx) = mpsc::channel(1);
        let ws_loop_handle = RUNTIME.spawn(self.build_ws_loop_fut(
            client,
            transcription_settings,
            lateness,
            buffer_rx,
            transcript_notif_tx,
        ));

        state.ws_loop_handle = Some(ws_loop_handle);
        state.buffer_tx = Some(buffer_tx);

        Ok(())
    }

    fn build_ws_loop_fut(
        &self,
        client: aws_transcribe::Client,
        settings: TranscriptionSettings,
        lateness: gst::ClockTime,
        buffer_rx: mpsc::Receiver<gst::Buffer>,
        transcript_notif_tx: mpsc::Sender<()>,
    ) -> impl Future<Output = Result<(), gst::ErrorMessage>> {
        let imp_weak = self.downgrade();
        async move {
            use gst::glib::subclass::ObjectImplWeakRef;

            // Guard that restores client & transcript_notif_tx when the ws loop is done
            struct Guard {
                imp_weak: ObjectImplWeakRef<Transcriber>,
                client: Option<aws_transcribe::Client>,
                transcript_notif_tx: Option<mpsc::Sender<()>>,
            }

            impl Guard {
                fn client(&self) -> &aws_transcribe::Client {
                    self.client.as_ref().unwrap()
                }

                fn transcript_notif_tx(&mut self) -> &mut mpsc::Sender<()> {
                    self.transcript_notif_tx.as_mut().unwrap()
                }
            }

            impl Drop for Guard {
                fn drop(&mut self) {
                    if let Some(imp) = self.imp_weak.upgrade() {
                        let mut state = imp.state.lock().unwrap();
                        state.client = self.client.take();
                        state.transcript_notif_tx = self.transcript_notif_tx.take();
                    }
                }
            }

            let mut guard = Guard {
                imp_weak: imp_weak.clone(),
                client: Some(client),
                transcript_notif_tx: Some(transcript_notif_tx),
            };

            // Stream the incoming buffers chunked
            let chunk_stream = buffer_rx.flat_map(move |buffer: gst::Buffer| {
                async_stream::stream! {
                    let data = buffer.map_readable().unwrap();
                    use aws_transcribe::{model::{AudioEvent, AudioStream}, types::Blob};
                    for chunk in data.chunks(8192) {
                        yield Ok(AudioStream::AudioEvent(AudioEvent::builder().audio_chunk(Blob::new(chunk)).build()));
                    }
                }
            });

            let mut transcribe_builder = guard
                .client()
                .start_stream_transcription()
                .language_code(settings.lang_code)
                .media_sample_rate_hertz(settings.sample_rate)
                .media_encoding(model::MediaEncoding::Pcm)
                .enable_partial_results_stabilization(true)
                .partial_results_stability(settings.results_stability)
                .set_vocabulary_name(settings.vocabulary)
                .set_session_id(settings.session_id);

            if let Some(vocabulary_filter) = settings.vocabulary_filter {
                transcribe_builder = transcribe_builder
                    .vocabulary_filter_name(vocabulary_filter)
                    .vocabulary_filter_method(settings.vocabulary_filter_method);
            }

            let mut output = transcribe_builder
                .audio_stream(chunk_stream.into())
                .send()
                .await
                .map_err(|err| {
                    let err = format!("Transcribe ws init error: {err}");
                    if let Some(imp) = imp_weak.upgrade() {
                        gst::error!(CAT, imp: imp, "{err}");
                    }
                    gst::error_msg!(gst::LibraryError::Init, ["{err}"])
                })?;

            while let Some(event) = output
                .transcript_result_stream
                .recv()
                .await
                .map_err(|err| {
                    let err = format!("Transcribe ws stream error: {err}");
                    if let Some(imp) = imp_weak.upgrade() {
                        gst::error!(CAT, imp: imp, "{err}");
                    }
                    gst::error_msg!(gst::LibraryError::Failed, ["{err}"])
                })?
            {
                if let model::TranscriptResultStream::TranscriptEvent(transcript_evt) = event {
                    let mut enqueued = false;

                    if let Some(result) = transcript_evt
                        .transcript
                        .as_ref()
                        .and_then(|transcript| transcript.results())
                        .and_then(|results| results.get(0))
                    {
                        let Some(imp) = imp_weak.upgrade() else { break };

                        gst::trace!(CAT, imp: imp, "Received: {result:?}");

                        if let Some(alternative) = result
                            .alternatives
                            .as_ref()
                            .and_then(|alternatives| alternatives.get(0))
                        {
                            if let Some(items) = alternative.items() {
                                enqueued = imp.enqueue(items, result.is_partial, lateness);
                            }
                        }
                    }

                    if enqueued && guard.transcript_notif_tx().send(()).await.is_err() {
                        if let Some(imp) = imp_weak.upgrade() {
                            gst::debug!(CAT, imp: imp, "Terminated transcript_notif_tx channel");
                        }
                        break;
                    }
                } else if let Some(imp) = imp_weak.upgrade() {
                    gst::warning!(
                        CAT,
                        imp: imp,
                        "Transcribe ws returned unknown event: consider upgrading the SDK"
                    )
                } else {
                    // imp has left the building
                    break;
                }
            }

            if let Some(imp) = imp_weak.upgrade() {
                gst::debug!(CAT, imp: imp, "Exiting ws loop");
            }

            Ok(())
        }
    }

    fn disconnect(&self) {
        let mut state = self.state.lock().unwrap();
        gst::info!(CAT, imp: self, "Unpreparing");
        self.stop_task();
        // Also resets discont to true
        *state = State::default();
        gst::info!(CAT, imp: self, "Unprepared");
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Transcriber {
    const NAME: &'static str = "GstAwsTranscriber";
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

        let settings = Mutex::new(Settings::default());

        Self {
            srcpad,
            sinkpad,
            settings,
            state: Mutex::new(State::default()),
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
                    .default_value(Some(DEFAULT_LANGUAGE_CODE))
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
                glib::ParamSpecEnum::builder_with_default("results-stability", DEFAULT_STABILITY)
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
                glib::ParamSpecEnum::builder_with_default("vocabulary-filter-method", DEFAULT_VOCABULARY_FILTER_METHOD)
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

        let obj = self.obj();
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
            "Jordan Petridis <jordan@centricular.com>, Mathieu Duponchelle <mathieu@centricular.com>, François Laignel <francois@centricular.com>",
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
        gst::info!(CAT, imp: self, "Changing state {transition:?}");

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
