// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
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
use gst::{gst_debug, gst_error, gst_info, gst_warning};

use std::convert::TryFrom;
use std::mem;
use std::sync::Mutex;
use std::time::Instant;

use once_cell::sync::Lazy;

use super::custom_source::CustomSource;
use super::video_fallback::VideoFallbackSource;
use super::{RetryReason, Status};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "fallbacksrc",
        gst::DebugColorFlags::empty(),
        Some("Fallback Source Bin"),
    )
});

#[derive(Debug, Clone)]
struct Stats {
    num_retry: u64,
    last_retry_reason: RetryReason,
    buffering_percent: i32,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            num_retry: 0,
            last_retry_reason: RetryReason::None,
            buffering_percent: 100,
        }
    }
}

impl Stats {
    fn to_structure(&self) -> gst::Structure {
        gst::Structure::builder("application/x-fallbacksrc-stats")
            .field("num-retry", &self.num_retry)
            .field("last-retry-reason", &self.last_retry_reason)
            .field("buffering-percent", &self.buffering_percent)
            .build()
    }
}

#[derive(Debug, Clone)]
struct Settings {
    enable_audio: bool,
    enable_video: bool,
    uri: Option<String>,
    source: Option<gst::Element>,
    fallback_uri: Option<String>,
    timeout: gst::ClockTime,
    restart_timeout: gst::ClockTime,
    retry_timeout: gst::ClockTime,
    restart_on_eos: bool,
    min_latency: gst::ClockTime,
    buffer_duration: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enable_audio: true,
            enable_video: true,
            uri: None,
            source: None,
            fallback_uri: None,
            timeout: 5 * gst::ClockTime::SECOND,
            restart_timeout: 5 * gst::ClockTime::SECOND,
            retry_timeout: 60 * gst::ClockTime::SECOND,
            restart_on_eos: false,
            min_latency: gst::ClockTime::ZERO,
            buffer_duration: -1,
        }
    }
}

#[derive(Debug)]
enum Source {
    Uri(String),
    Element(gst::Element),
}

// Blocking buffer pad probe on the source pads. Once blocked we have a running time for the
// current buffer that can later be used for offsetting
//
// This is used for the initial offsetting after starting of the stream and for "pausing" when
// buffering.
struct Block {
    pad: gst::Pad,
    probe_id: gst::PadProbeId,
    running_time: Option<gst::ClockTime>,
}

// Connects one source pad with fallbackswitch and the corresponding fallback input
struct Stream {
    // Fallback input stream
    //   for video: filesrc, decoder, converters, imagefreeze
    //   for audio: live audiotestsrc, converters
    fallback_input: gst::Element,

    // source pad from source
    source_srcpad: Option<gst::Pad>,
    source_srcpad_block: Option<Block>,

    // clocksync for source source pad
    clocksync: gst::Element,

    clocksync_queue: gst::Element,
    clocksync_queue_srcpad: gst::Pad,

    // fallbackswitch
    switch: gst::Element,

    // output source pad, connected to switch
    srcpad: gst::GhostPad,
}

struct State {
    // uridecodebin3 or custom source element
    source: gst::Element,
    source_is_live: bool,
    source_pending_restart: bool,

    // For timing out the source and shutting it down to restart it
    source_restart_timeout: Option<gst::SingleShotClockId>,
    // For restarting the source after shutting it down
    source_pending_restart_timeout: Option<gst::SingleShotClockId>,
    // For failing completely if we didn't recover after the retry timeout
    source_retry_timeout: Option<gst::SingleShotClockId>,

    // All our output streams, selected by properties
    video_stream: Option<Stream>,
    audio_stream: Option<Stream>,
    flow_combiner: gst_base::UniqueFlowCombiner,

    last_buffering_update: Option<Instant>,

    // Stream collection posted by source
    streams: Option<gst::StreamCollection>,

    // Configure settings
    settings: Settings,
    configured_source: Source,

    // Statistics
    stats: Stats,
}

#[derive(Default)]
pub struct FallbackSrc {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

#[glib::object_subclass]
impl ObjectSubclass for FallbackSrc {
    const NAME: &'static str = "FallbackSrc";
    type Type = super::FallbackSrc;
    type ParentType = gst::Bin;
}

impl ObjectImpl for FallbackSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_boolean(
                    "enable-audio",
                    "Enable Audio",
                    "Enable the audio stream, this will output silence if there's no audio in the configured URI",
                    true,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_boolean(
                    "enable-video",
                    "Enable Video",
                    "Enable the video stream, this will output black or the fallback video if there's no video in the configured URI",
                    true,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_string("uri", "URI", "URI to use", None, glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY),
                glib::ParamSpec::new_object(
                    "source",
                    "Source",
                    "Source to use instead of the URI",
                    gst::Element::static_type(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_string(
                    "fallback-uri",
                    "Fallback URI",
                    "Fallback URI to use for video in case the main stream doesn't work",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_uint64(
                    "timeout",
                    "Timeout",
                    "Timeout for switching to the fallback URI",
                    0,
                    std::u64::MAX - 1,
                    5 * *gst::ClockTime::SECOND,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_uint64(
                    "restart-timeout",
                    "Timeout",
                    "Timeout for restarting an active source",
                    0,
                    std::u64::MAX - 1,
                    5 * *gst::ClockTime::SECOND,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_uint64(
                    "retry-timeout",
                    "Retry Timeout",
                    "Timeout for stopping after repeated failure",
                    0,
                    std::u64::MAX - 1,
                    60 * *gst::ClockTime::SECOND,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_boolean(
                    "restart-on-eos",
                    "Restart on EOS",
                    "Restart source on EOS",
                    false,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_enum(
                    "status",
                    "Status",
                    "Current source status",
                    Status::static_type(),
                    Status::Stopped as i32,
                    glib::ParamFlags::READABLE,
                ),
                glib::ParamSpec::new_uint64(
                    "min-latency",
                    "Minimum Latency",
                    "When the main source has a higher latency than the fallback source \
                     this allows to configure a minimum latency that would be configured \
                     if initially the fallback is enabled",
                    0,
                    std::u64::MAX - 1,
                    0,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_int64(
                    "buffer-duration",
                    "Buffer Duration",
                    "Buffer duration when buffering streams (-1 default value)",
                    -1,
                    std::i64::MAX - 1,
                    -1,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_boxed(
                    "statistics",
                    "Statistics",
                    "Various statistics",
                    gst::Structure::static_type(),
                    glib::ParamFlags::READABLE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "enable-audio" => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing enable-audio from {:?} to {:?}",
                    settings.enable_audio,
                    new_value,
                );
                settings.enable_audio = new_value;
            }
            "enable-video" => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing enable-video from {:?} to {:?}",
                    settings.enable_video,
                    new_value,
                );
                settings.enable_video = new_value;
            }
            "uri" => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing URI from {:?} to {:?}",
                    settings.uri,
                    new_value,
                );
                settings.uri = new_value;
            }
            "source" => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing source from {:?} to {:?}",
                    settings.source,
                    new_value,
                );
                settings.source = new_value;
            }
            "fallback-uri" => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing Fallback URI from {:?} to {:?}",
                    settings.fallback_uri,
                    new_value,
                );
                settings.fallback_uri = new_value;
            }
            "timeout" => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing timeout from {:?} to {:?}",
                    settings.timeout,
                    new_value,
                );
                settings.timeout = new_value;
            }
            "restart-timeout" => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing Restart Timeout from {:?} to {:?}",
                    settings.restart_timeout,
                    new_value,
                );
                settings.restart_timeout = new_value;
            }
            "retry-timeout" => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing Retry Timeout from {:?} to {:?}",
                    settings.retry_timeout,
                    new_value,
                );
                settings.retry_timeout = new_value;
            }
            "restart-on-eos" => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing restart-on-eos from {:?} to {:?}",
                    settings.restart_on_eos,
                    new_value,
                );
                settings.restart_on_eos = new_value;
            }
            "min-latency" => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing Minimum Latency from {:?} to {:?}",
                    settings.min_latency,
                    new_value,
                );
                settings.min_latency = new_value;
            }
            "buffer-duration" => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing Buffer Duration from {:?} to {:?}",
                    settings.buffer_duration,
                    new_value,
                );
                settings.buffer_duration = new_value;
            }
            _ => unimplemented!(),
        }
    }

    // Called whenever a value of a property is read. It can be called
    // at any time from any thread.
    #[allow(clippy::blocks_in_if_conditions)]
    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "enable-audio" => {
                let settings = self.settings.lock().unwrap();
                settings.enable_audio.to_value()
            }
            "enable-video" => {
                let settings = self.settings.lock().unwrap();
                settings.enable_video.to_value()
            }
            "uri" => {
                let settings = self.settings.lock().unwrap();
                settings.uri.to_value()
            }
            "source" => {
                let settings = self.settings.lock().unwrap();
                settings.source.to_value()
            }
            "fallback-uri" => {
                let settings = self.settings.lock().unwrap();
                settings.fallback_uri.to_value()
            }
            "timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.timeout.to_value()
            }
            "restart-timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.restart_timeout.to_value()
            }
            "retry-timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.retry_timeout.to_value()
            }
            "restart-on-eos" => {
                let settings = self.settings.lock().unwrap();
                settings.restart_on_eos.to_value()
            }
            "status" => {
                let state_guard = self.state.lock().unwrap();

                // If we have no state then we'r stopped
                let state = match &*state_guard {
                    None => return Status::Stopped.to_value(),
                    Some(ref state) => state,
                };

                // If any restarts/retries are pending, we're retrying
                if state.source_pending_restart
                    || state.source_pending_restart_timeout.is_some()
                    || state.source_retry_timeout.is_some()
                {
                    return Status::Retrying.to_value();
                }

                // Otherwise if buffering < 100, we have no streams yet or of the expected
                // streams there is no source pad yet, we're buffering
                let mut have_audio = false;
                let mut have_video = false;
                if let Some(ref streams) = state.streams {
                    for stream in streams.iter() {
                        have_audio =
                            have_audio || stream.stream_type().contains(gst::StreamType::AUDIO);
                        have_video =
                            have_video || stream.stream_type().contains(gst::StreamType::VIDEO);
                    }
                }

                if state.stats.buffering_percent < 100
                    || state.source_restart_timeout.is_some()
                    || state.streams.is_none()
                    || (have_audio
                        && state
                            .audio_stream
                            .as_ref()
                            .map(|s| s.source_srcpad.is_none() || s.source_srcpad_block.is_some())
                            .unwrap_or(true))
                    || (have_video
                        && state
                            .video_stream
                            .as_ref()
                            .map(|s| s.source_srcpad.is_none() || s.source_srcpad_block.is_some())
                            .unwrap_or(true))
                {
                    return Status::Buffering.to_value();
                }

                // Otherwise we're running now
                Status::Running.to_value()
            }
            "min-latency" => {
                let settings = self.settings.lock().unwrap();
                settings.min_latency.to_value()
            }
            "buffer-duration" => {
                let settings = self.settings.lock().unwrap();
                settings.buffer_duration.to_value()
            }
            "statistics" => self.stats().to_value(),
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![glib::subclass::Signal::builder(
                "update-uri",
                &[String::static_type().into()],
                String::static_type().into(),
            )
            .action()
            .class_handler(|_token, args| {
                // Simply return the input by default
                Some(args[1].clone())
            })
            .accumulator(|_hint, ret, value| {
                // First signal handler wins
                *ret = value.clone();
                false
            })
            .build()]
        });

        SIGNALS.as_ref()
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.set_suppressed_flags(gst::ElementFlags::SOURCE | gst::ElementFlags::SINK);
        obj.set_element_flags(gst::ElementFlags::SOURCE);
        obj.set_bin_flags(gst::BinFlags::STREAMS_AWARE);
    }
}

impl ElementImpl for FallbackSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Fallback Source",
                "Generic/Source",
                "Live source with uridecodebin3 or custom source, and fallback image stream",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let audio_src_pad_template = gst::PadTemplate::new(
                "audio",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &gst::Caps::new_any(),
            )
            .unwrap();

            let video_src_pad_template = gst::PadTemplate::new(
                "video",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![audio_src_pad_template, video_src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                self.start(element)?;
            }
            _ => (),
        }

        self.parent_change_state(element, transition)?;

        // Change the source state manually here to be able to catch errors. State changes always
        // happen from sink to source, so we do this after chaining up.
        self.change_source_state(element, transition);

        // Ignore parent state change return to prevent spurious async/no-preroll return values
        // due to core state change bugs
        match transition {
            gst::StateChange::ReadyToPaused | gst::StateChange::PlayingToPaused => {
                Ok(gst::StateChangeSuccess::NoPreroll)
            }
            gst::StateChange::ReadyToNull => {
                self.stop(element);
                Ok(gst::StateChangeSuccess::Success)
            }
            _ => Ok(gst::StateChangeSuccess::Success),
        }
    }
}

impl BinImpl for FallbackSrc {
    fn handle_message(&self, bin: &Self::Type, msg: gst::Message) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Buffering(ref m) => {
                // Don't forward upwards, we handle this internally
                self.handle_buffering(bin, m);
            }
            MessageView::StreamsSelected(ref m) => {
                // Don't forward upwards, we are exposing streams based on properties
                // TODO: Do stream configuration via our own stream collection and handling
                // of stream select events
                // TODO: Also needs updating of StreamCollection handling in CustomSource
                self.handle_streams_selected(bin, m);
            }
            MessageView::Error(ref m) => {
                if !self.handle_error(bin, m) {
                    self.parent_handle_message(bin, msg);
                }
            }
            _ => self.parent_handle_message(bin, msg),
        }
    }
}

impl FallbackSrc {
    fn create_main_input(
        &self,
        element: &super::FallbackSrc,
        source: &Source,
        buffer_duration: i64,
    ) -> gst::Element {
        let source = match source {
            Source::Uri(ref uri) => {
                let source = gst::ElementFactory::make("uridecodebin3", Some("uridecodebin"))
                    .expect("No uridecodebin3 found");

                let uri = element
                    .emit_by_name("update-uri", &[uri])
                    .expect("Failed to emit update-uri signal")
                    .expect("No value returned");
                let uri = uri
                    .get::<&str>()
                    .expect("Wrong type returned from update-uri signal");

                source.set_property("uri", &uri).unwrap();
                source.set_property("use-buffering", &true).unwrap();
                source
                    .set_property("buffer-duration", &buffer_duration)
                    .unwrap();

                source
            }
            Source::Element(ref source) => CustomSource::new(source).upcast(),
        };

        // Handle any async state changes internally, they don't affect the pipeline because we
        // convert everything to a live stream
        source.set_property("async-handling", &true).unwrap();
        // Don't let the bin handle state changes of the source. We want to do it manually to catch
        // possible errors and retry, without causing the whole bin state change to fail
        source.set_locked_state(true);

        let element_weak = element.downgrade();
        source.connect_pad_added(move |_, pad| {
            let element = match element_weak.upgrade() {
                None => return,
                Some(element) => element,
            };
            let src = FallbackSrc::from_instance(&element);

            if let Err(msg) = src.handle_source_pad_added(&element, pad) {
                element.post_error_message(msg);
            }
        });
        let element_weak = element.downgrade();
        source.connect_pad_removed(move |_, pad| {
            let element = match element_weak.upgrade() {
                None => return,
                Some(element) => element,
            };
            let src = FallbackSrc::from_instance(&element);

            src.handle_source_pad_removed(&element, pad);
        });

        element.add_many(&[&source]).unwrap();

        source
    }

    fn create_fallback_video_input(
        &self,
        _element: &super::FallbackSrc,
        min_latency: gst::ClockTime,
        fallback_uri: Option<&str>,
    ) -> gst::Element {
        VideoFallbackSource::new(fallback_uri, min_latency).upcast()
    }

    fn create_fallback_audio_input(&self, _element: &super::FallbackSrc) -> gst::Element {
        let input = gst::Bin::new(Some("fallback_audio"));
        let audiotestsrc = gst::ElementFactory::make("audiotestsrc", Some("fallback_audiosrc"))
            .expect("No audiotestsrc found");
        input.add_many(&[&audiotestsrc]).unwrap();

        audiotestsrc.set_property_from_str("wave", "silence");
        audiotestsrc.set_property("is-live", &true).unwrap();

        let srcpad = audiotestsrc.static_pad("src").unwrap();
        input
            .add_pad(
                &gst::GhostPad::builder(Some("src"), gst::PadDirection::Src)
                    .build_with_target(&srcpad)
                    .unwrap(),
            )
            .unwrap();

        input.upcast()
    }

    fn create_stream(
        &self,
        element: &super::FallbackSrc,
        timeout: gst::ClockTime,
        min_latency: gst::ClockTime,
        is_audio: bool,
        fallback_uri: Option<&str>,
    ) -> Stream {
        let fallback_input = if is_audio {
            self.create_fallback_audio_input(element)
        } else {
            self.create_fallback_video_input(element, min_latency, fallback_uri)
        };

        let switch =
            gst::ElementFactory::make("fallbackswitch", None).expect("No fallbackswitch found");
        let clocksync = gst::ElementFactory::make("clocksync", None)
            .or_else(|_| -> Result<_, glib::BoolError> {
                let identity = gst::ElementFactory::make("identity", None)?;
                identity.set_property("sync", &true).unwrap();
                Ok(identity)
            })
            .expect("No clocksync or identity found");

        // Workaround for issues caused by https://gitlab.freedesktop.org/gstreamer/gst-plugins-base/-/issues/800
        let clocksync_queue = gst::ElementFactory::make("queue", None).expect("No queue found");
        clocksync_queue
            .set_properties(&[
                ("max-size-buffers", &0u32),
                ("max-size-bytes", &0u32),
                ("max-size-time", &gst::ClockTime::SECOND),
            ])
            .unwrap();

        element
            .add_many(&[&fallback_input, &switch, &clocksync_queue, &clocksync])
            .unwrap();

        let element_weak = element.downgrade();
        switch.connect_notify(Some("active-pad"), move |_switch, _pspec| {
            let element = match element_weak.upgrade() {
                None => return,
                Some(element) => element,
            };

            let src = FallbackSrc::from_instance(&element);
            src.handle_switch_active_pad_change(&element);
        });
        switch.set_property("timeout", &timeout.nseconds()).unwrap();
        switch
            .set_property("min-upstream-latency", &min_latency.nseconds())
            .unwrap();

        gst::Element::link_pads(&fallback_input, Some("src"), &switch, Some("fallback_sink"))
            .unwrap();
        gst::Element::link_pads(&clocksync_queue, Some("src"), &clocksync, Some("sink")).unwrap();
        gst::Element::link_pads(&clocksync, Some("src"), &switch, Some("sink")).unwrap();
        // clocksync_queue sink pad is not connected to anything yet at this point!

        let srcpad = switch.static_pad("src").unwrap();
        let templ = element
            .pad_template(if is_audio { "audio" } else { "video" })
            .unwrap();
        let ghostpad = gst::GhostPad::builder_with_template(&templ, Some(&templ.name()))
            .proxy_pad_chain_function({
                let element_weak = element.downgrade();
                move |pad, _parent, buffer| {
                    let element = match element_weak.upgrade() {
                        None => return Err(gst::FlowError::Flushing),
                        Some(element) => element,
                    };

                    let src = FallbackSrc::from_instance(&element);
                    src.proxy_pad_chain(&element, pad, buffer)
                }
            })
            .build_with_target(&srcpad)
            .unwrap();

        element.add_pad(&ghostpad).unwrap();

        Stream {
            fallback_input,
            source_srcpad: None,
            source_srcpad_block: None,
            clocksync,
            clocksync_queue_srcpad: clocksync_queue.static_pad("src").unwrap(),
            clocksync_queue,
            switch,
            srcpad: ghostpad.upcast(),
        }
    }

    fn start(&self, element: &super::FallbackSrc) -> Result<(), gst::StateChangeError> {
        gst_debug!(CAT, obj: element, "Starting");
        let mut state_guard = self.state.lock().unwrap();
        if state_guard.is_some() {
            return Err(gst::StateChangeError);
        }

        let settings = self.settings.lock().unwrap().clone();
        let configured_source = match settings
            .uri
            .as_ref()
            .cloned()
            .map(Source::Uri)
            .or_else(|| settings.source.as_ref().cloned().map(Source::Element))
        {
            Some(source) => source,
            None => {
                gst_error!(CAT, obj: element, "No URI or source element configured");
                gst::element_error!(
                    element,
                    gst::LibraryError::Settings,
                    ["No URI or source element configured"]
                );
                return Err(gst::StateChangeError);
            }
        };

        let fallback_uri = &settings.fallback_uri;

        // Create main input
        let source = self.create_main_input(element, &configured_source, settings.buffer_duration);

        let mut flow_combiner = gst_base::UniqueFlowCombiner::new();

        // Create video stream
        let video_stream = if settings.enable_video {
            let stream = self.create_stream(
                element,
                settings.timeout,
                settings.min_latency,
                false,
                fallback_uri.as_deref(),
            );
            flow_combiner.add_pad(&stream.srcpad);
            Some(stream)
        } else {
            None
        };

        // Create audio stream
        let audio_stream = if settings.enable_audio {
            let stream =
                self.create_stream(element, settings.timeout, settings.min_latency, true, None);
            flow_combiner.add_pad(&stream.srcpad);
            Some(stream)
        } else {
            None
        };

        *state_guard = Some(State {
            source,
            source_is_live: false,
            source_pending_restart: false,
            source_restart_timeout: None,
            source_pending_restart_timeout: None,
            source_retry_timeout: None,
            video_stream,
            audio_stream,
            flow_combiner,
            last_buffering_update: None,
            streams: None,
            settings,
            configured_source,
            stats: Stats::default(),
        });

        drop(state_guard);

        element.no_more_pads();

        element.notify("status");

        gst_debug!(CAT, obj: element, "Started");
        Ok(())
    }

    fn stop(&self, element: &super::FallbackSrc) {
        gst_debug!(CAT, obj: element, "Stopping");
        let mut state_guard = self.state.lock().unwrap();
        let mut state = match state_guard.take() {
            Some(state) => state,
            None => return,
        };
        drop(state_guard);

        element.notify("status");

        // In theory all streams should've been removed from the source's pad-removed signal
        // handler when going from Paused to Ready but better safe than sorry here
        for stream in [&state.video_stream, &state.audio_stream]
            .iter()
            .filter_map(|v| v.as_ref())
        {
            element.remove(&stream.switch).unwrap();
            element.remove(&stream.clocksync_queue).unwrap();
            element.remove(&stream.clocksync).unwrap();
            element.remove(&stream.fallback_input).unwrap();
            let _ = stream.srcpad.set_target(None::<&gst::Pad>);
            let _ = element.remove_pad(&stream.srcpad);
        }
        state.video_stream = None;
        state.audio_stream = None;

        if let Source::Element(ref source) = state.configured_source {
            // Explicitly remove the source element from the CustomSource so that we can
            // later create a new CustomSource and add it again there.
            if source.has_as_parent(&state.source) {
                let _ = source.set_state(gst::State::Null);
                let _ = state
                    .source
                    .downcast_ref::<gst::Bin>()
                    .unwrap()
                    .remove(source);
            }
        }
        element.remove(&state.source).unwrap();

        if let Some(timeout) = state.source_pending_restart_timeout.take() {
            timeout.unschedule();
        }

        if let Some(timeout) = state.source_retry_timeout.take() {
            timeout.unschedule();
        }

        if let Some(timeout) = state.source_restart_timeout.take() {
            timeout.unschedule();
        }

        gst_debug!(CAT, obj: element, "Stopped");
    }

    fn change_source_state(&self, element: &super::FallbackSrc, transition: gst::StateChange) {
        gst_debug!(CAT, obj: element, "Changing source state: {:?}", transition);
        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            Some(state) => state,
            None => return,
        };

        if transition.current() <= transition.next() && state.source_pending_restart {
            gst_debug!(
                CAT,
                obj: element,
                "Not starting source because pending restart"
            );
            return;
        } else if transition.next() <= gst::State::Ready && state.source_pending_restart {
            gst_debug!(
                CAT,
                obj: element,
                "Unsetting pending restart because shutting down"
            );
            state.source_pending_restart = false;
            if let Some(timeout) = state.source_pending_restart_timeout.take() {
                timeout.unschedule();
            }
        }
        let source = state.source.clone();
        drop(state_guard);

        element.notify("status");

        let res = source.set_state(transition.next());
        match res {
            Err(_) => {
                gst_error!(CAT, obj: element, "Source failed to change state");
                // Try again later if we're not shutting down
                if transition != gst::StateChange::ReadyToNull {
                    let _ = source.set_state(gst::State::Null);
                    let mut state_guard = self.state.lock().unwrap();
                    let state = state_guard.as_mut().expect("no state");
                    self.handle_source_error(element, state, RetryReason::StateChangeFailure);
                    drop(state_guard);
                    element.notify("statistics");
                }
            }
            Ok(res) => {
                gst_debug!(
                    CAT,
                    obj: element,
                    "Source changed state successfully: {:?}",
                    res
                );

                let mut state_guard = self.state.lock().unwrap();
                let state = state_guard.as_mut().expect("no state");

                // Remember if the source is live
                if transition == gst::StateChange::ReadyToPaused {
                    state.source_is_live = res == gst::StateChangeSuccess::NoPreroll;
                }

                if (state.source_is_live && transition == gst::StateChange::ReadyToPaused)
                    || (!state.source_is_live && transition == gst::StateChange::PausedToPlaying)
                {
                    assert!(state.source_restart_timeout.is_none());
                    self.schedule_source_restart_timeout(element, state, gst::ClockTime::ZERO);
                }
            }
        }
    }

    fn proxy_pad_chain(
        &self,
        element: &super::FallbackSrc,
        pad: &gst::ProxyPad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let res = gst::ProxyPad::chain_default(pad, Some(element), buffer);

        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => return res,
            Some(state) => state,
        };

        state.flow_combiner.update_pad_flow(pad, res)
    }

    fn handle_source_pad_added(
        &self,
        element: &super::FallbackSrc,
        pad: &gst::Pad,
    ) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Pad {} added to source", pad.name(),);

        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return Ok(());
            }
            Some(state) => state,
        };

        let (type_, stream) = match pad.name() {
            x if x.starts_with("audio_") => ("audio", &mut state.audio_stream),
            x if x.starts_with("video_") => ("video", &mut state.video_stream),
            _ => {
                let caps = match pad.current_caps().unwrap_or_else(|| pad.query_caps(None)) {
                    caps if !caps.is_any() && !caps.is_empty() => caps,
                    _ => return Ok(()),
                };

                let s = caps.structure(0).unwrap();

                if s.name().starts_with("audio/") {
                    ("audio", &mut state.audio_stream)
                } else if s.name().starts_with("video/") {
                    ("video", &mut state.video_stream)
                } else {
                    // TODO: handle subtitles etc
                    return Ok(());
                }
            }
        };

        let stream = match stream {
            None => {
                gst_debug!(CAT, obj: element, "No {} stream enabled", type_);
                return Ok(());
            }
            Some(Stream {
                source_srcpad: Some(_),
                ..
            }) => {
                gst_debug!(CAT, obj: element, "Already configured a {} stream", type_);
                return Ok(());
            }
            Some(ref mut stream) => stream,
        };

        let sinkpad = stream.clocksync_queue.static_pad("sink").unwrap();
        pad.link(&sinkpad).map_err(|err| {
            gst_error!(
                CAT,
                obj: element,
                "Failed to link source pad to clocksync: {}",
                err
            );
            gst::error_msg!(
                gst::CoreError::Negotiation,
                ["Failed to link source pad to clocksync: {}", err]
            )
        })?;

        if state.settings.restart_on_eos {
            let element_weak = element.downgrade();
            pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
                let element = match element_weak.upgrade() {
                    None => return gst::PadProbeReturn::Ok,
                    Some(element) => element,
                };

                let src = FallbackSrc::from_instance(&element);

                match info.data {
                    Some(gst::PadProbeData::Event(ref ev)) if ev.type_() == gst::EventType::Eos => {
                        gst_debug!(
                            CAT,
                            obj: &element,
                            "Received EOS from source on pad {}, restarting",
                            pad.name()
                        );

                        let mut state_guard = src.state.lock().unwrap();
                        let state = match &mut *state_guard {
                            None => {
                                return gst::PadProbeReturn::Ok;
                            }
                            Some(state) => state,
                        };
                        src.handle_source_error(&element, state, RetryReason::Eos);
                        drop(state_guard);
                        element.notify("statistics");

                        gst::PadProbeReturn::Drop
                    }
                    _ => gst::PadProbeReturn::Ok,
                }
            });
        }

        assert!(stream.source_srcpad_block.is_none());
        stream.source_srcpad = Some(pad.clone());
        stream.source_srcpad_block = Some(self.add_pad_probe(element, stream));

        drop(state_guard);
        element.notify("status");

        Ok(())
    }

    fn add_pad_probe(&self, element: &super::FallbackSrc, stream: &mut Stream) -> Block {
        // FIXME: Not literally correct as we add the probe to the queue source pad but that's only
        // a workaround until
        //     https://gitlab.freedesktop.org/gstreamer/gst-plugins-base/-/issues/800
        // is fixed.
        gst_debug!(
            CAT,
            obj: element,
            "Adding probe to pad {}",
            stream.source_srcpad.as_ref().unwrap().name()
        );

        let element_weak = element.downgrade();
        let probe_id = stream
            .clocksync_queue_srcpad
            .add_probe(
                gst::PadProbeType::BLOCK
                    | gst::PadProbeType::BUFFER
                    | gst::PadProbeType::EVENT_DOWNSTREAM,
                move |pad, info| {
                    let element = match element_weak.upgrade() {
                        None => return gst::PadProbeReturn::Pass,
                        Some(element) => element,
                    };
                    let pts = match info.data {
                        Some(gst::PadProbeData::Buffer(ref buffer)) => buffer.pts(),
                        Some(gst::PadProbeData::Event(ref ev)) => match ev.view() {
                            gst::EventView::Gap(ref ev) => Some(ev.get().0),
                            _ => return gst::PadProbeReturn::Pass,
                        },
                        _ => unreachable!(),
                    };

                    let src = FallbackSrc::from_instance(&element);

                    if let Err(msg) = src.handle_pad_blocked(&element, pad, pts) {
                        element.post_error_message(msg);
                    }

                    gst::PadProbeReturn::Ok
                },
            )
            .unwrap();

        Block {
            pad: stream.clocksync_queue_srcpad.clone(),
            probe_id,
            running_time: gst::ClockTime::NONE,
        }
    }

    fn handle_pad_blocked(
        &self,
        element: &super::FallbackSrc,
        pad: &gst::Pad,
        pts: impl Into<Option<gst::ClockTime>>,
    ) -> Result<(), gst::ErrorMessage> {
        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return Ok(());
            }
            Some(state) => state,
        };

        // FIXME: Not literally correct as we added the probe to the queue source pad but that's only
        // a workaround until
        //     https://gitlab.freedesktop.org/gstreamer/gst-plugins-base/-/issues/800
        // is fixed.

        let stream = if let Some(stream) = state
            .audio_stream
            .as_mut()
            .filter(|s| &s.clocksync_queue_srcpad == pad)
        {
            gst_debug!(
                CAT,
                obj: element,
                "Called probe on pad {}",
                stream.source_srcpad.as_ref().unwrap().name()
            );
            stream
        } else if let Some(stream) = state
            .video_stream
            .as_mut()
            .filter(|s| &s.clocksync_queue_srcpad == pad)
        {
            gst_debug!(
                CAT,
                obj: element,
                "Called probe on pad {}",
                stream.source_srcpad.as_ref().unwrap().name()
            );
            stream
        } else {
            unreachable!();
        };

        // Directly unblock for live streams
        if state.source_is_live {
            for (source_srcpad, block) in [state.video_stream.as_mut(), state.audio_stream.as_mut()]
                .iter_mut()
                .filter_map(|s| s.as_mut())
                .filter_map(|s| {
                    if let Some(block) = s.source_srcpad_block.take() {
                        Some((s.source_srcpad.as_ref().unwrap(), block))
                    } else {
                        None
                    }
                })
            {
                gst_debug!(
                    CAT,
                    obj: element,
                    "Removing pad probe for pad {}",
                    source_srcpad.name()
                );
                block.pad.remove_probe(block.probe_id);
            }

            gst_debug!(CAT, obj: element, "Live source, unblocking directly");

            drop(state_guard);
            element.notify("status");

            return Ok(());
        }

        // Update running time for this block
        let block = match stream.source_srcpad_block {
            Some(ref mut block) => block,
            None => return Ok(()),
        };

        let ev = match pad.sticky_event(gst::EventType::Segment, 0) {
            Some(ev) => ev,
            None => {
                gst_warning!(CAT, obj: element, "Have no segment event yet");
                return Ok(());
            }
        };

        let segment = match ev.view() {
            gst::EventView::Segment(s) => s.segment(),
            _ => unreachable!(),
        };
        let segment = segment.downcast_ref::<gst::ClockTime>().ok_or_else(|| {
            gst_error!(CAT, obj: element, "Have no time segment");
            gst::error_msg!(gst::CoreError::Clock, ["Have no time segment"])
        })?;

        let pts = pts.into();
        let running_time = if let Some((_, start)) =
            pts.zip(segment.start()).filter(|(pts, start)| pts < start)
        {
            Some(start)
        } else if let Some((_, stop)) = pts.zip(segment.stop()).filter(|(pts, stop)| pts >= stop) {
            Some(stop)
        } else {
            segment.to_running_time(pts)
        };

        gst_debug!(
            CAT,
            obj: element,
            "Have block running time {}",
            running_time.display(),
        );

        block.running_time = running_time;

        self.unblock_pads(element, state);

        drop(state_guard);
        element.notify("status");

        Ok(())
    }

    fn unblock_pads(&self, element: &super::FallbackSrc, state: &mut State) {
        // Check if all streams are blocked and have a running time and we have
        // 100% buffering
        if state.stats.buffering_percent < 100 {
            gst_debug!(
                CAT,
                obj: element,
                "Not unblocking yet: buffering {}%",
                state.stats.buffering_percent
            );
            return;
        }

        let streams = match state.streams {
            None => {
                gst_debug!(CAT, obj: element, "Have no stream collection yet");
                return;
            }
            Some(ref streams) => streams,
        };
        let mut have_audio = false;
        let mut have_video = false;
        for stream in streams.iter() {
            have_audio = have_audio || stream.stream_type().contains(gst::StreamType::AUDIO);
            have_video = have_video || stream.stream_type().contains(gst::StreamType::VIDEO);
        }

        let want_audio = state.settings.enable_audio;
        let want_video = state.settings.enable_video;

        let audio_running_time = state
            .audio_stream
            .as_ref()
            .and_then(|s| s.source_srcpad_block.as_ref())
            .and_then(|b| b.running_time);
        let video_running_time = state
            .video_stream
            .as_ref()
            .and_then(|s| s.source_srcpad_block.as_ref())
            .and_then(|b| b.running_time);

        let audio_srcpad = state
            .audio_stream
            .as_ref()
            .and_then(|s| s.source_srcpad.as_ref().cloned());
        let video_srcpad = state
            .video_stream
            .as_ref()
            .and_then(|s| s.source_srcpad.as_ref().cloned());

        let audio_is_eos = audio_srcpad
            .as_ref()
            .map(|p| p.pad_flags().contains(gst::PadFlags::EOS))
            .unwrap_or(false);
        let video_is_eos = video_srcpad
            .as_ref()
            .map(|p| p.pad_flags().contains(gst::PadFlags::EOS))
            .unwrap_or(false);

        // If we need both, wait for both and take the minimum, otherwise take the one we need.
        // Also consider EOS, we'd never get a new running time after EOS so don't need to wait.
        // FIXME: All this surely can be simplified somehow

        // FIXME I guess this could be moved up
        let current_running_time = match element.current_running_time() {
            Some(current_running_time) => current_running_time,
            None => {
                gst_debug!(CAT, obj: element, "Waiting for current_running_time");
                return;
            }
        };

        if have_audio && want_audio && have_video && want_video {
            if audio_running_time.is_none()
                && !audio_is_eos
                && video_running_time.is_none()
                && !video_is_eos
            {
                gst_debug!(
                    CAT,
                    obj: element,
                    "Waiting for audio and video pads to block"
                );
                return;
            } else if audio_running_time.is_none() && !audio_is_eos {
                gst_debug!(CAT, obj: element, "Waiting for audio pad to block");
                return;
            } else if video_running_time.is_none() && !video_is_eos {
                gst_debug!(CAT, obj: element, "Waiting for video pad to block");
                return;
            }

            let audio_running_time = audio_running_time.expect("checked above");
            let video_running_time = video_running_time.expect("checked above");

            let min_running_time = if audio_is_eos {
                video_running_time
            } else if video_is_eos {
                audio_running_time
            } else {
                audio_running_time.min(video_running_time)
            };

            let offset = if current_running_time > min_running_time {
                (current_running_time - min_running_time).nseconds() as i64
            } else {
                -((min_running_time - current_running_time).nseconds() as i64)
            };

            gst_debug!(
                CAT,
                obj: element,
                "Unblocking at {} with pad offset {} (audio: {} eos {}, video {} eos {})",
                current_running_time,
                offset,
                audio_running_time,
                audio_is_eos,
                video_running_time,
                video_is_eos,
            );

            if let Some(block) = state
                .audio_stream
                .as_mut()
                .and_then(|s| s.source_srcpad_block.take())
            {
                if !audio_is_eos {
                    block.pad.set_offset(offset);
                }
                block.pad.remove_probe(block.probe_id);
            }

            if let Some(block) = state
                .video_stream
                .as_mut()
                .and_then(|s| s.source_srcpad_block.take())
            {
                if !video_is_eos {
                    block.pad.set_offset(offset);
                }
                block.pad.remove_probe(block.probe_id);
            }
        } else if have_audio && want_audio {
            let audio_running_time = match audio_running_time {
                Some(audio_running_time) => audio_running_time,
                None => {
                    gst_debug!(CAT, obj: element, "Waiting for audio pad to block");
                    return;
                }
            };

            let offset = if current_running_time > audio_running_time {
                (current_running_time - audio_running_time).nseconds() as i64
            } else {
                -((audio_running_time - current_running_time).nseconds() as i64)
            };

            gst_debug!(
                CAT,
                obj: element,
                "Unblocking at {} with pad offset {} (audio: {} eos {})",
                current_running_time,
                offset,
                audio_running_time,
                audio_is_eos
            );

            if let Some(block) = state
                .audio_stream
                .as_mut()
                .and_then(|s| s.source_srcpad_block.take())
            {
                if !audio_is_eos {
                    block.pad.set_offset(offset);
                }
                block.pad.remove_probe(block.probe_id);
            }
        } else if have_video && want_video {
            let video_running_time = match video_running_time {
                Some(video_running_time) => video_running_time,
                None => {
                    gst_debug!(CAT, obj: element, "Waiting for video pad to block");
                    return;
                }
            };

            let offset = if current_running_time > video_running_time {
                (current_running_time - video_running_time).nseconds() as i64
            } else {
                -((video_running_time - current_running_time).nseconds() as i64)
            };

            gst_debug!(
                CAT,
                obj: element,
                "Unblocking at {} with pad offset {} (video: {} eos {})",
                current_running_time,
                offset,
                video_running_time,
                video_is_eos
            );

            if let Some(block) = state
                .video_stream
                .as_mut()
                .and_then(|s| s.source_srcpad_block.take())
            {
                if !video_is_eos {
                    block.pad.set_offset(offset);
                }
                block.pad.remove_probe(block.probe_id);
            }
        }
    }

    fn handle_source_pad_removed(&self, element: &super::FallbackSrc, pad: &gst::Pad) {
        gst_debug!(CAT, obj: element, "Pad {} removed from source", pad.name());

        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return;
            }
            Some(state) => state,
        };

        // Don't have to do anything here other than forgetting about the pad. Unlinking will
        // automatically happen while the pad is being removed from source and thus leaves the
        // bin hierarchy
        let stream = if let Some(stream) = state
            .audio_stream
            .as_mut()
            .filter(|s| s.source_srcpad.as_ref() == Some(pad))
        {
            stream
        } else if let Some(stream) = state
            .video_stream
            .as_mut()
            .filter(|s| s.source_srcpad.as_ref() == Some(pad))
        {
            stream
        } else {
            return;
        };

        stream.source_srcpad = None;

        self.unblock_pads(element, state);

        drop(state_guard);
        element.notify("status");
    }

    fn handle_buffering(&self, element: &super::FallbackSrc, m: &gst::message::Buffering) {
        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return;
            }
            Some(state) => state,
        };

        if state.source_pending_restart {
            gst_debug!(CAT, obj: element, "Has pending restart");
            return;
        }

        gst_debug!(CAT, obj: element, "Got buffering {}%", m.percent());

        state.stats.buffering_percent = m.percent();
        if state.stats.buffering_percent < 100 {
            state.last_buffering_update = Some(Instant::now());
            // Block source pads if needed to pause
            if let Some(ref mut stream) = state.audio_stream {
                if stream.source_srcpad_block.is_none() && stream.source_srcpad.is_some() {
                    stream.source_srcpad_block = Some(self.add_pad_probe(element, stream));
                }
            }
            if let Some(ref mut stream) = state.video_stream {
                if stream.source_srcpad_block.is_none() && stream.source_srcpad.is_some() {
                    stream.source_srcpad_block = Some(self.add_pad_probe(element, stream));
                }
            }
        } else {
            // Check if we can unblock now
            self.unblock_pads(element, state);
        }

        drop(state_guard);
        element.notify("status");
        element.notify("statistics");
    }

    fn handle_streams_selected(
        &self,
        element: &super::FallbackSrc,
        m: &gst::message::StreamsSelected,
    ) {
        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return;
            }
            Some(state) => state,
        };

        let streams = m.stream_collection();

        gst_debug!(
            CAT,
            obj: element,
            "Got stream collection {:?}",
            streams.debug()
        );

        let mut have_audio = false;
        let mut have_video = false;
        for stream in streams.iter() {
            have_audio = have_audio || stream.stream_type().contains(gst::StreamType::AUDIO);
            have_video = have_video || stream.stream_type().contains(gst::StreamType::VIDEO);
        }

        if !have_audio && state.settings.enable_audio {
            gst_warning!(
                CAT,
                obj: element,
                "Have no audio streams but audio is enabled"
            );
        }

        if !have_video && state.settings.enable_video {
            gst_warning!(
                CAT,
                obj: element,
                "Have no video streams but video is enabled"
            );
        }

        state.streams = Some(streams);

        // This might not be the first stream collection and we might have some unblocked pads from
        // before already, which would need to be blocked again now for keeping things in sync
        for stream in [&mut state.video_stream, &mut state.audio_stream]
            .iter_mut()
            .filter_map(|v| v.as_mut())
        {
            if stream.source_srcpad.is_some() && stream.source_srcpad_block.is_none() {
                stream.source_srcpad_block = Some(self.add_pad_probe(element, stream));
            }
        }

        self.unblock_pads(element, state);

        drop(state_guard);
        element.notify("status");
    }

    fn handle_error(&self, element: &super::FallbackSrc, m: &gst::message::Error) -> bool {
        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return false;
            }
            Some(state) => state,
        };

        let src = match m.src().and_then(|s| s.downcast::<gst::Element>().ok()) {
            None => return false,
            Some(src) => src,
        };

        gst_debug!(
            CAT,
            obj: element,
            "Got error message from {}",
            src.path_string()
        );

        if src == state.source || src.has_as_ancestor(&state.source) {
            self.handle_source_error(element, state, RetryReason::Error);
            drop(state_guard);
            element.notify("status");
            element.notify("statistics");
            return true;
        }

        // Check if error is from video fallback input and if so, try another
        // fallback to videotestsrc
        if let Some(ref mut video_stream) = state.video_stream {
            if src == video_stream.fallback_input
                || src.has_as_ancestor(&video_stream.fallback_input)
            {
                gst_debug!(CAT, obj: element, "Got error from video fallback input");

                let prev_fallback_uri = video_stream
                    .fallback_input
                    .property("uri")
                    .unwrap()
                    .get::<Option<String>>()
                    .unwrap();

                // This means previously videotestsrc was configured
                // Something went wrong and there is no other way than to error out
                if prev_fallback_uri.is_none() {
                    return false;
                }

                let fallback_input = &video_stream.fallback_input;
                fallback_input.call_async(|fallback_input| {
                    // Re-run video fallback input with videotestsrc
                    let _ = fallback_input.set_state(gst::State::Null);
                    let _ = fallback_input.set_property("uri", None::<&str>);
                    let _ = fallback_input.sync_state_with_parent();
                });

                return true;
            }
        }

        gst_error!(
            CAT,
            obj: element,
            "Give up for error message from {}",
            src.path_string()
        );

        false
    }

    fn handle_source_error(
        &self,
        element: &super::FallbackSrc,
        state: &mut State,
        reason: RetryReason,
    ) {
        gst_debug!(CAT, obj: element, "Handling source error");

        state.stats.last_retry_reason = reason;
        if state.source_pending_restart {
            gst_debug!(CAT, obj: element, "Source is already pending restart");
            return;
        }

        // Increase retry count only if there was no pending restart
        state.stats.num_retry += 1;

        // Unschedule pending timeout, we're restarting now
        if let Some(timeout) = state.source_restart_timeout.take() {
            timeout.unschedule();
        }

        // Prevent state changes from changing the state in an uncoordinated way
        state.source_pending_restart = true;

        // Drop any EOS events from any source pads of the source that might happen because of the
        // error. We don't need to remove these pad probes because restarting the source will also
        // remove/add the pads again.
        for pad in state.source.src_pads() {
            pad.add_probe(
                gst::PadProbeType::EVENT_DOWNSTREAM,
                |_pad, info| match info.data {
                    Some(gst::PadProbeData::Event(ref event)) => {
                        if event.type_() == gst::EventType::Eos {
                            gst::PadProbeReturn::Drop
                        } else {
                            gst::PadProbeReturn::Ok
                        }
                    }
                    _ => unreachable!(),
                },
            )
            .unwrap();
        }

        let source_weak = state.source.downgrade();
        element.call_async(move |element| {
            let src = FallbackSrc::from_instance(element);

            let source = match source_weak.upgrade() {
                None => return,
                Some(source) => source,
            };

            // Remove blocking pad probes if they are still there as otherwise shutting down the
            // source will deadlock on the probes.
            let mut state_guard = src.state.lock().unwrap();
            let state = match &mut *state_guard {
                None
                | Some(State {
                    source_pending_restart: false,
                    ..
                }) => {
                    gst_debug!(CAT, obj: element, "Restarting source not needed anymore");
                    return;
                }
                Some(state) => state,
            };
            for (source_srcpad, block) in [state.video_stream.as_mut(), state.audio_stream.as_mut()]
                .iter_mut()
                .filter_map(|s| s.as_mut())
                .filter_map(|s| {
                    if let Some(block) = s.source_srcpad_block.take() {
                        Some((s.source_srcpad.as_ref().unwrap(), block))
                    } else {
                        None
                    }
                })
            {
                gst_debug!(
                    CAT,
                    obj: element,
                    "Removing pad probe for pad {}",
                    source_srcpad.name()
                );
                block.pad.remove_probe(block.probe_id);
            }
            drop(state_guard);

            gst_debug!(CAT, obj: element, "Shutting down source");
            let _ = source.set_state(gst::State::Null);

            // Sleep for 1s before retrying

            let mut state_guard = src.state.lock().unwrap();
            let state = match &mut *state_guard {
                None
                | Some(State {
                    source_pending_restart: false,
                    ..
                }) => {
                    gst_debug!(CAT, obj: element, "Restarting source not needed anymore");
                    return;
                }
                Some(state) => state,
            };

            for stream in [state.video_stream.as_mut(), state.audio_stream.as_mut()]
                .iter_mut()
                .filter_map(|s| s.as_mut())
            {
                stream.source_srcpad_block = None;
                stream.source_srcpad = None;
            }

            gst_debug!(CAT, obj: element, "Waiting for 1s before retrying");
            let clock = gst::SystemClock::obtain();
            let wait_time = clock.time().unwrap() + gst::ClockTime::SECOND;
            assert!(state.source_pending_restart_timeout.is_none());

            let timeout = clock.new_single_shot_id(wait_time);
            let element_weak = element.downgrade();
            timeout
                .wait_async(move |_clock, _time, _id| {
                    let element = match element_weak.upgrade() {
                        None => return,
                        Some(element) => element,
                    };

                    gst_debug!(CAT, obj: &element, "Woke up, retrying");
                    element.call_async(|element| {
                        let src = FallbackSrc::from_instance(element);

                        let mut state_guard = src.state.lock().unwrap();
                        let state = match &mut *state_guard {
                            None
                            | Some(State {
                                source_pending_restart: false,
                                ..
                            }) => {
                                gst_debug!(
                                    CAT,
                                    obj: element,
                                    "Restarting source not needed anymore"
                                );
                                return;
                            }
                            Some(state) => state,
                        };

                        let (source, old_source) = if let Source::Uri(..) = state.configured_source
                        {
                            // FIXME: Create a new uridecodebin3 because it currently is not reusable
                            // See https://gitlab.freedesktop.org/gstreamer/gst-plugins-base/-/issues/746
                            element.remove(&state.source).unwrap();

                            let source = src.create_main_input(
                                element,
                                &state.configured_source,
                                state.settings.buffer_duration,
                            );

                            (
                                source.clone(),
                                Some(mem::replace(&mut state.source, source)),
                            )
                        } else {
                            (state.source.clone(), None)
                        };

                        state.source_pending_restart = false;
                        state.source_pending_restart_timeout = None;
                        state.stats.buffering_percent = 100;
                        state.last_buffering_update = None;

                        if let Some(timeout) = state.source_restart_timeout.take() {
                            gst_debug!(CAT, obj: element, "Unscheduling restart timeout");
                            timeout.unschedule();
                        }
                        drop(state_guard);

                        if let Some(old_source) = old_source {
                            // Drop old source after releasing the lock, it might call the pad-removed callback
                            // still
                            drop(old_source);
                        }

                        if source.sync_state_with_parent().is_err() {
                            gst_error!(CAT, obj: element, "Source failed to change state");
                            let _ = source.set_state(gst::State::Null);
                            let mut state_guard = src.state.lock().unwrap();
                            let state = state_guard.as_mut().expect("no state");
                            src.handle_source_error(
                                element,
                                state,
                                RetryReason::StateChangeFailure,
                            );
                            drop(state_guard);
                            element.notify("statistics");
                        } else {
                            let mut state_guard = src.state.lock().unwrap();
                            let state = state_guard.as_mut().expect("no state");
                            assert!(state.source_restart_timeout.is_none());
                            src.schedule_source_restart_timeout(
                                element,
                                state,
                                gst::ClockTime::ZERO,
                            );
                        }
                    });
                })
                .expect("Failed to wait async");
            state.source_pending_restart_timeout = Some(timeout);
        });
    }

    #[allow(clippy::blocks_in_if_conditions)]
    fn schedule_source_restart_timeout(
        &self,
        element: &super::FallbackSrc,
        state: &mut State,
        elapsed: gst::ClockTime,
    ) {
        if state.source_pending_restart {
            gst_debug!(
                CAT,
                obj: element,
                "Not scheduling source restart timeout because source is pending restart already",
            );
            return;
        }

        let clock = gst::SystemClock::obtain();
        let wait_time = clock.time().unwrap() + state.settings.restart_timeout - elapsed;
        gst_debug!(
            CAT,
            obj: element,
            "Scheduling source restart timeout for {}",
            wait_time,
        );

        let timeout = clock.new_single_shot_id(wait_time);
        let element_weak = element.downgrade();
        timeout
            .wait_async(move |_clock, _time, _id| {
                let element = match element_weak.upgrade() {
                    None => return,
                    Some(element) => element,
                };

                element.call_async(move |element| {
                    let src = FallbackSrc::from_instance(element);

                    gst_debug!(CAT, obj: element, "Source restart timeout triggered");
                    let mut state_guard = src.state.lock().unwrap();
                    let state = match &mut *state_guard {
                        None => {
                            gst_debug!(CAT, obj: element, "Restarting source not needed anymore");
                            return;
                        }
                        Some(state) => state,
                    };

                    state.source_restart_timeout = None;

                    // If we have the fallback activated then restart the source now.
                    if src.have_fallback_activated(element, state) {
                        // If we're not actively buffering right now let's restart the source
                        if state
                            .last_buffering_update
                            .map(|i| i.elapsed() >= state.settings.restart_timeout.into())
                            .unwrap_or(state.stats.buffering_percent == 100)
                        {
                            gst_debug!(CAT, obj: element, "Not buffering, restarting source");

                            src.handle_source_error(element, state, RetryReason::Timeout);
                            drop(state_guard);
                            element.notify("statistics");
                        } else {
                            gst_debug!(CAT, obj: element, "Buffering, restarting source later");
                            let elapsed = state
                                .last_buffering_update
                                .and_then(|last_buffering_update| {
                                    gst::ClockTime::try_from(last_buffering_update.elapsed()).ok()
                                })
                                .unwrap_or(gst::ClockTime::ZERO);

                            src.schedule_source_restart_timeout(element, state, elapsed);
                        }
                    } else {
                        gst_debug!(CAT, obj: element, "Restarting source not needed anymore");
                    }
                });
            })
            .expect("Failed to wait async");

        state.source_restart_timeout = Some(timeout);
    }

    #[allow(clippy::blocks_in_if_conditions)]
    fn have_fallback_activated(&self, _element: &super::FallbackSrc, state: &State) -> bool {
        let mut have_audio = false;
        let mut have_video = false;
        if let Some(ref streams) = state.streams {
            for stream in streams.iter() {
                have_audio = have_audio || stream.stream_type().contains(gst::StreamType::AUDIO);
                have_video = have_video || stream.stream_type().contains(gst::StreamType::VIDEO);
            }
        }

        // If we have neither audio nor video (no streams yet), or active pad for the ones we have
        // is the fallback pad then we have the fallback activated.
        (!have_audio && !have_video)
            || (have_audio
                && state.audio_stream.is_some()
                && state
                    .audio_stream
                    .as_ref()
                    .and_then(|s| {
                        s.switch
                            .property("active-pad")
                            .unwrap()
                            .get::<Option<gst::Pad>>()
                            .unwrap()
                    })
                    .map(|p| p.name() == "fallback_sink")
                    .unwrap_or(true))
            || (have_video
                && state.video_stream.is_some()
                && state
                    .video_stream
                    .as_ref()
                    .and_then(|s| {
                        s.switch
                            .property("active-pad")
                            .unwrap()
                            .get::<Option<gst::Pad>>()
                            .unwrap()
                    })
                    .map(|p| p.name() == "fallback_sink")
                    .unwrap_or(true))
    }

    fn handle_switch_active_pad_change(&self, element: &super::FallbackSrc) {
        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return;
            }
            Some(state) => state,
        };

        // If we have the fallback activated then start the retry timeout unless it was started
        // already. Otherwise cancel the retry timeout.
        if self.have_fallback_activated(element, state) {
            gst_warning!(CAT, obj: element, "Switched to fallback stream");
            if state.source_restart_timeout.is_none() {
                self.schedule_source_restart_timeout(element, state, gst::ClockTime::ZERO);
            }
        } else {
            gst_debug!(CAT, obj: element, "Switched to main stream");
            if let Some(timeout) = state.source_retry_timeout.take() {
                gst_debug!(CAT, obj: element, "Unscheduling retry timeout");
                timeout.unschedule();
            }

            if let Some(timeout) = state.source_restart_timeout.take() {
                gst_debug!(CAT, obj: element, "Unscheduling restart timeout");
                timeout.unschedule();
            }
        }

        drop(state_guard);
        element.notify("status");
    }

    fn stats(&self) -> gst::Structure {
        let state_guard = self.state.lock().unwrap();

        let state = match &*state_guard {
            None => return Stats::default().to_structure(),
            Some(ref state) => state,
        };

        state.stats.to_structure()
    }
}
