// Copyright (C) 2025 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! AWS Transcriber element (2nd version, simplified).
//!
//! This element calls AWS Transcribe to generate text from audio

use gst::subclass::prelude::*;
use gst::{glib, prelude::*};

use aws_sdk_s3::config::StalledStreamProtectionConfig;
use aws_sdk_transcribestreaming::error::ProvideErrorMetadata;
use aws_sdk_transcribestreaming::types as aws_types;

use futures::channel::mpsc;
use futures::prelude::*;

use std::collections::VecDeque;
use std::sync::Mutex;

use std::sync::LazyLock;

use super::CAT;
use crate::s3utils::RUNTIME;
use crate::transcriber::remote_types::TranscriptDef;
use crate::transcriber::{AwsTranscriberResultStability, AwsTranscriberVocabularyFilterMethod};

#[allow(deprecated)]
static AWS_BEHAVIOR_VERSION: LazyLock<aws_config::BehaviorVersion> =
    LazyLock::new(aws_config::BehaviorVersion::v2023_11_09);

const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::from_seconds(1);
const DEFAULT_LATENESS: gst::ClockTime = gst::ClockTime::ZERO;
const DEFAULT_REGION: &str = "us-east-1";
const DEFAULT_INPUT_LANG_CODE: &str = "en-US";
const DEFAULT_STABILITY: AwsTranscriberResultStability = AwsTranscriberResultStability::Low;
const DEFAULT_VOCABULARY_FILTER_METHOD: AwsTranscriberVocabularyFilterMethod =
    AwsTranscriberVocabularyFilterMethod::Mask;
const DEFAULT_SHOW_SPEAKER_LABEL: bool = false;

#[derive(Debug, Clone)]
pub(super) struct Settings {
    latency: gst::ClockTime,
    lateness: gst::ClockTime,
    access_key: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    language_code: String,
    vocabulary: Option<String>,
    session_id: Option<String>,
    results_stability: AwsTranscriberResultStability,
    vocabulary_filter: Option<String>,
    vocabulary_filter_method: AwsTranscriberVocabularyFilterMethod,
    show_speaker_label: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            latency: DEFAULT_LATENCY,
            lateness: DEFAULT_LATENESS,
            access_key: None,
            secret_access_key: None,
            session_token: None,
            language_code: DEFAULT_INPUT_LANG_CODE.to_string(),
            vocabulary: None,
            session_id: None,
            results_stability: DEFAULT_STABILITY,
            vocabulary_filter: None,
            vocabulary_filter_method: DEFAULT_VOCABULARY_FILTER_METHOD,
            show_speaker_label: DEFAULT_SHOW_SPEAKER_LABEL,
        }
    }
}

struct State {
    // (live, min, max)
    upstream_latency: Option<(bool, gst::ClockTime, Option<gst::ClockTime>)>,
    pending_discont: bool,
    last_input_rtime: Option<gst::ClockTime>,
    disconts: VecDeque<(gst::ClockTime, gst::ClockTime)>,
    discont_accumulator: gst::ClockTime,
    in_segment: Option<gst::FormattedSegment<gst::ClockTime>>,
    client: Option<aws_sdk_transcribestreaming::Client>,
    audio_tx: Option<mpsc::Sender<gst::Buffer>>,
    result_rx: Option<std::sync::mpsc::Receiver<aws_types::TranscriptEvent>>,
    receive_handle: Option<tokio::task::JoinHandle<()>>,
    partial_index: usize,
    seqnum: gst::Seqnum,
}

impl Default for State {
    fn default() -> Self {
        Self {
            upstream_latency: None,
            pending_discont: true,
            last_input_rtime: None,
            disconts: VecDeque::new(),
            discont_accumulator: gst::ClockTime::ZERO,
            in_segment: None,
            client: None,
            audio_tx: None,
            result_rx: None,
            receive_handle: None,
            partial_index: 0,
            seqnum: gst::Seqnum::next(),
        }
    }
}

pub struct Transcriber {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    pub(super) aws_config: Mutex<Option<aws_config::SdkConfig>>,
}

#[derive(Debug)]
enum TranscriptOutput {
    Event(gst::Event),
    Item(gst::Buffer),
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

    fn dequeue(&self, transcript: &mut aws_types::Transcript) -> Vec<TranscriptOutput> {
        let (latency, lateness) = {
            let settings = self.settings.lock().unwrap();
            (settings.latency, settings.lateness)
        };
        let now = self.obj().current_running_time();
        let upstream_latency = self.upstream_latency();

        let mut state = self.state.lock().unwrap();
        let mut output: Vec<TranscriptOutput> = vec![];

        if let Some(result) = transcript
            .results
            .as_mut()
            .and_then(|results| results.drain(..).next())
        {
            if let Some(mut items) = result
                .alternatives
                .and_then(|mut alternatives| alternatives.drain(..).next())
                .and_then(|alternative| alternative.items)
            {
                if items.len() < state.partial_index {
                    gst::error!(
                        CAT,
                        imp = self,
                        "sanity check failed, alternative length {} < partial_index {}",
                        items.len(),
                        state.partial_index
                    );

                    if !result.is_partial {
                        state.partial_index = 0;
                    }

                    return output;
                }

                gst::trace!(
                    CAT,
                    imp = self,
                    "draining items from index {}",
                    state.partial_index
                );

                for item in items.drain(state.partial_index..) {
                    if !item.stable().unwrap_or(false) {
                        gst::trace!(CAT, imp = self, "{item:?} isn't stabilized");
                        break;
                    }

                    gst::log!(CAT, imp = self, "{item:?} is stabilized");

                    if let Some(content) = item.content {
                        let aws_start_time: gst::ClockTime =
                            ((item.start_time * 1_000_000_000.0) as u64).nseconds();
                        let aws_end_time: gst::ClockTime =
                            ((item.end_time * 1_000_000_000.0) as u64).nseconds();

                        let mut flags: gst::BufferFlags = gst::BufferFlags::empty();

                        while let Some((discont_rtime, pts)) = state.disconts.pop_front() {
                            gst::trace!(
                                CAT,
                                imp = self,
                                "checking if next discont is reached: {discont_rtime} {pts}"
                            );

                            if aws_start_time >= discont_rtime {
                                flags.set(gst::BufferFlags::DISCONT, true);

                                let base_rtime = state
                                    .in_segment
                                    .as_ref()
                                    .unwrap()
                                    .to_running_time(pts)
                                    .unwrap();

                                state.discont_accumulator += base_rtime - discont_rtime;

                                gst::info!(
                                    CAT,
                                    imp = self,
                                    "next discont is reached: \
                                         discontinuity running time: {discont_rtime} \
                                         discontinuity pts: {pts}, \
                                         new base running time: {base_rtime}\
                                         discont accumulator is now {}",
                                    state.discont_accumulator
                                );
                            } else {
                                gst::trace!(CAT, imp = self, "next discont not reached yet");
                                state.disconts.push_front((discont_rtime, pts));
                                break;
                            }
                        }

                        let rtime = aws_start_time + state.discont_accumulator + lateness;

                        let Some(mut pts) = state
                            .in_segment
                            .as_ref()
                            .unwrap()
                            .position_from_running_time(rtime)
                        else {
                            gst::warning!(
                                CAT,
                                imp = self,
                                "received item with running time outside of segment ({})",
                                rtime
                            );
                            continue;
                        };

                        let duration = aws_end_time.saturating_sub(aws_start_time);

                        if let Some(position) = state.in_segment.as_ref().unwrap().position() {
                            match pts.cmp(&position) {
                                std::cmp::Ordering::Greater => {
                                    gst::log!(
                                        CAT,
                                        imp = self,
                                        "position lagging, queuing gap \
                                            with pts {position} and duration {}",
                                        pts - position
                                    );
                                    output.push(TranscriptOutput::Event(
                                        gst::event::Gap::builder(position)
                                            .duration(pts - position)
                                            .seqnum(state.seqnum)
                                            .build(),
                                    ));
                                }
                                std::cmp::Ordering::Less => {
                                    gst::warning!(CAT, imp = self,
                                            "Item stabilized too late, adjusting timestamp ({pts} -> {position}), \
                                            consider increasing latency");
                                    pts = position;
                                }
                                _ => (),
                            }
                        }

                        gst::log!(
                            CAT,
                            imp = self,
                            "queuing item {content} with pts {pts} \
                                and duration {duration}"
                        );

                        state
                            .in_segment
                            .as_mut()
                            .unwrap()
                            .set_position(Some(pts + duration));

                        let mut buf = gst::Buffer::from_mut_slice(content.into_bytes());
                        {
                            let buf_mut = buf.get_mut().unwrap();
                            buf_mut.set_pts(pts);
                            buf_mut.set_duration(duration);
                        }

                        output.push(TranscriptOutput::Item(buf));
                    }

                    state.partial_index += 1;

                    gst::trace!(
                        CAT,
                        imp = self,
                        "partial index is now {}",
                        state.partial_index
                    );
                }

                if !result.is_partial {
                    gst::log!(CAT, imp = self, "result was final, partial index reset");

                    output.push(TranscriptOutput::Event(
                        gst::event::CustomDownstream::builder(
                            gst::Structure::builder("rstranscribe/final-transcript").build(),
                        )
                        .build(),
                    ));

                    state.partial_index = 0;
                }
            }
        }

        if let Some(upstream_latency) = upstream_latency {
            let (upstream_live, upstream_min, _) = upstream_latency;

            // Don't push a gap before we've even received a first buffer,
            // as we haven't set in_segment.position to a start time yet
            if upstream_live && state.last_input_rtime.is_some() {
                if let Some(now) = now {
                    let seqnum = state.seqnum;
                    let in_segment = state.in_segment.as_mut().unwrap();
                    let position = in_segment.position().unwrap();
                    let last_rtime = in_segment.to_running_time(position).unwrap();

                    let deadline = last_rtime + latency + upstream_min;

                    gst::trace!(
                        CAT,
                        imp = self,
                        "upstream is live, deadline: {deadline} now: {now} \
                        ({last_rtime} + {latency} + {upstream_min}"
                    );

                    if deadline < now {
                        let gap_duration = now - deadline;

                        gst::log!(
                            CAT,
                            "deadline reached, queuing gap with pts {position} \
                            and duration {gap_duration}"
                        );

                        output.push(TranscriptOutput::Event(
                            gst::event::Gap::builder(position)
                                .duration(gap_duration)
                                .seqnum(seqnum)
                                .build(),
                        ));
                        in_segment.set_position(position + gap_duration);
                    }
                }
            }
        }

        output
    }

    fn start_srcpad_task(&self) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp = self, "starting source pad task");

        self.ensure_connection()
            .map_err(|err| gst::loggable_error!(CAT, "Failed to start pad task: {err}"))?;

        let this_weak = self.downgrade();
        let res = self.srcpad.start_task(move || loop {
            let Some(this) = this_weak.upgrade() else {
                break;
            };

            let Some(result_rx) = this.state.lock().unwrap().result_rx.take() else {
                gst::debug!(CAT, imp = this, "no more result channel, pausing");
                let _ = this.srcpad.pause_task();
                break;
            };

            let Ok(mut transcript_evt) = result_rx.recv() else {
                gst::info!(CAT, imp = this, "no more results, pushing EOS and pausing");
                let seqnum = this.state.lock().unwrap().seqnum;
                let _ = this
                    .srcpad
                    .push_event(gst::event::Eos::builder().seqnum(seqnum).build());
                let _ = this.srcpad.pause_task();
                break;
            };

            gst::trace!(
                CAT,
                imp = this,
                "processing transcript event {transcript_evt:?}"
            );

            this.state.lock().unwrap().result_rx = Some(result_rx);

            let Some(ref mut transcript) = transcript_evt.transcript else {
                gst::trace!(
                    CAT,
                    imp = this,
                    "transcript event does not contain a transcript"
                );
                continue;
            };

            let t = TranscriptDef {
                results: transcript.results.clone(),
            };

            let serialized = serde_json::to_string(&t).expect("serializable");

            let s = gst::Structure::builder("awstranscribe/raw")
                .field("transcript", serialized)
                .field("arrival-time", this.obj().current_running_time())
                .field(
                    "language-code",
                    &this.settings.lock().as_ref().unwrap().language_code,
                )
                .build();

            gst::trace!(
                CAT,
                imp = this,
                "serialized transcript event, posting message"
            );

            let _ = this
                .obj()
                .post_message(gst::message::Element::builder(s).src(&*this.obj()).build());

            for item in this.dequeue(transcript).drain(..) {
                match item {
                    TranscriptOutput::Item(buffer) => {
                        gst::debug!(CAT, imp = this, "pushing buffer {buffer:?}");

                        if let Err(err) = this.srcpad.push(buffer) {
                            if err != gst::FlowError::Flushing {
                                gst::element_error!(
                                    this.obj(),
                                    gst::StreamError::Failed,
                                    ["Streaming failed: {}", err]
                                );
                            }
                            let _ = this.srcpad.pause_task();
                        }
                    }
                    TranscriptOutput::Event(event) => {
                        gst::debug!(CAT, imp = this, "pushing event {event:?}");

                        this.srcpad.push_event(event);
                    }
                }
            }
        });

        if res.is_err() {
            return Err(gst::loggable_error!(CAT, "Failed to start pad task"));
        }

        gst::debug!(CAT, imp = self, "started source pad task");

        Ok(())
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::trace!(CAT, obj = pad, "Handling event {event:?}");

        use gst::EventView::*;
        match event.view() {
            StreamStart(_) => {
                gst::info!(CAT, imp = self, "received stream start, connecting");

                if let Err(err) = self.start_srcpad_task() {
                    gst::error!(CAT, imp = self, "Failed to start srcpad task: {err}");
                    return false;
                }

                self.state.lock().unwrap().seqnum = event.seqnum();

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            FlushStart(_) => {
                gst::info!(CAT, imp = self, "received flush start, disconnecting");
                let ret = gst::Pad::event_default(pad, Some(&*self.obj()), event);
                let _ = self.state.lock().unwrap().result_rx.take();
                let _ = self.srcpad.pause_task();
                self.disconnect(false);
                ret
            }
            Segment(e) => {
                {
                    let mut state = self.state.lock().unwrap();

                    if state.in_segment.is_some() {
                        gst::element_imp_error!(
                            self,
                            gst::StreamError::Format,
                            ["Multiple segments not supported"]
                        );
                        return false;
                    }

                    let mut segment = match e.segment().clone().downcast::<gst::ClockTime>() {
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

                    segment.set_position(segment.start());

                    gst::info!(CAT, imp = self, "using segment {segment:?}");

                    state.in_segment = Some(segment);
                }

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            Caps(_) => {
                let caps = gst::Caps::builder("text/x-raw")
                    .field("format", "utf8")
                    .build();

                let event = gst::event::Caps::builder(&caps)
                    .seqnum(self.state.lock().unwrap().seqnum)
                    .build();

                self.srcpad.push_event(event)
            }
            Eos(_) => {
                self.state.lock().unwrap().audio_tx.take();
                true
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, obj = pad, "Handling {buffer:?}");

        {
            let mut state = self.state.lock().unwrap();

            if buffer.flags().contains(gst::BufferFlags::DISCONT) {
                gst::info!(
                    CAT,
                    imp = self,
                    "buffer is discont, storing pending discont"
                );
                state.pending_discont = true;
            }

            let Some(segment) = state.in_segment.as_ref() else {
                gst::warning!(CAT, imp = self, "dropping buffer before segment");
                return Ok(gst::FlowSuccess::Ok);
            };

            let Some(pts) = buffer.pts() else {
                gst::warning!(CAT, imp = self, "dropping first buffer without a PTS");
                return Ok(gst::FlowSuccess::Ok);
            };

            let Some(mut rtime) = segment.to_running_time(pts) else {
                gst::log!(CAT, imp = self, "clipping buffer outside segment");
                return Ok(gst::FlowSuccess::Ok);
            };

            if state.pending_discont {
                state.pending_discont = false;

                let discont_rtime = state.last_input_rtime.unwrap_or(gst::ClockTime::ZERO);

                gst::info!(
                    CAT,
                    imp = self,
                    "storing discont with running time {discont_rtime} and pts {pts}"
                );

                state.disconts.push_back((discont_rtime, pts));
            }

            rtime.opt_add_assign(buffer.duration());

            gst::trace!(CAT, imp = self, "storing last input running time {rtime}");

            if state.last_input_rtime.is_none() {
                state.in_segment.as_mut().unwrap().set_position(Some(pts));
            }

            state.last_input_rtime = Some(rtime);
        }

        let Some(mut audio_tx) = self.state.lock().unwrap().audio_tx.take() else {
            gst::debug!(CAT, obj = pad, "Flushing");
            return Err(gst::FlowError::Flushing);
        };

        futures::executor::block_on(audio_tx.send(buffer)).map_err(|err| {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["Streaming failed: {err}"]);
            gst::FlowError::Error
        })?;

        self.state.lock().unwrap().audio_tx = Some(audio_tx);

        Ok(gst::FlowSuccess::Ok)
    }

    fn ensure_connection(&self) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.state.lock().unwrap();
        if state.client.is_none() {
            let client = aws_sdk_transcribestreaming::Client::new(
                self.aws_config.lock().unwrap().as_ref().expect("prepared"),
            );

            let (audio_tx, audio_rx) = mpsc::channel::<gst::Buffer>(1);
            let (result_tx, result_rx) = std::sync::mpsc::channel::<aws_types::TranscriptEvent>();

            // Stream the incoming buffers chunked
            let chunk_stream = audio_rx.flat_map(move |buffer| {
                async_stream::stream! {
                    let data = buffer.map_readable().unwrap();
                    use aws_sdk_transcribestreaming::primitives::Blob;
                    use aws_types::{AudioEvent, AudioStream};
                    for chunk in data.chunks(8192) {
                        yield Ok(AudioStream::AudioEvent(AudioEvent::builder().audio_chunk(Blob::new(chunk)).build()));
                    }
                }
            });

            let mut builder = client
                .start_stream_transcription()
                .language_code(settings.language_code.as_str().into())
                .media_sample_rate_hertz(48_000i32)
                .media_encoding(aws_types::MediaEncoding::Pcm)
                .enable_partial_results_stabilization(true)
                .partial_results_stability(settings.results_stability.into())
                .set_vocabulary_name(settings.vocabulary)
                .set_session_id(settings.session_id)
                .set_show_speaker_label(Some(settings.show_speaker_label));

            if let Some(vocabulary_filter) = settings.vocabulary_filter {
                builder = builder
                    .vocabulary_filter_name(vocabulary_filter)
                    .vocabulary_filter_method(settings.vocabulary_filter_method.into());
            }

            state.audio_tx = Some(audio_tx);
            state.result_rx = Some(result_rx);
            state.client = Some(client);

            let _enter_guard = RUNTIME.enter();

            gst::info!(CAT, imp = self, "establishing connection");

            let mut output = futures::executor::block_on(async move {
                builder.audio_stream(chunk_stream.into()).send().await
            })
            .map_err(|err| {
                let err = format!("Transcribe ws init error: {err}: {} ({err:?})", err.meta());
                gst::error!(CAT, imp = self, "{err}");
                gst::error_msg!(gst::LibraryError::Init, ["{err}"])
            })?;

            gst::info!(CAT, imp = self, "connection established!");

            let this_weak = self.downgrade();
            state.receive_handle = Some(RUNTIME.spawn(async move {
                loop {
                    let event = match output.transcript_result_stream.recv().await {
                        Ok(event) => event,
                        Err(err) => {
                            let err = format!(
                                "Transcribe ws stream error: {err}: {} {err:?}",
                                err.meta()
                            );
                            if let Some(this) = this_weak.upgrade() {
                                gst::error!(CAT, imp = this, "{err}");
                                this.post_error_message(gst::error_msg!(
                                    gst::LibraryError::Failed,
                                    ["{err}"]
                                ));
                            };
                            break;
                        }
                    };

                    let Some(event) = event else {
                        if let Some(this) = this_weak.upgrade() {
                            gst::debug!(CAT, imp = this, "Transcriber loop sending EOS");
                        }
                        break;
                    };
                    if let aws_types::TranscriptResultStream::TranscriptEvent(transcript_evt) =
                        event
                    {
                        if result_tx.send(transcript_evt).is_err() {
                            if let Some(this) = this_weak.upgrade() {
                                gst::info!(CAT, imp = this, "Result tx closed");
                            }
                            break;
                        }
                    } else if let Some(this) = this_weak.upgrade() {
                        gst::warning!(
                            CAT,
                            imp = this,
                            "Transcribe ws returned unknown event: consider upgrading the SDK"
                        )
                    }
                }
            }));
        } else {
            gst::trace!(CAT, imp = self, "connection was already established");
        }

        Ok(())
    }

    fn prepare(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Preparing");

        let (access_key, secret_access_key, session_token) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.access_key.clone(),
                settings.secret_access_key.clone(),
                settings.session_token.clone(),
            )
        };

        gst::log!(CAT, imp = self, "Loading aws config...");
        let _enter_guard = RUNTIME.enter();

        let config_loader = match (access_key, secret_access_key) {
            (Some(key), Some(secret_key)) => {
                gst::log!(CAT, imp = self, "Using settings credentials");
                aws_config::defaults(*AWS_BEHAVIOR_VERSION).credentials_provider(
                    aws_sdk_translate::config::Credentials::new(
                        key,
                        secret_key,
                        session_token,
                        None,
                        "translate",
                    ),
                )
            }
            _ => {
                gst::log!(CAT, imp = self, "Attempting to get credentials from env...");
                aws_config::defaults(*AWS_BEHAVIOR_VERSION)
            }
        };

        let config_loader = config_loader.region(
            aws_config::meta::region::RegionProviderChain::default_provider()
                .or_else(DEFAULT_REGION),
        );

        let config_loader =
            config_loader.stalled_stream_protection(StalledStreamProtectionConfig::disabled());

        let config = futures::executor::block_on(config_loader.load());
        gst::log!(CAT, imp = self, "Using region {}", config.region().unwrap());

        *self.aws_config.lock().unwrap() = Some(config);

        gst::debug!(CAT, imp = self, "Prepared");

        Ok(())
    }

    fn disconnect(&self, stop_task: bool) {
        let mut state = self.state.lock().unwrap();

        if let Some(handle) = state.receive_handle.take() {
            gst::info!(CAT, imp = self, "aborting result reception");
            handle.abort();
        }

        // Make sure the task is fully stopped before resetting the state,
        // in order not to break expectations such as in_segment being
        // present while the task is still processing items
        if stop_task {
            drop(state);
            let _ = self.srcpad.stop_task();
            state = self.state.lock().unwrap();
        }

        *state = State::default();
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        match query.view_mut() {
            gst::QueryViewMut::Latency(ref mut q) => {
                self.state.lock().unwrap().upstream_latency = None;

                if let Some(upstream_latency) = self.upstream_latency() {
                    let (live, min, max) = upstream_latency;
                    let our_latency = self.settings.lock().unwrap().latency;

                    if live {
                        q.set(true, min + our_latency, max.map(|max| max + our_latency));
                    } else {
                        q.set(live, min, max);
                    }
                    true
                } else {
                    false
                }
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Transcriber {
    const NAME: &'static str = "GstAwsTranscriber2";
    type Type = super::Transcriber;
    type ParentType = gst::Element;

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

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::PadBuilder::<gst::Pad>::from_template(&templ)
            .query_function(|pad, parent, query| {
                Transcriber::catch_panic_pad_function(
                    parent,
                    || false,
                    |transcriber| transcriber.src_query(pad, query),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Default::default(),
            state: Default::default(),
            aws_config: Default::default(),
        }
    }
}

impl ObjectImpl for Transcriber {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("latency")
                    .nick("Latency")
                    .blurb("Amount of milliseconds to allow AWS transcribe")
                    .default_value(DEFAULT_LATENCY.mseconds() as u32)
                    .mutable_ready()
                    .deprecated()
                    .build(),
                glib::ParamSpecUInt::builder("lateness")
                    .nick("Lateness")
                    .blurb("Amount of milliseconds to introduce as lateness")
                    .default_value(DEFAULT_LATENESS.mseconds() as u32)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("language-code")
                    .nick("Language Code")
                    .blurb("The Language of the Stream, see \
                        <https://docs.aws.amazon.com/transcribe/latest/dg/how-streaming-transcription.html> \
                        for an up to date list of allowed languages")
                    .default_value(Some(DEFAULT_INPUT_LANG_CODE))
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
                glib::ParamSpecBoolean::builder("show-speaker-label")
                    .nick("Show Speaker Label")
                    .blurb("Defines whether to partition speakers in the transcription output")
                    .default_value(DEFAULT_SHOW_SPEAKER_LABEL)
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
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
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
            "language-code" => {
                let language_code: String = value.get().expect("type checked upstream");
                let mut settings = self.settings.lock().unwrap();
                settings.language_code = language_code;
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
            "show-speaker-label" => {
                let mut settings = self.settings.lock().unwrap();
                settings.show_speaker_label = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "latency" => {
                let settings = self.settings.lock().unwrap();
                (settings.latency.mseconds() as u32).to_value()
            }
            "lateness" => {
                let settings = self.settings.lock().unwrap();
                (settings.lateness.mseconds() as u32).to_value()
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
            "language-code" => {
                let settings = self.settings.lock().unwrap();
                settings.language_code.to_value()
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
            "vocabulary-filter-name" => {
                let settings = self.settings.lock().unwrap();
                settings.vocabulary_filter.to_value()
            }
            "vocabulary-filter-method" => {
                let settings = self.settings.lock().unwrap();
                settings.vocabulary_filter_method.to_value()
            }
            "show-speaker-label" => {
                let settings = self.settings.lock().unwrap();
                settings.show_speaker_label.to_value()
            }
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
                "Audio/Text/Filter",
                "Speech to Text filter, using AWS transcribe",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_audio::AudioCapsBuilder::new()
                .format(gst_audio::AudioFormat::S16le)
                // FIXME
                .rate(48_000)
                .channels(1)
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

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

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::log!(CAT, imp = self, "Changing state {transition:?}");

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare().map_err(|err| {
                    self.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PausedToReady => {
                self.disconnect(true);
            }
            _ => (),
        }

        self.parent_change_state(transition)
    }
}
