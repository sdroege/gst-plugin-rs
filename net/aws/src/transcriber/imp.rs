// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
// Copyright (C) 2023 François Laignel <francois@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! AWS Transcriber element.
//!
//! This element calls AWS Transcribe to extract transcripts from an audio stream.
//! The element can optionally translate the resulting transcripts to one or
//! multiple languages.
//!
//! This module contains the element implementation as well as the `TranslateSrcPad`
//! sublcass and its `TranslationPadTask`.
//!
//! Web service specific code can be found in the `transcribe` and `translate` modules.

use gst::subclass::prelude::*;
use gst::{glib, prelude::*};

use aws_sdk_transcribestreaming as aws_transcribe;

use futures::channel::mpsc;
use futures::future::AbortHandle;
use futures::prelude::*;
use tokio::{runtime, sync::broadcast, task};

use std::collections::{BTreeSet, VecDeque};
use std::sync::Mutex;

use once_cell::sync::Lazy;

use super::transcribe::{TranscriberLoop, TranscriptEvent, TranscriptItem, TranscriptionSettings};
use super::translate::{TranslateLoop, TranslateQueue, TranslatedItem};
use super::{
    AwsTranscriberResultStability, AwsTranscriberVocabularyFilterMethod,
    TranslationTokenizationMethod, CAT,
};

static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
});

const DEFAULT_TRANSCRIBER_REGION: &str = "us-east-1";

// Deprecated in 0.11.0: due to evolutions of the transcriber element,
// this property has been replaced by `TRANSCRIBE_LATENCY_PROPERTY`.
const DEPRECATED_LATENCY_PROPERTY: &str = "latency";

const TRANSCRIBE_LATENCY_PROPERTY: &str = "transcribe-latency";
pub const DEFAULT_TRANSCRIBE_LATENCY: gst::ClockTime = gst::ClockTime::from_seconds(8);

const TRANSLATE_LATENCY_PROPERTY: &str = "translate-latency";
pub const DEFAULT_TRANSLATE_LATENCY: gst::ClockTime = gst::ClockTime::from_mseconds(500);

const TRANSLATE_LOOKAHEAD_PROPERTY: &str = "translate-lookahead";
pub const DEFAULT_TRANSLATE_LOOKAHEAD: gst::ClockTime = gst::ClockTime::from_seconds(5);

const DEFAULT_LATENESS: gst::ClockTime = gst::ClockTime::ZERO;
pub const DEFAULT_INPUT_LANG_CODE: &str = "en-US";

const DEFAULT_STABILITY: AwsTranscriberResultStability = AwsTranscriberResultStability::Low;
const DEFAULT_VOCABULARY_FILTER_METHOD: AwsTranscriberVocabularyFilterMethod =
    AwsTranscriberVocabularyFilterMethod::Mask;

// The period at which the event loops will check if they need to push
// anything downstream when no other events show up.
pub const GRANULARITY: gst::ClockTime = gst::ClockTime::from_mseconds(100);

const OUTPUT_LANG_CODE_PROPERTY: &str = "language-code";
const DEFAULT_OUTPUT_LANG_CODE: Option<&str> = None;

const TRANSLATION_TOKENIZATION_PROPERTY: &str = "tokenization-method";

#[derive(Debug, Clone)]
pub(super) struct Settings {
    transcribe_latency: gst::ClockTime,
    translate_latency: gst::ClockTime,
    translate_lookahead: gst::ClockTime,
    lateness: gst::ClockTime,
    pub language_code: String,
    pub vocabulary: Option<String>,
    pub session_id: Option<String>,
    pub results_stability: AwsTranscriberResultStability,
    access_key: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    pub vocabulary_filter: Option<String>,
    pub vocabulary_filter_method: AwsTranscriberVocabularyFilterMethod,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            transcribe_latency: DEFAULT_TRANSCRIBE_LATENCY,
            translate_latency: DEFAULT_TRANSLATE_LATENCY,
            translate_lookahead: DEFAULT_TRANSLATE_LOOKAHEAD,
            lateness: DEFAULT_LATENESS,
            language_code: DEFAULT_INPUT_LANG_CODE.to_string(),
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

pub(super) struct State {
    buffer_tx: Option<mpsc::Sender<gst::Buffer>>,
    transcriber_loop_handle: Option<task::JoinHandle<Result<(), gst::ErrorMessage>>>,
    srcpads: BTreeSet<super::TranslateSrcPad>,
    pad_serial: u32,
    pub seqnum: gst::Seqnum,
}

impl Default for State {
    fn default() -> Self {
        Self {
            buffer_tx: None,
            transcriber_loop_handle: None,
            srcpads: Default::default(),
            pad_serial: 0,
            seqnum: gst::Seqnum::next(),
        }
    }
}

pub struct Transcriber {
    static_srcpad: super::TranslateSrcPad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    pub(super) aws_config: Mutex<Option<aws_config::SdkConfig>>,
    // sender to broadcast transcript items to the translate src pads.
    transcript_event_tx: broadcast::Sender<TranscriptEvent>,
}

impl Transcriber {
    fn start_srcpad_tasks(&self, state: &State) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp: self, "Starting tasks");

        if self.static_srcpad.is_linked() {
            self.static_srcpad.imp().start_task()?;
        }

        for pad in state.srcpads.iter() {
            pad.imp().start_task()?;
        }

        gst::debug!(CAT, imp: self, "Tasks Started");

        Ok(())
    }

    fn stop_tasks(&self, state: &mut State) {
        gst::debug!(CAT, imp: self, "Stopping tasks");

        if self.static_srcpad.is_linked() {
            self.static_srcpad.imp().stop_task();
        }

        for pad in state.srcpads.iter() {
            pad.imp().stop_task();
        }

        // Terminate the audio buffer stream
        state.buffer_tx = None;

        if let Some(transcriber_loop_handle) = state.transcriber_loop_handle.take() {
            transcriber_loop_handle.abort();
        }

        gst::debug!(CAT, imp: self, "Tasks Stopped");
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj: pad, "Handling event {event:?}");

        use gst::EventView::*;
        match event.view() {
            Eos(_) => {
                // Terminate the audio buffer stream
                self.state.lock().unwrap().buffer_tx = None;

                true
            }
            FlushStart(_) => {
                gst::info!(CAT, imp: self, "Received flush start, disconnecting");
                let ret = gst::Pad::event_default(pad, Some(&*self.obj()), event);
                self.stop_tasks(&mut self.state.lock().unwrap());

                ret
            }
            FlushStop(_) => {
                gst::info!(CAT, imp: self, "Received flush stop, restarting task");

                if gst::Pad::event_default(pad, Some(&*self.obj()), event) {
                    let state = self.state.lock().unwrap();
                    match self.start_srcpad_tasks(&state) {
                        Err(err) => {
                            gst::error!(CAT, imp: self, "Failed to start srcpad tasks: {err}");
                            false
                        }
                        Ok(_) => true,
                    }
                } else {
                    false
                }
            }
            Segment(e) => {
                let format = e.segment().format();
                if format != gst::Format::Time {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Format,
                        ["Only Time segments supported, got {format:?}"]
                    );
                    return false;
                };

                self.state.lock().unwrap().seqnum = e.seqnum();

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

        self.ensure_connection();

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

    fn ensure_connection(&self) {
        let mut state = self.state.lock().unwrap();

        if state.buffer_tx.is_some() {
            return;
        }

        let settings = self.settings.lock().unwrap();

        let in_caps = self.sinkpad.current_caps().unwrap();
        let s = in_caps.structure(0).unwrap();
        let sample_rate = s.get::<i32>("rate").unwrap();

        let transcription_settings = TranscriptionSettings::from(&settings, sample_rate);

        let (buffer_tx, buffer_rx) = mpsc::channel(1);

        let transcriber_loop = TranscriberLoop::new(
            self,
            transcription_settings,
            settings.lateness,
            buffer_rx,
            self.transcript_event_tx.clone(),
        );
        let transcriber_loop_handle = RUNTIME.spawn(transcriber_loop.run());

        state.transcriber_loop_handle = Some(transcriber_loop_handle);
        state.buffer_tx = Some(buffer_tx);
    }

    fn prepare(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Preparing");

        let (access_key, secret_access_key, session_token);
        {
            let settings = self.settings.lock().unwrap();
            access_key = settings.access_key.to_owned();
            secret_access_key = settings.secret_access_key.to_owned();
            session_token = settings.session_token.to_owned();
        }

        gst::info!(CAT, imp: self, "Loading aws config...");
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

        *self.aws_config.lock().unwrap() = Some(config);

        gst::debug!(CAT, imp: self, "Prepared");

        Ok(())
    }

    fn disconnect(&self) {
        gst::info!(CAT, imp: self, "Unpreparing");
        let mut state = self.state.lock().unwrap();

        self.stop_tasks(&mut state);

        for pad in state.srcpads.iter() {
            pad.imp().set_discont();
        }
        gst::info!(CAT, imp: self, "Unprepared");
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Transcriber {
    const NAME: &'static str = "GstAwsTranscriber";
    type Type = super::Transcriber;
    type ParentType = gst::Element;
    type Interfaces = (gst::ChildProxy,);

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
        let static_srcpad =
            gst::PadBuilder::<super::TranslateSrcPad>::from_template(&templ, Some("src"))
                .activatemode_function(|pad, parent, mode, active| {
                    Transcriber::catch_panic_pad_function(
                        parent,
                        || {
                            Err(gst::loggable_error!(
                                CAT,
                                "Panic activating TranslateSrcPad"
                            ))
                        },
                        |elem| TranslateSrcPad::activatemode(elem, pad, mode, active),
                    )
                })
                .query_function(|pad, parent, query| {
                    Transcriber::catch_panic_pad_function(
                        parent,
                        || false,
                        |elem| TranslateSrcPad::src_query(elem, pad, query),
                    )
                })
                .flags(gst::PadFlags::FIXED_CAPS)
                .build();

        // Setting the channel capacity so that a TranslateSrcPad that would lag
        // behind for some reasons get a chance to catch-up without loosing items.
        // Receiver will be created by subscribing to sender later.
        let (transcript_event_tx, _) = broadcast::channel(128);

        Self {
            static_srcpad,
            sinkpad,
            settings: Default::default(),
            state: Default::default(),
            aws_config: Default::default(),
            transcript_event_tx,
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
                    .default_value(Some(DEFAULT_INPUT_LANG_CODE))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder(DEPRECATED_LATENCY_PROPERTY)
                    .nick("Latency")
                    .blurb("Amount of milliseconds to allow AWS transcribe (Deprecated. Use transcribe-latency)")
                    .default_value(DEFAULT_TRANSCRIBE_LATENCY.mseconds() as u32)
                    .mutable_ready()
                    .deprecated()
                    .build(),
                glib::ParamSpecUInt::builder(TRANSCRIBE_LATENCY_PROPERTY)
                    .nick("AWS Transcribe Latency")
                    .blurb("Amount of milliseconds to allow AWS transcribe")
                    .default_value(DEFAULT_TRANSCRIBE_LATENCY.mseconds() as u32)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder(TRANSLATE_LATENCY_PROPERTY)
                    .nick("AWS Translate Latency")
                    .blurb(concat!(
                        "Amount of milliseconds to allow AWS translate ",
                        "(ignored if the input and output languages are the same)",
                    ))
                    .default_value(DEFAULT_TRANSLATE_LATENCY.mseconds() as u32)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder(TRANSLATE_LOOKAHEAD_PROPERTY)
                    .nick("Translate lookahead")
                    .blurb(concat!(
                        "Maximum duration in milliseconds of transcript to lookahead ",
                        "before sending to translation when no separator was encountered",
                    ))
                    .default_value(DEFAULT_TRANSLATE_LOOKAHEAD.mseconds() as u32)
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
        obj.add_pad(&self.static_srcpad).unwrap();
        obj.set_element_flags(gst::ElementFlags::PROVIDE_CLOCK | gst::ElementFlags::REQUIRE_CLOCK);
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "language-code" => {
                let mut settings = self.settings.lock().unwrap();
                settings.language_code = value.get().expect("type checked upstream");
            }
            DEPRECATED_LATENCY_PROPERTY => {
                let mut settings = self.settings.lock().unwrap();
                settings.transcribe_latency = gst::ClockTime::from_mseconds(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            TRANSCRIBE_LATENCY_PROPERTY => {
                let mut settings = self.settings.lock().unwrap();
                settings.transcribe_latency = gst::ClockTime::from_mseconds(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            TRANSLATE_LATENCY_PROPERTY => {
                self.settings.lock().unwrap().translate_latency =
                    gst::ClockTime::from_mseconds(value.get::<u32>().unwrap().into());
            }
            TRANSLATE_LOOKAHEAD_PROPERTY => {
                self.settings.lock().unwrap().translate_lookahead =
                    gst::ClockTime::from_mseconds(value.get::<u32>().unwrap().into());
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
            DEPRECATED_LATENCY_PROPERTY => {
                let settings = self.settings.lock().unwrap();
                (settings.transcribe_latency.mseconds() as u32).to_value()
            }
            TRANSCRIBE_LATENCY_PROPERTY => {
                let settings = self.settings.lock().unwrap();
                (settings.transcribe_latency.mseconds() as u32).to_value()
            }
            TRANSLATE_LATENCY_PROPERTY => {
                (self.settings.lock().unwrap().translate_latency.mseconds() as u32).to_value()
            }
            TRANSLATE_LOOKAHEAD_PROPERTY => {
                (self.settings.lock().unwrap().translate_lookahead.mseconds() as u32).to_value()
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
            let src_pad_template = gst::PadTemplate::with_gtype(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
                super::TranslateSrcPad::static_type(),
            )
            .unwrap();
            let req_src_pad_template = gst::PadTemplate::with_gtype(
                "translate_src_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Request,
                &src_caps,
                super::TranslateSrcPad::static_type(),
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

            vec![src_pad_template, req_src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::info!(CAT, imp: self, "Changing state {transition:?}");

        if let gst::StateChange::NullToReady = transition {
            self.prepare().map_err(|err| {
                self.post_error_message(err);
                gst::StateChangeError
            })?;
        }

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

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        _name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let mut state = self.state.lock().unwrap();

        let pad = gst::PadBuilder::<super::TranslateSrcPad>::from_template(
            templ,
            Some(format!("translate_src_{}", state.pad_serial).as_str()),
        )
        .activatemode_function(|pad, parent, mode, active| {
            Transcriber::catch_panic_pad_function(
                parent,
                || {
                    Err(gst::loggable_error!(
                        CAT,
                        "Panic activating TranslateSrcPad"
                    ))
                },
                |elem| TranslateSrcPad::activatemode(elem, pad, mode, active),
            )
        })
        .query_function(|pad, parent, query| {
            Transcriber::catch_panic_pad_function(
                parent,
                || false,
                |elem| TranslateSrcPad::src_query(elem, pad, query),
            )
        })
        .flags(gst::PadFlags::FIXED_CAPS)
        .build();

        state.srcpads.insert(pad.clone());

        state.pad_serial += 1;
        drop(state);

        self.obj().add_pad(&pad).unwrap();

        let _ = self
            .obj()
            .post_message(gst::message::Latency::builder().src(&*self.obj()).build());

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

    fn provide_clock(&self) -> Option<gst::Clock> {
        Some(gst::SystemClock::obtain())
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
struct TranslationPadTask {
    pad: glib::subclass::ObjectImplRef<TranslateSrcPad>,
    elem: super::Transcriber,
    transcript_event_rx: broadcast::Receiver<TranscriptEvent>,
    needs_translate: bool,
    translate_queue: TranslateQueue,
    translate_loop_handle: Option<task::JoinHandle<Result<(), gst::ErrorMessage>>>,
    to_translate_tx: Option<mpsc::Sender<Vec<TranscriptItem>>>,
    from_translate_rx: Option<mpsc::Receiver<Vec<TranslatedItem>>>,
    translate_latency: gst::ClockTime,
    translate_lookahead: gst::ClockTime,
    send_events: bool,
    translated_items: VecDeque<TranslatedItem>,
    our_latency: gst::ClockTime,
    seqnum: gst::Seqnum,
    send_eos: bool,
    pending_translations: usize,
    start_time: Option<gst::ClockTime>,
}

impl TranslationPadTask {
    fn try_new(
        pad: &TranslateSrcPad,
        elem: super::Transcriber,
        transcript_event_rx: broadcast::Receiver<TranscriptEvent>,
    ) -> Result<TranslationPadTask, gst::ErrorMessage> {
        let mut this = TranslationPadTask {
            pad: pad.ref_counted(),
            elem,
            transcript_event_rx,
            needs_translate: false,
            translate_queue: TranslateQueue::default(),
            translate_loop_handle: None,
            to_translate_tx: None,
            from_translate_rx: None,
            translate_latency: DEFAULT_TRANSLATE_LATENCY,
            translate_lookahead: DEFAULT_TRANSLATE_LOOKAHEAD,
            send_events: true,
            translated_items: VecDeque::new(),
            our_latency: DEFAULT_TRANSCRIBE_LATENCY,
            seqnum: gst::Seqnum::next(),
            send_eos: false,
            pending_translations: 0,
            start_time: None,
        };

        let _enter_guard = RUNTIME.enter();
        futures::executor::block_on(this.init_translate())?;

        Ok(this)
    }
}

impl Drop for TranslationPadTask {
    fn drop(&mut self) {
        if let Some(translate_loop_handle) = self.translate_loop_handle.take() {
            translate_loop_handle.abort();
        }
    }
}

impl TranslationPadTask {
    async fn run_iter(&mut self) -> Result<(), gst::ErrorMessage> {
        self.ensure_init_events()?;

        if self.needs_translate {
            self.translate_iter().await?;
        } else {
            self.passthrough_iter().await?;
        }

        if !self.dequeue().await {
            gst::info!(CAT, imp: self.pad, "Failed to dequeue buffer, pausing");
            let _ = self.pad.obj().pause_task();
        }

        Ok(())
    }

    async fn passthrough_iter(&mut self) -> Result<(), gst::ErrorMessage> {
        // This is to make sure we send items on a timely basis or at least Gap events.
        let timeout = tokio::time::sleep(GRANULARITY.into()).fuse();
        futures::pin_mut!(timeout);

        let transcript_event_rx = self.transcript_event_rx.recv().fuse();
        futures::pin_mut!(transcript_event_rx);

        // `timeout` takes precedence over `transcript_events` reception
        // because we may need to `dequeue` `items` or push a `Gap` event
        // before current latency budget is exhausted.
        futures::select_biased! {
            _ = timeout => (),
            items_res = transcript_event_rx => {
                use TranscriptEvent::*;
                use broadcast::error::RecvError;
                match items_res {
                    Ok(Items(transcript_items)) => {
                        for transcript_item in transcript_items.iter() {
                            self.translated_items.push_back(transcript_item.into());
                        }
                    }
                    Ok(Eos) => {
                        gst::debug!(CAT, imp: self.pad, "Got eos");
                        self.send_eos = true;
                    }
                    Err(RecvError::Lagged(nb_msg)) => {
                        gst::warning!(CAT, imp: self.pad, "Missed {nb_msg} transcript sets");
                    }
                    Err(RecvError::Closed) => {
                        gst::debug!(CAT, imp: self.pad, "Transcript chan terminated: setting eos");
                        self.send_eos = true;
                    }
                }
            }
        }

        Ok(())
    }

    async fn translate_iter(&mut self) -> Result<(), gst::ErrorMessage> {
        if self
            .translate_loop_handle
            .as_ref()
            .map_or(true, task::JoinHandle::is_finished)
        {
            const ERR: &str = "Translate loop is not running";
            gst::error!(CAT, imp: self.pad, "{ERR}");
            return Err(gst::error_msg!(gst::StreamError::Failed, ["{ERR}"]));
        }

        let transcript_items = {
            // This is to make sure we send items on a timely basis or at least Gap events.
            let timeout = tokio::time::sleep(GRANULARITY.into()).fuse();
            futures::pin_mut!(timeout);

            let from_translate_rx = self
                .from_translate_rx
                .as_mut()
                .expect("from_translation chan must be available in translation mode");

            let transcript_event_rx = self.transcript_event_rx.recv().fuse();
            futures::pin_mut!(transcript_event_rx);

            // `timeout` takes precedence over `transcript_events` reception
            // because we may need to `dequeue` `items` or push a `Gap` event
            // before current latency budget is exhausted.
            futures::select_biased! {
                _ = timeout => return Ok(()),
                translated_items = from_translate_rx.next() => {
                    let Some(translated_items) = translated_items else {
                        const ERR: &str = "translation chan terminated";
                        gst::debug!(CAT, imp: self.pad, "{ERR}");
                        return Err(gst::error_msg!(gst::StreamError::Failed, ["{ERR}"]));
                    };

                    self.translated_items.extend(translated_items);
                    self.pending_translations = self.pending_translations.saturating_sub(1);

                    return Ok(());
                }
                items_res = transcript_event_rx => {
                    use TranscriptEvent::*;
                    use broadcast::error::RecvError;
                    match items_res {
                        Ok(Items(transcript_items)) => transcript_items,
                        Ok(Eos) => {
                            gst::debug!(CAT, imp: self.pad, "Got eos");
                            self.send_eos = true;
                            return Ok(());
                        }
                        Err(RecvError::Lagged(nb_msg)) => {
                            gst::warning!(CAT, imp: self.pad, "Missed {nb_msg} transcript sets");
                            return Ok(());
                        }
                        Err(RecvError::Closed) => {
                            gst::debug!(CAT, imp: self.pad, "Transcript chan terminated: setting eos");
                            self.send_eos = true;
                            return Ok(());
                        }
                    }
                }
            }
        };

        for items in transcript_items.iter() {
            if let Some(ready_items) = self.translate_queue.push(items) {
                self.send_for_translation(ready_items).await?;
            }
        }

        Ok(())
    }

    async fn dequeue(&mut self) -> bool {
        let (now, start_time, mut last_position, mut discont_pending);
        {
            let mut pad_state = self.pad.state.lock().unwrap();

            let Some(cur_rt) = self.elem.current_running_time() else {
                // Wait for the clock to be available
                return true;
            };
            now = cur_rt;

            if self.start_time.is_none() {
                self.start_time = Some(now);
                pad_state.out_segment.set_position(now);
            }

            start_time = self.start_time.unwrap();
            last_position = pad_state.out_segment.position().unwrap();
            discont_pending = pad_state.discont_pending;
        }

        if self.needs_translate && !self.translate_queue.is_empty() {
            // Maximum delay for an item to be pushed to stream on time
            // Margin:
            // - 1 * GRANULARITY: the time it will take before we can check this again,
            //   without running late, in the case of a timeout.
            // - 2 * GRANULARITY: extra margin to account for additional overheads.
            //   FIXME explaing which ones.
            let max_delay = self.our_latency.saturating_sub(3 * GRANULARITY);

            // Estimated time of arrival for an item sent to translation now.
            // (in transcript item ts base)
            let translation_eta = now + self.translate_latency - start_time;

            let deadline = translation_eta.saturating_sub(max_delay);

            if let Some(ready_items) = self
                .translate_queue
                .dequeue(deadline, self.translate_lookahead)
            {
                gst::debug!(CAT, imp: self.pad, "Forcing  {} transcripts to translation", ready_items.len());
                if self.send_for_translation(ready_items).await.is_err() {
                    return false;
                }
            }
        }

        /* First, check our pending buffers */
        while let Some(item) = self.translated_items.front() {
            // Note: items pts start from 0 + lateness
            gst::trace!(
                CAT,
                imp: self.pad,
                "Checking now {now} if item is ready for dequeuing, PTS {}, threshold {} vs {}",
                item.pts,
                item.pts + self.our_latency.saturating_sub(3 * GRANULARITY),
                now - start_time
            );

            // Margin:
            // - 1 * GRANULARITY: the time it will take before we can check this again,
            //   without running late, in the case of a timeout.
            // - 2 * GRANULARITY: extra margin to account for additional overheads.
            //   FIXME explaing which ones.
            if item.pts + self.our_latency.saturating_sub(3 * GRANULARITY) < now - start_time {
                /* Safe unwrap, we know we have an item */
                let TranslatedItem {
                    pts: item_pts,
                    mut duration,
                    content,
                } = self.translated_items.pop_front().unwrap();

                let mut pts = start_time + item_pts;

                let mut buf = gst::Buffer::from_mut_slice(content.into_bytes());
                {
                    let buf = buf.get_mut().unwrap();

                    if discont_pending {
                        buf.set_flags(gst::BufferFlags::DISCONT);
                        discont_pending = false;
                    }

                    buf.set_pts(pts);
                    buf.set_duration(duration);
                }

                use std::cmp::Ordering::*;
                match pts.cmp(&last_position) {
                    Greater => {
                        // The buffer we are about to push starts after the end of
                        // last item previously pushed to the stream.
                        let gap_event = gst::event::Gap::builder(last_position)
                            .duration(pts - last_position)
                            .seqnum(self.seqnum)
                            .build();
                        gst::log!(CAT, imp: self.pad, "Pushing gap:    {last_position} -> {pts}");
                        if !self.pad.obj().push_event(gap_event) {
                            return false;
                        }
                    }
                    Less => {
                        // The buffer we are about to push was expected to start
                        // before the end of last item previously pushed to the stream.
                        // => update it to fit in stream.
                        let delta = last_position - pts;

                        gst::warning!(
                            CAT,
                            imp: self.pad,
                            "Updating item PTS ({pts} < {last_position}), consider increasing latency",
                        );

                        pts = last_position;
                        // FIXME if the resulting duration is zero, we might as well not push it.
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

                gst::debug!(CAT, imp: self.pad, "Pushing buffer: {pts} -> {}", pts + duration);

                if self.pad.obj().push(buf).is_err() {
                    return false;
                }
            } else {
                // Current and subsequent items are not ready to be pushed
                break;
            }
        }

        if self.send_eos
            && self.pending_translations == 0
            && self.translated_items.is_empty()
            && self.translate_queue.is_empty()
        {
            /* We're EOS, we can pause and exit early */
            let _ = self.pad.obj().pause_task();

            gst::info!(CAT, imp: self.pad, "Sending eos");
            return self
                .pad
                .obj()
                .push_event(gst::event::Eos::builder().seqnum(self.seqnum).build());
        }

        /* next, push a gap if we're lagging behind the target position */
        gst::trace!(
            CAT,
            imp: self.pad,
            "Checking now: {now} if we need to push a gap, last_position: {last_position}, threshold: {}",
            last_position + self.our_latency.saturating_sub(GRANULARITY)
        );

        if now > last_position + self.our_latency.saturating_sub(GRANULARITY) {
            // We are running out of latency budget since last time we pushed downstream,
            // so push a Gap long enough to keep continuity before we dequeue again:
            // worse case scenario, this is GRANULARITY ms from now.
            let duration = now - last_position - self.our_latency.saturating_sub(GRANULARITY);

            let gap_event = gst::event::Gap::builder(last_position)
                .duration(duration)
                .seqnum(self.seqnum)
                .build();

            gst::log!(
                CAT,
                imp: self.pad,
                "Pushing gap:    {last_position} -> {}",
                last_position + duration
            );

            last_position += duration;

            if !self.pad.obj().push_event(gap_event) {
                return false;
            }
        }

        let mut pad_state = self.pad.state.lock().unwrap();
        pad_state.out_segment.set_position(last_position);
        pad_state.discont_pending = discont_pending;

        true
    }

    async fn send_for_translation(
        &mut self,
        transcript_items: Vec<TranscriptItem>,
    ) -> Result<(), gst::ErrorMessage> {
        let res = self
            .to_translate_tx
            .as_mut()
            .expect("to_translation chan must be available in translation mode")
            .send(transcript_items)
            .await;

        if res.is_err() {
            const ERR: &str = "to_translation chan terminated";
            gst::debug!(CAT, imp: self.pad, "{ERR}");
            return Err(gst::error_msg!(gst::StreamError::Failed, ["{ERR}"]));
        }

        self.pending_translations += 1;

        Ok(())
    }

    fn ensure_init_events(&mut self) -> Result<(), gst::ErrorMessage> {
        if !self.send_events {
            return Ok(());
        }

        let mut events = vec![];

        {
            let elem_imp = self.elem.imp();
            let elem_state = elem_imp.state.lock().unwrap();

            let mut pad_state = self.pad.state.lock().unwrap();

            self.seqnum = elem_state.seqnum;
            pad_state.out_segment = Default::default();

            events.push(
                gst::event::StreamStart::builder("transcription")
                    .seqnum(self.seqnum)
                    .build(),
            );

            let caps = gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build();
            events.push(gst::event::Caps::builder(&caps).seqnum(self.seqnum).build());

            events.push(
                gst::event::Segment::builder(&pad_state.out_segment)
                    .seqnum(self.seqnum)
                    .build(),
            );
        }

        for event in events.drain(..) {
            gst::info!(CAT, imp: self.pad, "Sending {event:?}");
            if !self.pad.obj().push_event(event) {
                const ERR: &str = "Failed to send initial";
                gst::error!(CAT, imp: self.pad, "{ERR}");
                return Err(gst::error_msg!(gst::StreamError::Failed, ["{ERR}"]));
            }
        }

        self.send_events = false;

        Ok(())
    }
}

impl TranslationPadTask {
    async fn init_translate(&mut self) -> Result<(), gst::ErrorMessage> {
        let mut translation_loop = None;

        {
            let elem_imp = self.elem.imp();
            let elem_settings = elem_imp.settings.lock().unwrap();

            let pad_settings = self.pad.settings.lock().unwrap();

            self.our_latency = TranslateSrcPad::our_latency(&elem_settings, &pad_settings);
            if self.our_latency + elem_settings.lateness <= 2 * GRANULARITY {
                let err = format!(
                    "total latency + lateness must be greater than {}",
                    2 * GRANULARITY
                );
                gst::error!(CAT, imp: self.pad, "{err}");
                return Err(gst::error_msg!(gst::LibraryError::Settings, ["{err}"]));
            }

            self.translate_latency = elem_settings.translate_latency;
            self.translate_lookahead = elem_settings.translate_lookahead;

            self.needs_translate = TranslateSrcPad::needs_translation(
                &elem_settings.language_code,
                pad_settings.language_code.as_deref(),
            );

            if self.needs_translate {
                let (to_translate_tx, to_translate_rx) = mpsc::channel(64);
                let (from_translate_tx, from_translate_rx) = mpsc::channel(64);

                translation_loop = Some(TranslateLoop::new(
                    elem_imp,
                    &self.pad,
                    &elem_settings.language_code,
                    pad_settings.language_code.as_deref().unwrap(),
                    pad_settings.tokenization_method,
                    to_translate_rx,
                    from_translate_tx,
                ));

                self.to_translate_tx = Some(to_translate_tx);
                self.from_translate_rx = Some(from_translate_rx);
            }
        }

        if let Some(translation_loop) = translation_loop {
            translation_loop.check_language().await?;
            self.translate_loop_handle = Some(RUNTIME.spawn(translation_loop.run()));
        }

        Ok(())
    }
}

#[derive(Debug)]
struct TranslationPadState {
    discont_pending: bool,
    out_segment: gst::FormattedSegment<gst::ClockTime>,
    task_abort_handle: Option<AbortHandle>,
}

impl Default for TranslationPadState {
    fn default() -> TranslationPadState {
        TranslationPadState {
            discont_pending: true,
            out_segment: Default::default(),
            task_abort_handle: None,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct TranslatePadSettings {
    language_code: Option<String>,
    tokenization_method: TranslationTokenizationMethod,
}

#[derive(Debug, Default)]
pub struct TranslateSrcPad {
    state: Mutex<TranslationPadState>,
    settings: Mutex<TranslatePadSettings>,
}

impl TranslateSrcPad {
    fn start_task(&self) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp: self, "Starting task");

        let elem = self.parent();
        let transcript_event_rx = elem.imp().transcript_event_tx.subscribe();
        let mut pad_task = TranslationPadTask::try_new(self, elem, transcript_event_rx)
            .map_err(|err| gst::loggable_error!(CAT, format!("Failed to start pad task {err}")))?;

        let imp = self.ref_counted();
        let res = self.obj().start_task(move || {
            let (abortable_task_iter, abort_handle) = future::abortable(pad_task.run_iter());
            imp.state.lock().unwrap().task_abort_handle = Some(abort_handle);

            let _enter = RUNTIME.enter();
            match futures::executor::block_on(abortable_task_iter) {
                Ok(Ok(())) => (),
                Ok(Err(err)) => {
                    // Don't bring down the whole element if this Pad fails
                    // FIXME is there a way to mark the Pad in error though?
                    gst::info!(CAT, imp: imp, "Pausing task due to: {err}");
                    let _ = imp.obj().pause_task();
                }
                Err(_) => gst::debug!(CAT, imp: imp, "task iter aborted"),
            }
        });

        if res.is_err() {
            return Err(gst::loggable_error!(CAT, "Failed to start pad task"));
        }

        gst::debug!(CAT, imp: self, "Task started");

        Ok(())
    }

    fn stop_task(&self) {
        gst::debug!(CAT, imp: self, "Stopping task");

        // See also the note in `start_task()`:
        // 1. Mark the task as stopped so no further iteration is executed.
        let _ = self.obj().stop_task();

        // 2. Abort the task iteration if the Future is pending.
        if let Some(task_abort_handle) = self.state.lock().unwrap().task_abort_handle.take() {
            task_abort_handle.abort();
        }

        gst::debug!(CAT, imp: self, "Task stopped");
    }

    fn set_discont(&self) {
        self.state.lock().unwrap().discont_pending = true;
    }

    #[inline]
    fn needs_translation(input_lang: &str, output_lang: Option<&str>) -> bool {
        output_lang.map_or(false, |other| {
            !input_lang.eq_ignore_ascii_case(other.as_ref())
        })
    }

    #[inline]
    fn our_latency(
        elem_settings: &Settings,
        pad_settings: &TranslatePadSettings,
    ) -> gst::ClockTime {
        if Self::needs_translation(
            &elem_settings.language_code,
            pad_settings.language_code.as_deref(),
        ) {
            elem_settings.transcribe_latency
                + elem_settings.translate_lookahead
                + elem_settings.translate_latency
        } else {
            elem_settings.transcribe_latency
        }
    }

    #[track_caller]
    fn parent(&self) -> super::Transcriber {
        self.obj()
            .parent()
            .map(|elem_obj| {
                elem_obj
                    .downcast::<super::Transcriber>()
                    .expect("Wrong Element type")
            })
            .expect("Pad should have a parent at this stage")
    }
}

impl TranslateSrcPad {
    #[track_caller]
    pub fn activatemode(
        _elem: &Transcriber,
        pad: &super::TranslateSrcPad,
        _mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if active {
            pad.imp().start_task()?;
        } else {
            pad.imp().stop_task();
        }

        Ok(())
    }

    pub fn src_query(
        elem: &Transcriber,
        pad: &super::TranslateSrcPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst::log!(CAT, obj: pad, "Handling query {query:?}");

        use gst::QueryViewMut::*;
        match query.view_mut() {
            Latency(q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = elem.sinkpad.peer_query(&mut peer_query);

                if ret {
                    let (_, min, _) = peer_query.result();

                    let our_latency = {
                        let elem_settings = elem.settings.lock().unwrap();
                        let pad_settings = pad.imp().settings.lock().unwrap();

                        Self::our_latency(&elem_settings, &pad_settings)
                    };

                    gst::info!(CAT, obj: pad, "Our latency {our_latency}");
                    q.set(true, our_latency + min, gst::ClockTime::NONE);
                }
                ret
            }
            Position(q) => {
                if q.format() == gst::Format::Time {
                    let stream_time = {
                        let state = pad.imp().state.lock().unwrap();
                        state
                            .out_segment
                            .to_stream_time(state.out_segment.position())
                    };

                    let Some(stream_time) = stream_time else { return false };
                    q.set(stream_time);

                    true
                } else {
                    false
                }
            }
            _ => gst::Pad::query_default(pad, Some(pad), query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TranslateSrcPad {
    const NAME: &'static str = "GstTranslateSrcPad";
    type Type = super::TranslateSrcPad;
    type ParentType = gst::Pad;

    fn new() -> Self {
        Default::default()
    }
}

impl ObjectImpl for TranslateSrcPad {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder(OUTPUT_LANG_CODE_PROPERTY)
                    .nick("Language Code")
                    .blurb("The Language the Stream must be translated to")
                    .default_value(DEFAULT_OUTPUT_LANG_CODE)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder(TRANSLATION_TOKENIZATION_PROPERTY)
                    .nick("Translations tokenization method")
                    .blurb("The tokenization method to apply to translations")
                    .default_value(TranslationTokenizationMethod::default())
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            OUTPUT_LANG_CODE_PROPERTY => {
                self.settings.lock().unwrap().language_code = value.get().unwrap()
            }
            TRANSLATION_TOKENIZATION_PROPERTY => {
                self.settings.lock().unwrap().tokenization_method = value.get().unwrap()
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            OUTPUT_LANG_CODE_PROPERTY => self.settings.lock().unwrap().language_code.to_value(),
            TRANSLATION_TOKENIZATION_PROPERTY => {
                self.settings.lock().unwrap().tokenization_method.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for TranslateSrcPad {}

impl PadImpl for TranslateSrcPad {}
