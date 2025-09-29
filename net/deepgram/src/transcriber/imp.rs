// Copyright (C) 2025 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Deepgram Transcriber element.
//!
//! This element calls the Deepgram streaming transcription API to generate text from audio

use gst::subclass::prelude::*;
use gst::{glib, prelude::*};

use std::collections::VecDeque;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Mutex;

use std::sync::LazyLock;

use tokio::runtime;

use futures::channel::mpsc;
use futures::prelude::*;

use super::{DeepgramInterimStrategy, CAT};
use deepgram::{
    common::{options::Encoding, options::OptionsBuilder, stream_response::StreamResponse},
    Deepgram,
};

static RUNTIME: LazyLock<runtime::Runtime> = LazyLock::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("gst-deepgram-runtime")
        .build()
        .unwrap()
});

const GRANULARITY: gst::ClockTime = gst::ClockTime::from_mseconds(100);

const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::from_seconds(1);
const DEFAULT_LATENESS: gst::ClockTime = gst::ClockTime::ZERO;
const DEFAULT_INTERIM_STRATEGY: DeepgramInterimStrategy = DeepgramInterimStrategy::Index;
const DEFAULT_INTERIM_TIMING_THRESHOLD: gst::ClockTime = gst::ClockTime::from_mseconds(40);
const DEFAULT_DIARIZATION: bool = false;
const DEFAULT_API_KEY: Option<&str> = None;
const DEFAULT_INPUT_LANG_CODE: &str = "en";

#[derive(Debug, Clone)]
pub(super) struct Settings {
    latency: gst::ClockTime,
    lateness: gst::ClockTime,
    interim_strategy: DeepgramInterimStrategy,
    interim_timing_threshold: gst::ClockTime,
    diarization: bool,
    api_key: Option<String>,
    language_code: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            latency: DEFAULT_LATENCY,
            lateness: DEFAULT_LATENESS,
            interim_strategy: DEFAULT_INTERIM_STRATEGY,
            interim_timing_threshold: DEFAULT_INTERIM_TIMING_THRESHOLD,
            diarization: DEFAULT_DIARIZATION,
            api_key: DEFAULT_API_KEY.map(String::from),
            language_code: DEFAULT_INPUT_LANG_CODE.to_string(),
        }
    }
}

struct State {
    // (live, min, max)
    upstream_latency: Option<(bool, gst::ClockTime, Option<gst::ClockTime>)>,
    pending_discont: bool,
    last_input_rtime: Option<gst::ClockTime>,
    client: Option<Deepgram>,
    disconts: VecDeque<(gst::ClockTime, gst::ClockTime)>,
    discont_accumulator: gst::ClockTime,
    in_segment: Option<gst::FormattedSegment<gst::ClockTime>>,
    audio_tx: Option<mpsc::Sender<gst::Buffer>>,
    result_tx: Option<std::sync::mpsc::Sender<StreamResponse>>,
    receive_handle: Option<tokio::task::JoinHandle<()>>,
    interim_index: usize,
    interim_start_time: Option<gst::ClockTime>,
    seqnum: gst::Seqnum,
    task_started: bool,
    last_result: Option<StreamResponse>,
    last_speaker: Option<i32>,
    audio_info: Option<gst_audio::AudioInfo>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            upstream_latency: None,
            pending_discont: true,
            last_input_rtime: None,
            client: None,
            disconts: VecDeque::new(),
            discont_accumulator: gst::ClockTime::ZERO,
            in_segment: None,
            audio_tx: None,
            result_tx: None,
            receive_handle: None,
            interim_index: 0,
            interim_start_time: None,
            seqnum: gst::Seqnum::next(),
            task_started: false,
            last_result: None,
            last_speaker: None,
            audio_info: None,
        }
    }
}

// Locking order: state -> settings
pub struct Transcriber {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
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

    fn dequeue_result(
        &self,
        state: &mut State,
        result: &StreamResponse,
        now: Option<gst::ClockTime>,
        settings: &Settings,
        upstream_min_latency: gst::ClockTime,
    ) -> (Vec<TranscriptOutput>, bool) {
        let mut output: Vec<TranscriptOutput> = vec![];

        let StreamResponse::TranscriptResponse {
            is_final,
            channel,
            speech_final,
            ..
        } = result
        else {
            unreachable!();
        };

        let Some(alternative) = channel.alternatives.first() else {
            return (output, *is_final);
        };

        let in_segment = state
            .in_segment
            .as_mut()
            .expect("segment received before result");

        for (idx, item) in alternative.words.iter().enumerate() {
            let dg_start_time: gst::ClockTime = ((item.start * 1_000_000_000.0) as u64).nseconds();
            let dg_end_time: gst::ClockTime = ((item.end * 1_000_000_000.0) as u64).nseconds();

            match settings.interim_strategy {
                DeepgramInterimStrategy::Timing => {
                    if let Some(interim_start_time) = state.interim_start_time {
                        if dg_start_time <= interim_start_time + settings.interim_timing_threshold {
                            continue;
                        }
                    }
                }
                DeepgramInterimStrategy::Index => {
                    if idx < state.interim_index {
                        continue;
                    }
                }
                _ => (),
            }

            let rtime = dg_start_time + state.discont_accumulator + settings.lateness;

            if !is_final {
                let Some(now) = now else {
                    return (output, *is_final);
                };

                if rtime + settings.latency + upstream_min_latency > now + GRANULARITY {
                    gst::trace!(
                        CAT,
                        imp = self,
                        "interim item {item:?} still has time to stabilize, {} + {} + {} > {} + {}",
                        rtime,
                        settings.latency,
                        upstream_min_latency,
                        now,
                        GRANULARITY
                    );
                    break;
                }

                gst::log!(CAT, imp = self, "interim {item:?} needs to be output now");
            } else {
                gst::log!(CAT, imp = self, "{item:?} is final");
            }

            let mut flags: gst::BufferFlags = gst::BufferFlags::empty();

            while let Some((discont_rtime, pts)) = state.disconts.pop_front() {
                gst::trace!(
                    CAT,
                    imp = self,
                    "checking if next discont is reached: {discont_rtime} {pts}"
                );

                if dg_start_time >= discont_rtime {
                    flags.set(gst::BufferFlags::DISCONT, true);

                    let base_rtime = in_segment.to_running_time(pts).unwrap();

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

            let Some(mut pts) = in_segment.position_from_running_time(rtime) else {
                gst::warning!(
                    CAT,
                    imp = self,
                    "received item with running time outside of segment ({})",
                    rtime
                );
                continue;
            };

            let duration = dg_end_time.saturating_sub(dg_start_time);

            if let Some(position) = in_segment.position() {
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
                        gst::debug!(
                            CAT,
                            imp = self,
                            "Item stabilized too late, adjusting timestamp ({pts} -> {position}), \
                                consider increasing latency"
                        );
                        pts = position;
                    }
                    _ => (),
                }
            }

            if item.speaker != state.last_speaker {
                output.push(TranscriptOutput::Event(
                    gst::event::CustomDownstream::builder(
                        gst::Structure::builder("rstranscribe/speaker-change")
                            .field("speaker", item.speaker.map(|n| n.to_string()))
                            .build(),
                    )
                    .build(),
                ));
            }

            let punctuated_word = item.punctuated_word.as_ref().expect("punctuation enabled");

            output.push(TranscriptOutput::Event(
                gst::event::CustomUpstream::builder(
                    gst::Structure::builder("rstranscribe/new-item")
                        .field("speaker", item.speaker.map(|n| n.to_string()))
                        .field("content", punctuated_word)
                        .field("running-time", rtime)
                        .field("duration", duration)
                        .build(),
                )
                .build(),
            ));

            state.last_speaker = item.speaker;

            gst::debug!(
                CAT,
                imp = self,
                "queuing item {} with pts {pts} \
                    and duration {duration}",
                punctuated_word,
            );

            in_segment.set_position(Some(pts + duration));

            let mut buf = gst::Buffer::from_mut_slice(punctuated_word.clone().into_bytes());
            {
                let buf_mut = buf.get_mut().unwrap();
                buf_mut.set_pts(pts);
                buf_mut.set_duration(duration);
            }

            output.push(TranscriptOutput::Item(buf));

            state.interim_index = idx;
            state.interim_start_time = Some(dg_start_time);

            gst::trace!(
                CAT,
                imp = self,
                "partial index is now {}",
                state.interim_index
            );
        }

        if *speech_final {
            output.push(TranscriptOutput::Event(
                gst::event::CustomDownstream::builder(
                    gst::Structure::builder("rstranscribe/final-transcript").build(),
                )
                .build(),
            ));
        }

        if *is_final {
            gst::log!(CAT, imp = self, "result was final, partial index reset");

            state.interim_index = 0;
            state.interim_start_time = None;
        }

        (output, *is_final)
    }

    fn dequeue(&self, result: Option<&StreamResponse>) -> (Vec<TranscriptOutput>, bool) {
        let now = self.obj().current_running_time();
        let upstream_latency = self.upstream_latency();

        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        let (mut output, is_final) = match result {
            Some(result) => {
                if let Some((_, upstream_min, _)) = upstream_latency {
                    self.dequeue_result(&mut state, result, now, &settings, upstream_min)
                } else {
                    self.dequeue_result(&mut state, result, now, &settings, gst::ClockTime::ZERO)
                }
            }
            None => (vec![], true),
        };

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

                    let deadline = last_rtime + settings.latency + upstream_min;

                    gst::trace!(
                        CAT,
                        imp = self,
                        "upstream is live, deadline: {deadline} now: {now} \
                        ({last_rtime} + {} + {upstream_min}",
                        settings.latency
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

        (output, is_final)
    }

    fn start_srcpad_task(&self) -> Result<(), gst::LoggableError> {
        if self.state.lock().unwrap().task_started {
            gst::debug!(CAT, imp = self, "Task started already");
            return Ok(());
        }

        gst::debug!(CAT, imp = self, "starting source pad task");

        let (result_tx, result_rx) = std::sync::mpsc::channel::<StreamResponse>();
        self.state.lock().unwrap().result_tx = Some(result_tx);

        let this_weak = self.downgrade();
        let res = self.srcpad.start_task(move || loop {
            let Some(this) = this_weak.upgrade() else {
                break;
            };

            let result = match result_rx
                .recv_timeout(std::time::Duration::from_millis(GRANULARITY.mseconds()))
            {
                Ok(result) => Some(result),
                Err(RecvTimeoutError::Timeout) => this.state.lock().unwrap().last_result.take(),
                Err(RecvTimeoutError::Disconnected) => {
                    gst::info!(CAT, imp = this, "no more results, pushing EOS and pausing");
                    let seqnum = this.state.lock().unwrap().seqnum;
                    let _ = this
                        .srcpad
                        .push_event(gst::event::Eos::builder().seqnum(seqnum).build());
                    let _ = this.srcpad.pause_task();
                    break;
                }
            };

            gst::trace!(CAT, imp = this, "processing result {result:?}");

            let (mut items, is_final) = this.dequeue(result.as_ref());

            for item in items.drain(..) {
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

                        if event.is_downstream() {
                            this.srcpad.push_event(event);
                        } else {
                            this.sinkpad.push_event(event);
                        }
                    }
                }
            }

            this.state.lock().unwrap().last_result = if is_final { None } else { result };
        });

        if res.is_err() {
            return Err(gst::loggable_error!(CAT, "Failed to start pad task"));
        }

        self.state.lock().unwrap().task_started = true;

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
                let _ = self.state.lock().unwrap().result_tx.take();
                let _ = self.srcpad.pause_task();
                self.disconnect(false);
                ret
            }
            Segment(e) => {
                {
                    let mut state = self.state.lock().unwrap();

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

                    if let Some(old_in_segment) = state.in_segment.as_ref() {
                        segment.set_position(old_in_segment.position());

                        if *old_in_segment != segment {
                            gst::element_imp_error!(
                                self,
                                gst::StreamError::Format,
                                ["Multiple segments not supported"]
                            );
                            return false;
                        } else {
                            gst::debug!(CAT, imp = self, "ignoring duplicate segment");
                            return true;
                        }
                    }

                    segment.set_position(segment.start());

                    gst::info!(CAT, imp = self, "using segment {segment:?}");

                    state.in_segment = Some(segment);
                }

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            Caps(e) => {
                let in_caps = e.caps();

                let Ok(audio_info) = gst_audio::AudioInfo::from_caps(in_caps) else {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Format,
                        ["Failed to parse audio caps: {}", in_caps]
                    );
                    return false;
                };

                self.state.lock().unwrap().audio_info = Some(audio_info);

                let caps = self.srcpad.pad_template_caps();

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
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, obj = pad, "Handling {buffer:?}");

        self.ensure_connection().map_err(|err| {
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["Streaming failed: {}", err]
            );
            gst::FlowError::Error
        })?;

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

            let Some(audio_info) = state.audio_info.as_ref() else {
                gst::warning!(CAT, imp = self, "dropping buffer before caps");
                return Ok(gst::FlowSuccess::Ok);
            };

            buffer = match gst_audio::audio_buffer_clip(
                buffer,
                segment.upcast_ref(),
                audio_info.rate(),
                audio_info.bpf(),
            ) {
                None => {
                    gst::warning!(CAT, imp = self, "dropping buffer outside segment");
                    return Ok(gst::FlowSuccess::Ok);
                }
                Some(buffer) => buffer,
            };

            let Some(pts) = buffer.pts() else {
                gst::warning!(CAT, imp = self, "dropping buffer without a PTS");
                return Ok(gst::FlowSuccess::Ok);
            };

            let mut rtime = segment.to_running_time(pts).expect("buffer was clipped");

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

        RUNTIME.block_on(audio_tx.send(buffer)).map_err(|err| {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["Streaming failed: {err}"]);
            gst::FlowError::Error
        })?;

        self.state.lock().unwrap().audio_tx = Some(audio_tx);

        Ok(gst::FlowSuccess::Ok)
    }

    fn ensure_connection(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        if state.client.is_none() {
            let Some((sample_rate, channels)) = state
                .audio_info
                .as_ref()
                .map(|info| (info.rate(), info.channels()))
            else {
                return Err(gst::error_msg!(
                    gst::CoreError::Failed,
                    ["No caps set on sinkpad"]
                ));
            };

            let (use_interim_results, diarize, api_key, language_code) = {
                let settings = self.settings.lock().unwrap();

                let Some(api_key) = settings.api_key.clone() else {
                    return Err(gst::error_msg!(
                        gst::CoreError::Failed,
                        ["An API key is required"]
                    ));
                };

                (
                    !matches!(settings.interim_strategy, DeepgramInterimStrategy::Disabled),
                    settings.diarization,
                    api_key,
                    settings.language_code.clone(),
                )
            };

            let client = Deepgram::new(api_key).map_err(|err| {
                let err = format!("Failed to create deepgram client: {err}");
                gst::error!(CAT, imp = self, "{err}");
                gst::error_msg!(gst::LibraryError::Init, ["{err}"])
            })?;

            let (audio_tx, audio_rx) = mpsc::channel::<gst::Buffer>(1);

            state.audio_tx = Some(audio_tx);
            state.client = Some(client.clone());

            let this_weak = self.downgrade();
            state.receive_handle = Some(RUNTIME.spawn(async move {
                let mut results = if let Some(this) = this_weak.upgrade() {
                    gst::info!(CAT, imp = this, "establishing connection");

                    match client
                        .transcription()
                        .stream_request_with_options(
                            OptionsBuilder::new()
                                .punctuate(true)
                                .diarize(diarize)
                                .model(deepgram::common::options::Model::Nova3)
                                .language(language_code.into())
                                .build(),
                        )
                        .keep_alive()
                        .encoding(Encoding::Linear16)
                        .sample_rate(sample_rate)
                        .channels(channels as u16)
                        .interim_results(use_interim_results)
                        .stream(audio_rx.map(|buffer| {
                            let data = buffer.map_readable().unwrap();
                            Ok::<bytes::Bytes, std::sync::mpsc::RecvError>(
                                bytes::Bytes::copy_from_slice(data.as_slice()),
                            )
                        }))
                        .await
                    {
                        Err(err) => {
                            let err = format!("Error while building transcription stream: {err}");
                            gst::error!(CAT, imp = this, "{err}");
                            this.post_error_message(gst::error_msg!(
                                gst::LibraryError::Failed,
                                ["{err}"]
                            ));
                            return;
                        }
                        Ok(results) => {
                            gst::info!(CAT, imp = this, "connection established!");

                            results
                        }
                    }
                } else {
                    return;
                };

                loop {
                    let result = match results.next().await {
                        Some(Ok(result)) => result,
                        Some(Err(err)) => {
                            let err = format!("Transcribe ws stream error: {err}",);
                            if let Some(this) = this_weak.upgrade() {
                                gst::error!(CAT, imp = this, "{err}");
                                this.post_error_message(gst::error_msg!(
                                    gst::LibraryError::Failed,
                                    ["{err}"]
                                ));
                            };
                            break;
                        }
                        None => {
                            if let Some(this) = this_weak.upgrade() {
                                gst::debug!(CAT, imp = this, "Transcriber loop sending EOS");
                            }
                            break;
                        }
                    };

                    let Some(result_tx) = this_weak
                        .upgrade()
                        .and_then(|this| this.state.lock().unwrap().result_tx.clone())
                    else {
                        break;
                    };

                    if result_tx.send(result).is_err() {
                        if let Some(this) = this_weak.upgrade() {
                            gst::info!(CAT, imp = this, "Result tx closed");
                        }
                        break;
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

        gst::debug!(CAT, imp = self, "Prepared");

        Ok(())
    }

    fn disconnect(&self, stop_task: bool) {
        let mut state = self.state.lock().unwrap();

        if let Some(handle) = state.receive_handle.take() {
            gst::info!(CAT, imp = self, "aborting result reception");
            handle.abort();
        }

        let mut task_started = state.task_started;

        // Make sure the task is fully stopped before resetting the state,
        // in order not to break expectations such as in_segment being
        // present while the task is still processing items
        if stop_task {
            drop(state);
            let _ = self.srcpad.stop_task();
            state = self.state.lock().unwrap();
            task_started = false;
        }

        *state = State {
            task_started,
            ..Default::default()
        };
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
    const NAME: &'static str = "GstDeepgramTranscriber";
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
            state: Default::default(),
            settings: Default::default(),
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
                glib::ParamSpecEnum::builder_with_default(
                    "interim-strategy",
                    DEFAULT_INTERIM_STRATEGY,
                )
                .nick("Interim Strategy")
                .blurb("Defines how interim results should be used, if at all")
                .mutable_ready()
                .build(),
                glib::ParamSpecUInt::builder("interim-timing-threshold")
                    .nick("Interim Timing Threshold")
                    .blurb(
                        "Amount of milliseconds to consider a word in an interim result new. \
                        Only used with interim-strategy=timing",
                    )
                    .default_value(DEFAULT_INTERIM_TIMING_THRESHOLD.mseconds() as u32)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("diarization")
                    .nick("Diarization")
                    .blurb("Whether diarization should be enabled")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("api-key")
                    .nick("API Key")
                    .blurb("ElevenLabs API Key")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("language-code")
                    .nick("Language Code")
                    .blurb(
                        "The Language of the Stream, see \
                        <https://developers.deepgram.com/docs/language>",
                    )
                    .default_value(Some(DEFAULT_INPUT_LANG_CODE))
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
            "interim-strategy" => {
                let mut settings = self.settings.lock().unwrap();
                settings.interim_strategy = value
                    .get::<DeepgramInterimStrategy>()
                    .expect("type checked upstream");
            }
            "interim-timing-threshold" => {
                let mut settings = self.settings.lock().unwrap();
                settings.interim_timing_threshold = gst::ClockTime::from_mseconds(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            "diarization" => {
                let mut settings = self.settings.lock().unwrap();
                settings.diarization = value.get::<bool>().expect("type checked upstream");
            }
            "api-key" => {
                let mut settings = self.settings.lock().unwrap();
                settings.api_key = value.get().expect("type checked upstream");
            }
            "language-code" => {
                let language_code: String = value.get().expect("type checked upstream");
                let mut settings = self.settings.lock().unwrap();
                settings.language_code = language_code;
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
            "interim-strategy" => {
                let settings = self.settings.lock().unwrap();
                settings.interim_strategy.to_value()
            }
            "interim-timing-threshold" => {
                let settings = self.settings.lock().unwrap();
                (settings.interim_timing_threshold.mseconds() as u32).to_value()
            }
            "diarization" => {
                let settings = self.settings.lock().unwrap();
                settings.diarization.to_value()
            }
            "api-key" => {
                let settings = self.settings.lock().unwrap();
                settings.api_key.to_value()
            }
            "language-code" => {
                let settings = self.settings.lock().unwrap();
                settings.language_code.to_value()
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
                "Speech to Text filter, using Deepgram streaming transcription API",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_audio::AudioCapsBuilder::new()
                .format(gst_audio::AudioFormat::S16le)
                .rate_list([24_000, 8_000, 16_000, 32_000, 48_000])
                .channels_range(1..)
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
                let _ = self.state.lock().unwrap().result_tx.take();
                self.disconnect(true);
            }
            _ => (),
        }

        self.parent_change_state(transition)
    }
}
