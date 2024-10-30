// Copyright (C) 2024 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! AWS Polly element.
//!
//! This element calls AWS Polly to generate audio speech from text.

use gst::subclass::prelude::*;
use gst::{glib, prelude::*};

use aws_sdk_s3::config::StalledStreamProtectionConfig;

use futures::future::{abortable, AbortHandle};

use std::sync::Mutex;

use std::sync::LazyLock;

use super::{AwsPollyEngine, AwsPollyLanguageCode, AwsPollyVoiceId, CAT};
use crate::s3utils::RUNTIME;
use anyhow::{anyhow, Error};

#[allow(deprecated)]
static AWS_BEHAVIOR_VERSION: LazyLock<aws_config::BehaviorVersion> =
    LazyLock::new(aws_config::BehaviorVersion::v2023_11_09);

const DEFAULT_REGION: &str = "us-east-1";
const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::from_seconds(2);
const DEFAULT_ENGINE: AwsPollyEngine = AwsPollyEngine::Neural;
const DEFAULT_LANGUAGE_CODE: AwsPollyLanguageCode = AwsPollyLanguageCode::None;
const DEFAULT_VOICE_ID: AwsPollyVoiceId = AwsPollyVoiceId::Aria;

#[derive(Debug, Clone)]
pub(super) struct Settings {
    latency: gst::ClockTime,
    access_key: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    engine: AwsPollyEngine,
    language_code: AwsPollyLanguageCode,
    voice_id: AwsPollyVoiceId,
    lexicon_names: gst::Array,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            latency: DEFAULT_LATENCY,
            access_key: None,
            secret_access_key: None,
            session_token: None,
            engine: DEFAULT_ENGINE,
            language_code: DEFAULT_LANGUAGE_CODE,
            voice_id: DEFAULT_VOICE_ID,
            lexicon_names: gst::Array::default(),
        }
    }
}

struct State {
    out_segment: gst::FormattedSegment<gst::ClockTime>,
    client: Option<aws_sdk_polly::Client>,
    send_abort_handle: Option<AbortHandle>,
    in_format: Option<aws_sdk_polly::types::TextType>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            out_segment: gst::FormattedSegment::new(),
            client: None,
            send_abort_handle: None,
            in_format: None,
        }
    }
}

pub struct Polly {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    pub(super) aws_config: Mutex<Option<aws_config::SdkConfig>>,
}

impl Polly {
    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling event {event:?}");

        use gst::EventView::*;
        match event.view() {
            FlushStart(_) => {
                gst::info!(CAT, imp = self, "Received flush start, disconnecting");
                let ret = gst::Pad::event_default(pad, Some(&*self.obj()), event);
                self.disconnect();
                ret
            }
            Segment(e) => {
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
                state.out_segment = segment;
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            Caps(c) => {
                let format = c.caps().structure(0).map(|s| s.name().as_str());
                let mut state = self.state.lock().unwrap();

                state.in_format = format.and_then(|f| match f {
                    "text/x-raw" => Some(aws_sdk_polly::types::TextType::Text),
                    "application/ssml+xml" => Some(aws_sdk_polly::types::TextType::Ssml),
                    _ => None,
                });

                drop(state);

                let caps = gst_audio::AudioCapsBuilder::new()
                    .format(gst_audio::AudioFormat::S16le)
                    .rate(16_000)
                    .channels(1)
                    .layout(gst_audio::AudioLayout::Interleaved)
                    .build();

                let event = gst::event::Caps::builder(&caps).seqnum(c.seqnum()).build();

                self.srcpad.push_event(event)
            }
            Gap(g) => {
                let (pts, duration) = g.get();

                let mut state = self.state.lock().unwrap();
                state.out_segment.set_position(match duration {
                    Some(duration) => duration + pts,
                    _ => pts,
                });
                drop(state);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    async fn send(&self, inbuf: gst::Buffer) -> Result<gst::Buffer, Error> {
        let pts = inbuf
            .pts()
            .ok_or_else(|| anyhow!("Stream with timestamped buffers required"))?;

        let duration = inbuf
            .duration()
            .ok_or_else(|| anyhow!("Buffers of stream need to have a duration"))?;

        let data = inbuf
            .map_readable()
            .map_err(|_| anyhow!("Can't map buffer readable"))?;

        let data =
            std::str::from_utf8(&data).map_err(|err| anyhow!("Can't decode utf8: {}", err))?;

        let (client, in_format) = {
            let state = self.state.lock().unwrap();

            (
                state.client.as_ref().expect("connected").clone(),
                state.in_format.as_ref().expect("received caps").clone(),
            )
        };

        let job = {
            let settings = self.settings.lock().unwrap();

            let mut task = client
                .synthesize_speech()
                .engine(settings.engine.into())
                .output_format(aws_sdk_polly::types::OutputFormat::Pcm)
                .text_type(in_format)
                .text(data)
                .voice_id(settings.voice_id.into())
                .set_lexicon_names(Some(
                    settings
                        .lexicon_names
                        .iter()
                        .map(|v| v.get::<String>().unwrap())
                        .collect(),
                ));

            if settings.language_code != AwsPollyLanguageCode::None {
                task = task.language_code(settings.language_code.into());
            }

            task.send()
        };

        let resp = job.await.map_err(|err| {
            if let Some(err) = err.as_service_error() {
                gst::error!(CAT, imp = self, "Failed sending text chunk: {}", err.meta());
            } else {
                gst::error!(CAT, imp = self, "Failed sending text chunk: {}", err);
            }
            err
        })?;
        let blob = resp.audio_stream.collect().await?;

        let mut buf = gst::Buffer::from_slice(blob.into_bytes());
        let mut state = self.state.lock().unwrap();

        let discont = state
            .out_segment
            .position()
            .map(|position| position < pts + duration)
            .unwrap_or(true);

        {
            let buf_mut = buf.get_mut().unwrap();
            buf_mut.set_pts(pts);
            buf_mut.set_duration(duration);

            if discont {
                gst::log!(CAT, imp = self, "Marking buffer discont");
                buf_mut.set_flags(gst::BufferFlags::DISCONT);
            }
            inbuf.foreach_meta(|meta| {
                if meta.tags().is_empty() {
                    if let Err(err) =
                        meta.transform(buf_mut, &gst::meta::MetaTransformCopy::new(false, ..))
                    {
                        gst::trace!(CAT, imp = self, "Could not copy meta {}: {err}", meta.api());
                    }
                }
                std::ops::ControlFlow::Continue(())
            });
        }

        state.out_segment.set_position(pts + duration);

        Ok(buf)
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling {buffer:?}");

        self.ensure_connection().map_err(|err| {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["Streaming failed: {err}"]);
            gst::FlowError::Error
        })?;

        let (future, abort_handle) = abortable(self.send(buffer));

        self.state.lock().unwrap().send_abort_handle = Some(abort_handle);

        match RUNTIME.block_on(future) {
            Err(_) => {
                gst::debug!(CAT, imp = self, "send aborted, returning flushing");
                Err(gst::FlowError::Flushing)
            }
            Ok(res) => match res {
                Err(e) => {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Failed,
                        ["Failed sending data: {}", e]
                    );
                    Err(gst::FlowError::Error)
                }
                Ok(buf) => self.srcpad.push(buf),
            },
        }
    }

    fn ensure_connection(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        if state.client.is_none() {
            state.client = Some(aws_sdk_polly::Client::new(
                self.aws_config.lock().unwrap().as_ref().expect("prepared"),
            ));
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

        gst::info!(CAT, imp = self, "Loading aws config...");
        let _enter_guard = RUNTIME.enter();

        let config_loader = match (access_key, secret_access_key) {
            (Some(key), Some(secret_key)) => {
                gst::debug!(CAT, imp = self, "Using settings credentials");
                aws_config::defaults(*AWS_BEHAVIOR_VERSION).credentials_provider(
                    aws_sdk_polly::config::Credentials::new(
                        key,
                        secret_key,
                        session_token,
                        None,
                        "translate",
                    ),
                )
            }
            _ => {
                gst::debug!(CAT, imp = self, "Attempting to get credentials from env...");
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
        gst::debug!(CAT, imp = self, "Using region {}", config.region().unwrap());

        *self.aws_config.lock().unwrap() = Some(config);

        gst::debug!(CAT, imp = self, "Prepared");

        Ok(())
    }

    fn disconnect(&self) {
        gst::info!(CAT, imp = self, "Disconnecting");
        let mut state = self.state.lock().unwrap();

        if let Some(abort_handle) = state.send_abort_handle.take() {
            abort_handle.abort();
        }

        *state = State::default();
        gst::info!(CAT, imp = self, "Disconnected");
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling query {:?}", query);

        match query.view_mut() {
            gst::QueryViewMut::Latency(ref mut q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = self.sinkpad.peer_query(&mut peer_query);

                if ret {
                    let (live, min, max) = peer_query.result();
                    let our_latency = self.settings.lock().unwrap().latency;

                    if live {
                        q.set(true, min + our_latency, max.map(|max| max + our_latency));
                    } else {
                        q.set(live, min, max);
                    }
                }
                ret
            }
            gst::QueryViewMut::Position(ref mut q) => {
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
}

#[glib::object_subclass]
impl ObjectSubclass for Polly {
    const NAME: &'static str = "GstAwsPolly";
    type Type = super::Polly;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Polly::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |polly| polly.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Polly::catch_panic_pad_function(
                    parent,
                    || false,
                    |polly| polly.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::PadBuilder::<gst::Pad>::from_template(&templ)
            .query_function(|pad, parent, query| {
                Polly::catch_panic_pad_function(
                    parent,
                    || false,
                    |polly| polly.src_query(pad, query),
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

impl ObjectImpl for Polly {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("latency")
                    .nick("Latency")
                    .blurb("Amount of milliseconds to allow AWS Polly")
                    .default_value(DEFAULT_LATENCY.mseconds() as u32)
                    .mutable_ready()
                    .deprecated()
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
                glib::ParamSpecEnum::builder_with_default("engine", DEFAULT_ENGINE)
                    .nick("Engine")
                    .blurb("Defines what engine to use")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("voice-id", DEFAULT_VOICE_ID)
                    .nick("Voice Id")
                    .blurb("Defines what voice id to use")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("language-code", DEFAULT_LANGUAGE_CODE)
                    .nick("Language Code")
                    .blurb("Defines what language code to use")
                    .mutable_ready()
                    .build(),
                gst::ParamSpecArray::builder("lexicon-names")
                    .nick("Lexicon Names")
                    .blurb("List of lexicon names to use")
                    .element_spec(
                        &glib::ParamSpecString::builder("lexicon-name")
                            .nick("Lexicon Name")
                            .blurb("The lexicon name")
                            .build(),
                    )
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
            "engine" => {
                let mut settings = self.settings.lock().unwrap();
                settings.engine = value
                    .get::<AwsPollyEngine>()
                    .expect("type checked upstream");
            }
            "voice-id" => {
                let mut settings = self.settings.lock().unwrap();
                settings.voice_id = value
                    .get::<AwsPollyVoiceId>()
                    .expect("type checked upstream");
            }
            "language-code" => {
                let mut settings = self.settings.lock().unwrap();
                settings.language_code = value
                    .get::<AwsPollyLanguageCode>()
                    .expect("type checked upstream");
            }
            "lexicon-names" => {
                let mut settings = self.settings.lock().unwrap();
                settings.lexicon_names = value.get::<gst::Array>().expect("type checked upstream");
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
            "engine" => {
                let settings = self.settings.lock().unwrap();
                settings.engine.to_value()
            }
            "voice-id" => {
                let settings = self.settings.lock().unwrap();
                settings.voice_id.to_value()
            }
            "language-code" => {
                let settings = self.settings.lock().unwrap();
                settings.language_code.to_value()
            }
            "lexicon-names" => {
                let settings = self.settings.lock().unwrap();
                settings.lexicon_names.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Polly {}

impl ElementImpl for Polly {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Polly",
                "Audio/Text/Filter",
                "Text to Speech filter, using AWS polly",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst::Caps::builder_full()
                .structure(
                    gst::Structure::builder("text/x-raw")
                        .field("format", "utf8")
                        .build(),
                )
                .structure(gst::Structure::new_empty("application/ssml+xml"))
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst_audio::AudioCapsBuilder::new()
                .format(gst_audio::AudioFormat::S16le)
                .rate(16_000)
                .channels(1)
                .layout(gst_audio::AudioLayout::Interleaved)
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
        gst::info!(CAT, imp = self, "Changing state {transition:?}");

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare().map_err(|err| {
                    self.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PausedToReady => {
                self.disconnect();
            }
            _ => (),
        }

        self.parent_change_state(transition)
    }

    fn provide_clock(&self) -> Option<gst::Clock> {
        Some(gst::SystemClock::obtain())
    }
}
