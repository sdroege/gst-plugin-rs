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

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::mem;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "fallbacksrc",
        gst::DebugColorFlags::empty(),
        Some("Fallback Source Bin"),
    )
});

#[derive(Debug, Clone)]
struct Settings {
    enable_audio: bool,
    enable_video: bool,
    uri: Option<String>,
    fallback_uri: Option<String>,
    timeout: u64,
    retry_timeout: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enable_audio: true,
            enable_video: true,
            uri: None,
            fallback_uri: None,
            timeout: 5 * gst::SECOND_VAL,
            retry_timeout: 60 * gst::SECOND_VAL,
        }
    }
}

// Blocking buffer pad probe on the decodebin pads. Once blocked we have a running time for the
// current buffer that can later be used for offsetting
//
// This is used for the initial offsetting after starting of the stream and for "pausing" when
// buffering.
struct Block {
    pad: gst::Pad,
    probe_id: gst::PadProbeId,
    running_time: gst::ClockTime,
}

// Connects one decodebin source pad with fallbackswitch and the corresponding fallback input
struct Stream {
    // Fallback input stream
    //   for video: filesrc, decoder, converters, imagefreeze
    //   for audio: live audiotestsrc, converters
    fallback_input: gst::Element,

    // source pad from decodebin
    decodebin_srcpad: Option<gst::Pad>,
    decodebin_srcpad_block: Option<Block>,

    // clocksync for decodebin source pad
    clocksync: gst::Element,

    // fallbackswitch
    switch: gst::Element,

    // output source pad, connected to switch
    srcpad: gst::Pad,
}

struct State {
    // uridecodebin3
    source: gst::Element,
    source_is_live: bool,
    source_pending_restart: bool,
    // For restarting the source after shutting it down
    source_pending_restart_timeout: Option<gst::ClockId>,
    // For failing completely if we didn't recover after the retry timeout
    source_retry_timeout: Option<gst::ClockId>,

    // All our output streams, selected by properties
    video_stream: Option<Stream>,
    audio_stream: Option<Stream>,
    flow_combiner: gst_base::UniqueFlowCombiner,

    buffering_percent: u8,
    last_buffering_update: Option<Instant>,

    // Stream collection posted by decodebin3
    streams: Option<gst::StreamCollection>,

    // Configure settings
    settings: Settings,
}

struct FallbackSrc {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, GEnum)]
#[repr(u32)]
#[genum(type_name = "GstFallbackSourceStatus")]
enum Status {
    Stopped,
    Buffering,
    Retrying,
    Running,
}

static PROPERTIES: [subclass::Property; 7] = [
    subclass::Property("enable-audio", |name| {
        glib::ParamSpec::boolean(
            name,
            "Enable Audio",
            "Enable the audio stream, this will output silence if there's no audio in the configured URI",
            true,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("enable-video", |name| {
        glib::ParamSpec::boolean(
            name,
            "Enable Video",
            "Enable the video stream, this will output black or the fallback video if there's no video in the configured URI",
            true,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("uri", |name| {
        glib::ParamSpec::string(name, "URI", "URI to use", None, glib::ParamFlags::READWRITE)
    }),
    subclass::Property("fallback-uri", |name| {
        glib::ParamSpec::string(
            name,
            "Fallback URI",
            "Fallback URI to use for video in case the main stream doesn't work",
            None,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("timeout", |name| {
        glib::ParamSpec::uint64(
            name,
            "Timeout",
            "Timeout for switching to the fallback URI",
            0,
            std::u64::MAX,
            5 * gst::SECOND_VAL,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("retry-timeout", |name| {
        glib::ParamSpec::uint64(
            name,
            "Retry Timeout",
            "Timeout for stopping after repeated failure",
            0,
            std::u64::MAX,
            60 * gst::SECOND_VAL,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("status", |name| {
        glib::ParamSpec::enum_(
            name,
            "Status",
            "Current source status",
            Status::static_type(),
            Status::Stopped as i32,
            glib::ParamFlags::READABLE,
        )
    }),
];

impl ObjectSubclass for FallbackSrc {
    const NAME: &'static str = "FallbackSrc";
    type ParentType = gst::Bin;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(None),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Fallback Source",
            "Generic/Source",
            "Live source with uridecodebin3 and fallback image stream",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let src_pad_template = gst::PadTemplate::new(
            "audio",
            gst::PadDirection::Src,
            gst::PadPresence::Sometimes,
            &gst::Caps::new_any(),
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        let src_pad_template = gst::PadTemplate::new(
            "video",
            gst::PadDirection::Src,
            gst::PadPresence::Sometimes,
            &gst::Caps::new_any(),
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for FallbackSrc {
    glib_object_impl!();

    fn set_property(&self, obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        let element = obj.downcast_ref::<gst::Bin>().unwrap();

        match *prop {
            subclass::Property("enable-audio", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get_some().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: element,
                    "Changing enable-audio from {:?} to {:?}",
                    settings.enable_audio,
                    new_value,
                );
                settings.enable_audio = new_value;
            }
            subclass::Property("enable-video", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get_some().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: element,
                    "Changing enable-video from {:?} to {:?}",
                    settings.enable_video,
                    new_value,
                );
                settings.enable_video = new_value;
            }
            subclass::Property("uri", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: element,
                    "Changing URI from {:?} to {:?}",
                    settings.uri,
                    new_value,
                );
                settings.uri = new_value;
            }
            subclass::Property("fallback-uri", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: element,
                    "Changing Fallback URI from {:?} to {:?}",
                    settings.fallback_uri,
                    new_value,
                );
                settings.fallback_uri = new_value;
            }
            subclass::Property("timeout", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get_some().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: element,
                    "Changing timeout from {:?} to {:?}",
                    settings.timeout,
                    new_value,
                );
                settings.timeout = new_value;
            }
            subclass::Property("retry-timeout", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get_some().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: element,
                    "Changing Retry Timeout from {:?} to {:?}",
                    settings.retry_timeout,
                    new_value,
                );
                settings.retry_timeout = new_value;
            }
            _ => unimplemented!(),
        }
    }

    // Called whenever a value of a property is read. It can be called
    // at any time from any thread.
    #[allow(clippy::block_in_if_condition_stmt)]
    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("enable-audio", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.enable_audio.to_value())
            }
            subclass::Property("enable-video", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.enable_video.to_value())
            }
            subclass::Property("uri", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.uri.to_value())
            }
            subclass::Property("fallback-uri", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.fallback_uri.to_value())
            }
            subclass::Property("timeout", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.timeout.to_value())
            }
            subclass::Property("retry-timeout", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.retry_timeout.to_value())
            }
            subclass::Property("status", ..) => {
                let state_guard = self.state.lock().unwrap();

                // If we have no state then we'r stopped
                let state = match &*state_guard {
                    None => return Ok(Status::Stopped.to_value()),
                    Some(ref state) => state,
                };

                // If any restarts/retries are pending, we're retrying
                if state.source_pending_restart
                    || state.source_pending_restart_timeout.is_some()
                    || state.source_retry_timeout.is_some()
                {
                    return Ok(Status::Retrying.to_value());
                }

                // Otherwise if buffering < 100, we have no streams yet or of the expected
                // streams there is no decodebin source pad yet, we're buffering
                let mut have_audio = false;
                let mut have_video = false;
                if let Some(ref streams) = state.streams {
                    for stream in streams.iter() {
                        have_audio =
                            have_audio || stream.get_stream_type().contains(gst::StreamType::AUDIO);
                        have_video =
                            have_video || stream.get_stream_type().contains(gst::StreamType::VIDEO);
                    }
                }

                if state.buffering_percent < 100
                    || state.streams.is_none()
                    || (have_audio
                        && state
                            .audio_stream
                            .as_ref()
                            .map(|s| {
                                s.decodebin_srcpad.is_none() || s.decodebin_srcpad_block.is_some()
                            })
                            .unwrap_or(false))
                    || (have_video
                        && state
                            .video_stream
                            .as_ref()
                            .map(|s| {
                                s.decodebin_srcpad.is_none() || s.decodebin_srcpad_block.is_some()
                            })
                            .unwrap_or(false))
                {
                    return Ok(Status::Buffering.to_value());
                }

                // Otherwise we're running now
                Ok(Status::Running.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let bin = obj.downcast_ref::<gst::Bin>().unwrap();
        bin.set_suppressed_flags(gst::ElementFlags::SOURCE | gst::ElementFlags::SINK);
        bin.set_element_flags(gst::ElementFlags::SOURCE);
        bin.set_bin_flags(gst::BinFlags::STREAMS_AWARE);
    }
}

impl ElementImpl for FallbackSrc {
    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        element: &gst::Element,
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
        self.change_source_state(element, transition)?;

        // Ignore parent state change return to prevent spurious async/no-preroll return values
        // due to core state change bugs
        match transition {
            gst::StateChange::ReadyToPaused | gst::StateChange::PlayingToPaused => {
                Ok(gst::StateChangeSuccess::NoPreroll)
            }
            gst::StateChange::ReadyToNull => {
                self.stop(element)?;
                Ok(gst::StateChangeSuccess::Success)
            }
            _ => Ok(gst::StateChangeSuccess::Success),
        }
    }
}

impl BinImpl for FallbackSrc {
    fn handle_message(&self, bin: &gst::Bin, msg: gst::Message) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Buffering(ref m) => {
                // Don't forward upwards, we handle this internally
                self.handle_buffering(bin, m);
            }
            MessageView::StreamCollection(ref m) => {
                // Don't forward upwards, we are exposing streams based on properties
                // TODO: Do stream configuration via our own stream collection and handling
                // of stream select events
                self.handle_stream_collection(bin, m);
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
        element: &gst::Bin,
        uri: &str,
    ) -> Result<gst::Element, gst::StateChangeError> {
        let source = gst::ElementFactory::make("uridecodebin3", Some("uridecodebin"))
            .expect("No uridecodebin3 found");

        source.set_property("uri", &uri).unwrap();

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

            if let Err(msg) = src.handle_decodebin_pad_added(&element, pad) {
                element.post_error_message(&msg);
            }
        });
        let element_weak = element.downgrade();
        source.connect_pad_removed(move |_, pad| {
            let element = match element_weak.upgrade() {
                None => return,
                Some(element) => element,
            };
            let src = FallbackSrc::from_instance(&element);

            if let Err(msg) = src.handle_decodebin_pad_removed(&element, pad) {
                element.post_error_message(&msg);
            }
        });

        element.add_many(&[&source]).unwrap();

        Ok(source)
    }

    fn create_fallback_video_input(
        &self,
        element: &gst::Bin,
        fallback_uri: Option<&str>,
    ) -> Result<gst::Element, gst::StateChangeError> {
        let input = gst::Bin::new(Some("fallback_video"));

        let srcpad = match fallback_uri {
            Some(fallback_uri) => {
                let filesrc = gst::ElementFactory::make("filesrc", Some("fallback_filesrc"))
                    .expect("No filesrc found");
                let typefind = gst::ElementFactory::make("typefind", Some("fallback_typefind"))
                    .expect("No typefind found");
                let videoconvert =
                    gst::ElementFactory::make("videoconvert", Some("fallback_videoconvert"))
                        .expect("No videoconvert found");
                let videoscale =
                    gst::ElementFactory::make("videoscale", Some("fallback_videoscale"))
                        .expect("No videoscale found");
                let imagefreeze =
                    gst::ElementFactory::make("imagefreeze", Some("fallback_imagefreeze"))
                        .expect("No imagefreeze found");
                let clocksync = gst::ElementFactory::make("clocksync", Some("fallback_clocksync"))
                    .expect("No clocksync found");
                input
                    .add_many(&[
                        &filesrc,
                        &typefind,
                        &videoconvert,
                        &videoscale,
                        &imagefreeze,
                        &clocksync,
                    ])
                    .unwrap();
                gst::Element::link_many(&[&filesrc, &typefind]).unwrap();
                gst::Element::link_many(&[&videoconvert, &videoscale, &imagefreeze, &clocksync])
                    .unwrap();

                filesrc
                    .dynamic_cast_ref::<gst::URIHandler>()
                    .unwrap()
                    .set_uri(fallback_uri)
                    .map_err(|err| {
                        gst_error!(CAT, obj: element, "Failed to set fallback URI: {}", err);
                        gst_element_error!(
                            element,
                            gst::LibraryError::Settings,
                            ["Failed to set fallback URI: {}", err]
                        );
                        gst::StateChangeError
                    })?;

                let element_weak = element.downgrade();
                let input_weak = input.downgrade();
                let videoconvert_weak = videoconvert.downgrade();
                typefind
                    .connect("have-type", false, move |args| {
                        let typefind = args[0].get::<gst::Element>().unwrap().unwrap();
                        let _probability = args[1].get_some::<u32>().unwrap();
                        let caps = args[2].get::<gst::Caps>().unwrap().unwrap();

                        let element = match element_weak.upgrade() {
                            Some(element) => element,
                            None => return None,
                        };

                        let input = match input_weak.upgrade() {
                            Some(element) => element,
                            None => return None,
                        };

                        let videoconvert = match videoconvert_weak.upgrade() {
                            Some(element) => element,
                            None => return None,
                        };

                        let s = caps.get_structure(0).unwrap();
                        let decoder;
                        if s.get_name() == "image/jpeg" {
                            decoder = gst::ElementFactory::make("jpegdec", Some("decoder"))
                                .expect("jpegdec not found");
                        } else if s.get_name() == "image/png" {
                            decoder = gst::ElementFactory::make("pngdec", Some("decoder"))
                                .expect("pngdec not found");
                        } else {
                            gst_error!(CAT, obj: &element, "Unsupported caps {}", caps);
                            gst_element_error!(
                                element,
                                gst::StreamError::Format,
                                ["Unsupported caps {}", caps]
                            );
                            return None;
                        }

                        input.add(&decoder).unwrap();
                        decoder.sync_state_with_parent().unwrap();
                        if let Err(_err) =
                            gst::Element::link_many(&[&typefind, &decoder, &videoconvert])
                        {
                            gst_error!(CAT, obj: &element, "Can't link fallback image decoder");
                            gst_element_error!(
                                element,
                                gst::StreamError::Format,
                                ["Can't link fallback image decoder"]
                            );
                            return None;
                        }

                        None
                    })
                    .unwrap();

                clocksync.get_static_pad("src").unwrap()
            }
            None => {
                let videotestsrc =
                    gst::ElementFactory::make("videotestsrc", Some("fallback_videosrc"))
                        .expect("No videotestsrc found");
                input.add_many(&[&videotestsrc]).unwrap();

                videotestsrc.set_property_from_str("pattern", "black");
                videotestsrc.set_property("is-live", &true).unwrap();

                videotestsrc.get_static_pad("src").unwrap()
            }
        };

        input
            .add_pad(&gst::GhostPad::new(Some("src"), &srcpad).unwrap())
            .unwrap();

        Ok(input.upcast())
    }

    fn create_fallback_audio_input(
        &self,
        _element: &gst::Bin,
    ) -> Result<gst::Element, gst::StateChangeError> {
        let input = gst::Bin::new(Some("fallback_audio"));
        let audiotestsrc = gst::ElementFactory::make("audiotestsrc", Some("fallback_audiosrc"))
            .expect("No audiotestsrc found");
        input.add_many(&[&audiotestsrc]).unwrap();

        audiotestsrc.set_property_from_str("wave", "silence");
        audiotestsrc.set_property("is-live", &true).unwrap();

        let srcpad = audiotestsrc.get_static_pad("src").unwrap();
        input
            .add_pad(&gst::GhostPad::new(Some("src"), &srcpad).unwrap())
            .unwrap();

        Ok(input.upcast())
    }

    fn create_stream(
        &self,
        element: &gst::Bin,
        timeout: u64,
        is_audio: bool,
        fallback_uri: Option<&str>,
    ) -> Result<Stream, gst::StateChangeError> {
        let fallback_input = if is_audio {
            self.create_fallback_audio_input(element)?
        } else {
            self.create_fallback_video_input(element, fallback_uri)?
        };

        let switch =
            gst::ElementFactory::make("fallbackswitch", None).expect("No fallbackswitch found");
        let clocksync = gst::ElementFactory::make("clocksync", None).expect("No clocksync found");

        element
            .add_many(&[&fallback_input, &switch, &clocksync])
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
        switch.set_property("timeout", &timeout).unwrap();

        gst::Element::link_pads(&fallback_input, Some("src"), &switch, Some("fallback_sink"))
            .unwrap();
        gst::Element::link_pads(&clocksync, Some("src"), &switch, Some("sink")).unwrap();
        // clocksync sink pad is not connected to anything yet at this point!

        let srcpad = switch.get_static_pad("src").unwrap();
        let templ = element
            .get_pad_template(if is_audio { "audio" } else { "video" })
            .unwrap();
        let ghostpad =
            gst::GhostPad::new_from_template(Some(&templ.get_name()), &srcpad, &templ).unwrap();

        element.add_pad(&ghostpad).unwrap();

        let proxypad = ghostpad.get_internal().expect("no internal pad");
        let element_weak = element.downgrade();
        proxypad.set_chain_function(move |pad, _parent, buffer| {
            let element = match element_weak.upgrade() {
                None => return Err(gst::FlowError::Flushing),
                Some(element) => element,
            };

            let src = FallbackSrc::from_instance(&element);
            src.proxy_pad_chain(&element, pad, buffer)
        });

        Ok(Stream {
            fallback_input,
            decodebin_srcpad: None,
            decodebin_srcpad_block: None,
            clocksync,
            switch,
            srcpad: ghostpad.upcast(),
        })
    }

    fn start(&self, element: &gst::Element) -> Result<(), gst::StateChangeError> {
        let element = element.downcast_ref::<gst::Bin>().unwrap();

        gst_debug!(CAT, obj: element, "Starting");
        let mut state_guard = self.state.lock().unwrap();
        if state_guard.is_some() {
            return Err(gst::StateChangeError);
        }

        let settings = self.settings.lock().unwrap().clone();
        let uri = match settings.uri {
            Some(ref uri) => uri,
            None => {
                gst_error!(CAT, obj: element, "No URI configured");
                gst_element_error!(element, gst::LibraryError::Settings, ["No URI configured"]);
                return Err(gst::StateChangeError);
            }
        };

        let fallback_uri = &settings.fallback_uri;

        // Create main input
        let source = self.create_main_input(element, uri)?;

        let mut flow_combiner = gst_base::UniqueFlowCombiner::new();

        // Create video stream
        let video_stream = if settings.enable_video {
            let stream =
                self.create_stream(element, settings.timeout, false, fallback_uri.as_deref())?;
            flow_combiner.add_pad(&stream.srcpad);
            Some(stream)
        } else {
            None
        };

        // Create audio stream
        let audio_stream = if settings.enable_audio {
            let stream = self.create_stream(element, settings.timeout, true, None)?;
            flow_combiner.add_pad(&stream.srcpad);
            Some(stream)
        } else {
            None
        };

        *state_guard = Some(State {
            source,
            source_is_live: false,
            source_pending_restart: false,
            source_pending_restart_timeout: None,
            source_retry_timeout: None,
            video_stream,
            audio_stream,
            flow_combiner,
            buffering_percent: 100,
            last_buffering_update: None,
            streams: None,
            settings,
        });

        drop(state_guard);

        element.no_more_pads();

        element.notify("status");

        gst_debug!(CAT, obj: element, "Started");
        Ok(())
    }

    fn stop(&self, element: &gst::Element) -> Result<(), gst::StateChangeError> {
        let element = element.downcast_ref::<gst::Bin>().unwrap();

        gst_debug!(CAT, obj: element, "Stopping");
        let mut state_guard = self.state.lock().unwrap();
        let mut state = match state_guard.take() {
            Some(state) => state,
            None => return Ok(()),
        };
        drop(state_guard);

        element.notify("status");

        // In theory all streams should've been removed from uridecodebin's pad-removed signal
        // handler when going from Paused to Ready but better safe than sorry here
        for stream in [&state.video_stream, &state.audio_stream]
            .iter()
            .filter_map(|v| v.as_ref())
        {
            element.remove(&stream.switch).unwrap();
            element.remove(&stream.clocksync).unwrap();
            element.remove(&stream.fallback_input).unwrap();
            element.remove_pad(&stream.srcpad).unwrap();
        }
        state.video_stream = None;
        state.audio_stream = None;

        element.remove(&state.source).unwrap();

        if let Some(timeout) = state.source_pending_restart_timeout.take() {
            timeout.unschedule();
        }

        if let Some(timeout) = state.source_retry_timeout.take() {
            timeout.unschedule();
        }

        gst_debug!(CAT, obj: element, "Stopped");
        Ok(())
    }

    fn change_source_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<(), gst::StateChangeError> {
        let element = element.downcast_ref::<gst::Bin>().unwrap();

        gst_debug!(CAT, obj: element, "Changing source state: {:?}", transition);
        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            Some(state) => state,
            None => return Ok(()),
        };

        if transition.current() <= transition.next() && state.source_pending_restart {
            gst_debug!(
                CAT,
                obj: element,
                "Not starting source because pending restart"
            );
            return Ok(());
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
                    self.handle_source_error(element, state);
                }
            }
            Ok(res) => {
                gst_debug!(
                    CAT,
                    obj: element,
                    "Source changed state successfully: {:?}",
                    res
                );
                // Remember if the source is live
                if transition == gst::StateChange::ReadyToPaused {
                    let mut state_guard = self.state.lock().unwrap();
                    let state = state_guard.as_mut().expect("no state");
                    state.source_is_live = res == gst::StateChangeSuccess::NoPreroll;
                }
            }
        }

        Ok(())
    }

    fn proxy_pad_chain(
        &self,
        element: &gst::Bin,
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

    fn handle_decodebin_pad_added(
        &self,
        element: &gst::Bin,
        pad: &gst::Pad,
    ) -> Result<(), gst::ErrorMessage> {
        gst_debug!(
            CAT,
            obj: element,
            "Pad {} added to decodebin",
            pad.get_name(),
        );

        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return Ok(());
            }
            Some(state) => state,
        };

        let (type_, stream) = match pad.get_name() {
            x if x.starts_with("audio_") => ("audio", &mut state.audio_stream),
            x if x.starts_with("video_") => ("video", &mut state.video_stream),
            _ => {
                // TODO: handle subtitles etc
                return Ok(());
            }
        };

        let stream = match stream {
            None => {
                gst_debug!(CAT, obj: element, "No {} stream enabled", type_);
                return Ok(());
            }
            Some(Stream {
                decodebin_srcpad: Some(_),
                ..
            }) => {
                gst_debug!(CAT, obj: element, "Already configured a {} stream", type_);
                return Ok(());
            }
            Some(ref mut stream) => stream,
        };

        let sinkpad = stream.clocksync.get_static_pad("sink").unwrap();
        pad.link(&sinkpad).map_err(|err| {
            gst_error!(
                CAT,
                obj: element,
                "Failed to link decodebin pad to clocksync: {}",
                err
            );
            gst_error_msg!(
                gst::CoreError::Negotiation,
                ["Failed to link decodebin pad to clocksync: {}", err]
            )
        })?;

        stream.decodebin_srcpad = Some(pad.clone());
        stream.decodebin_srcpad_block = Some(self.add_decodebin_pad_probe(element, pad));

        drop(state_guard);
        element.notify("status");

        Ok(())
    }

    fn add_decodebin_pad_probe(&self, element: &gst::Bin, pad: &gst::Pad) -> Block {
        gst_debug!(CAT, obj: element, "Adding probe to pad {}", pad.get_name());

        let element_weak = element.downgrade();
        let probe_id = pad
            .add_probe(
                gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
                move |pad, info| {
                    let element = match element_weak.upgrade() {
                        None => return gst::PadProbeReturn::Pass,
                        Some(element) => element,
                    };
                    let buffer = match info.data {
                        Some(gst::PadProbeData::Buffer(ref buffer)) => buffer,
                        _ => unreachable!(),
                    };

                    let src = FallbackSrc::from_instance(&element);

                    if let Err(msg) = src.handle_decodebin_pad_blocked(&element, pad, buffer) {
                        element.post_error_message(&msg);
                    }

                    gst::PadProbeReturn::Ok
                },
            )
            .unwrap();

        Block {
            pad: pad.clone(),
            probe_id,
            running_time: gst::CLOCK_TIME_NONE,
        }
    }

    fn handle_decodebin_pad_blocked(
        &self,
        element: &gst::Bin,
        pad: &gst::Pad,
        buffer: &gst::BufferRef,
    ) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Called probe on pad {}", pad.get_name());

        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return Ok(());
            }
            Some(state) => state,
        };

        // Directly unblock for live streams
        if state.source_is_live {
            for block in [state.video_stream.as_mut(), state.audio_stream.as_mut()]
                .iter_mut()
                .filter_map(|s| s.as_mut())
                .filter_map(|s| s.decodebin_srcpad_block.take())
            {
                block.pad.remove_probe(block.probe_id);
            }

            gst_debug!(CAT, obj: element, "Live source, unblocking directly");

            drop(state_guard);
            element.notify("status");

            return Ok(());
        }

        // Update running time for this block
        let stream = if let Some(stream) = state
            .audio_stream
            .as_mut()
            .filter(|s| s.decodebin_srcpad.as_ref() == Some(pad))
        {
            stream
        } else if let Some(stream) = state
            .video_stream
            .as_mut()
            .filter(|s| s.decodebin_srcpad.as_ref() == Some(pad))
        {
            stream
        } else {
            unreachable!();
        };

        let block = match stream.decodebin_srcpad_block {
            Some(ref mut block) => block,
            None => return Ok(()),
        };

        let ev = pad
            .get_sticky_event(gst::EventType::Segment, 0)
            .ok_or_else(|| {
                gst_error!(CAT, obj: element, "Have no segment event");
                gst_error_msg!(gst::CoreError::Clock, ["Have no segment event"])
            })?;
        let segment = match ev.view() {
            gst::EventView::Segment(s) => s.get_segment(),
            _ => unreachable!(),
        };
        let segment = segment.downcast_ref::<gst::ClockTime>().ok_or_else(|| {
            gst_error!(CAT, obj: element, "Have no time segment");
            gst_error_msg!(gst::CoreError::Clock, ["Have no time segment"])
        })?;

        let pts = buffer.get_pts();
        let running_time = if pts < segment.get_start() {
            segment.get_start()
        } else if segment.get_stop().is_some() && pts >= segment.get_stop() {
            segment.get_stop()
        } else {
            segment.to_running_time(pts)
        };

        assert!(running_time.is_some());

        gst_debug!(
            CAT,
            obj: element,
            "Have block running time {} for pad {}",
            running_time,
            pad.get_name()
        );

        block.running_time = running_time;

        self.unblock_decodebin_pads(element, state);

        drop(state_guard);
        element.notify("status");

        Ok(())
    }

    fn unblock_decodebin_pads(&self, element: &gst::Bin, state: &mut State) {
        // Check if all streams are blocked and have a running time and we have
        // 100% buffering
        if state.buffering_percent < 100 {
            gst_debug!(
                CAT,
                obj: element,
                "Not unblocking yet: buffering {}%",
                state.buffering_percent
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
            have_audio = have_audio || stream.get_stream_type().contains(gst::StreamType::AUDIO);
            have_video = have_video || stream.get_stream_type().contains(gst::StreamType::VIDEO);
        }

        let want_audio = state.settings.enable_audio;
        let want_video = state.settings.enable_video;

        let audio_running_time = state
            .audio_stream
            .as_ref()
            .and_then(|s| s.decodebin_srcpad_block.as_ref().map(|b| b.running_time))
            .unwrap_or(gst::CLOCK_TIME_NONE);
        let video_running_time = state
            .video_stream
            .as_ref()
            .and_then(|s| s.decodebin_srcpad_block.as_ref().map(|b| b.running_time))
            .unwrap_or(gst::CLOCK_TIME_NONE);

        let audio_srcpad = state
            .audio_stream
            .as_ref()
            .and_then(|s| s.decodebin_srcpad.as_ref().cloned());
        let video_srcpad = state
            .video_stream
            .as_ref()
            .and_then(|s| s.decodebin_srcpad.as_ref().cloned());

        let audio_is_eos = audio_srcpad
            .as_ref()
            .map(|p| p.get_pad_flags().contains(gst::PadFlags::EOS))
            .unwrap_or(false);
        let video_is_eos = video_srcpad
            .as_ref()
            .map(|p| p.get_pad_flags().contains(gst::PadFlags::EOS))
            .unwrap_or(false);

        // If we need both, wait for both and take the minimum, otherwise take the one we need.
        // Also consider EOS, we'd never get a new running time after EOS so don't need to wait.
        // FIXME: All this surely can be simplified somehow

        let current_running_time = element.get_current_running_time();

        if have_audio && want_audio && have_video && want_video {
            if audio_running_time.is_none() && !audio_is_eos {
                gst_debug!(CAT, obj: element, "Waiting for audio pad to block");
                return;
            }
            if video_running_time.is_none() && !video_is_eos {
                gst_debug!(CAT, obj: element, "Waiting for video pad to block");
                return;
            }

            let min_running_time = if audio_is_eos {
                video_running_time
            } else if video_is_eos {
                audio_running_time
            } else {
                std::cmp::min(audio_running_time, video_running_time)
            };
            let offset = if current_running_time > min_running_time {
                (current_running_time - min_running_time).unwrap() as i64
            } else {
                -((min_running_time - current_running_time).unwrap() as i64)
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
                .and_then(|s| s.decodebin_srcpad_block.take())
            {
                if !audio_is_eos {
                    block.pad.set_offset(offset);
                }
                block.pad.remove_probe(block.probe_id);
            }

            if let Some(block) = state
                .video_stream
                .as_mut()
                .and_then(|s| s.decodebin_srcpad_block.take())
            {
                if !video_is_eos {
                    block.pad.set_offset(offset);
                }
                block.pad.remove_probe(block.probe_id);
            }
        } else if have_audio && want_audio {
            if audio_running_time.is_none() {
                gst_debug!(CAT, obj: element, "Waiting for audio pad to block");
                return;
            }

            let offset = if current_running_time > audio_running_time {
                (current_running_time - audio_running_time).unwrap() as i64
            } else {
                -((audio_running_time - current_running_time).unwrap() as i64)
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
                .and_then(|s| s.decodebin_srcpad_block.take())
            {
                if !audio_is_eos {
                    block.pad.set_offset(offset);
                }
                block.pad.remove_probe(block.probe_id);
            }
        } else if have_video && want_video {
            if video_running_time.is_none() {
                gst_debug!(CAT, obj: element, "Waiting for video pad to block");
                return;
            }

            let offset = if current_running_time > video_running_time {
                (current_running_time - video_running_time).unwrap() as i64
            } else {
                -((video_running_time - current_running_time).unwrap() as i64)
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
                .and_then(|s| s.decodebin_srcpad_block.take())
            {
                if !video_is_eos {
                    block.pad.set_offset(offset);
                }
                block.pad.remove_probe(block.probe_id);
            }
        }
    }

    fn handle_decodebin_pad_removed(
        &self,
        element: &gst::Bin,
        pad: &gst::Pad,
    ) -> Result<(), gst::ErrorMessage> {
        gst_debug!(
            CAT,
            obj: element,
            "Pad {} removed from decodebin",
            pad.get_name()
        );

        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return Ok(());
            }
            Some(state) => state,
        };

        // Don't have to do anything here other than forgetting about the pad. Unlinking will
        // automatically happen while the pad is being removed from decodebin and thus leaves the
        // bin hierarchy
        let stream = if let Some(stream) = state
            .audio_stream
            .as_mut()
            .filter(|s| s.decodebin_srcpad.as_ref() == Some(pad))
        {
            stream
        } else if let Some(stream) = state
            .video_stream
            .as_mut()
            .filter(|s| s.decodebin_srcpad.as_ref() == Some(pad))
        {
            stream
        } else {
            return Ok(());
        };

        stream.decodebin_srcpad = None;

        drop(state_guard);
        element.notify("status");

        Ok(())
    }

    fn handle_buffering(&self, element: &gst::Bin, m: &gst::message::Buffering) {
        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return;
            }
            Some(state) => state,
        };

        gst_debug!(CAT, obj: element, "Got buffering {}%", m.get_percent());

        state.buffering_percent = m.get_percent() as u8;
        if state.buffering_percent < 100 {
            state.last_buffering_update = Some(Instant::now());
            // Block decodebin pads if needed to pause
            if let Some(ref mut stream) = state.audio_stream {
                if stream.decodebin_srcpad_block.is_none() {
                    if let Some(ref pad) = stream.decodebin_srcpad {
                        stream.decodebin_srcpad_block =
                            Some(self.add_decodebin_pad_probe(element, pad));
                    }
                }
            }
            if let Some(ref mut stream) = state.video_stream {
                if stream.decodebin_srcpad_block.is_none() {
                    if let Some(ref pad) = stream.decodebin_srcpad {
                        stream.decodebin_srcpad_block =
                            Some(self.add_decodebin_pad_probe(element, pad));
                    }
                }
            }

            drop(state_guard);
            element.notify("status");
        } else {
            state.last_buffering_update = None;
            // Check if we can unblock now
            self.unblock_decodebin_pads(element, state);

            drop(state_guard);
            element.notify("status");
        }
    }

    fn handle_stream_collection(&self, element: &gst::Bin, m: &gst::message::StreamCollection) {
        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return;
            }
            Some(state) => state,
        };

        let streams = m.get_stream_collection();

        gst_debug!(
            CAT,
            obj: element,
            "Got stream collection {:?}",
            streams.debug()
        );

        let mut have_audio = false;
        let mut have_video = false;
        for stream in streams.iter() {
            have_audio = have_audio || stream.get_stream_type().contains(gst::StreamType::AUDIO);
            have_video = have_video || stream.get_stream_type().contains(gst::StreamType::VIDEO);
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

        self.unblock_decodebin_pads(element, state);

        drop(state_guard);
        element.notify("status");
    }

    fn handle_error(&self, element: &gst::Bin, m: &gst::message::Error) -> bool {
        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return false;
            }
            Some(state) => state,
        };

        let src = match m.get_src().and_then(|s| s.downcast::<gst::Element>().ok()) {
            None => return false,
            Some(src) => src,
        };

        gst_debug!(
            CAT,
            obj: element,
            "Got error message from {}",
            src.get_path_string()
        );

        if src == state.source || src.has_as_ancestor(&state.source) {
            self.handle_source_error(element, state);
            drop(state_guard);
            element.notify("status");
            return true;
        }

        false
    }

    fn handle_source_error(&self, element: &gst::Bin, state: &mut State) {
        gst_debug!(CAT, obj: element, "Handling source error");
        if state.source_pending_restart {
            gst_debug!(CAT, obj: element, "Source is already pending restart");
            return;
        }

        // Prevent state changes from changing the state in an uncoordinated way
        state.source_pending_restart = true;

        // Drop any EOS events from any source pads of the source that might happen because of the
        // error. We don't need to remove these pad probes because restarting the source will also
        // remove/add the pads again.
        for pad in state.source.get_src_pads() {
            pad.add_probe(
                gst::PadProbeType::EVENT_DOWNSTREAM,
                |_pad, info| match info.data {
                    Some(gst::PadProbeData::Event(ref event)) => {
                        if event.get_type() == gst::EventType::Eos {
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
            let source = match source_weak.upgrade() {
                None => return,
                Some(source) => source,
            };

            gst_debug!(CAT, obj: element, "Shutting down source");
            let _ = source.set_state(gst::State::Null);

            // Sleep for 1s before retrying

            let src = FallbackSrc::from_instance(element);

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
            gst_debug!(CAT, obj: element, "Waiting for 1s before retrying");
            let clock = gst::SystemClock::obtain();
            let wait_time = clock.get_time() + gst::SECOND;
            assert!(wait_time.is_some());
            assert!(state.source_pending_restart_timeout.is_none());

            let timeout = clock
                .new_single_shot_id(wait_time)
                .expect("can't create clock id");
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

                        // FIXME: Create a new uridecodebin3 because it currently is not reusable
                        // See https://gitlab.freedesktop.org/gstreamer/gst-plugins-base/-/issues/746
                        element.remove(&state.source).unwrap();

                        let source = src
                            .create_main_input(
                                element,
                                state.settings.uri.as_deref().expect("no uri"),
                            )
                            .expect("failed to create new source");

                        let old_source = mem::replace(&mut state.source, source.clone());

                        state.source_pending_restart = false;
                        state.source_pending_restart_timeout = None;
                        state.buffering_percent = 100;
                        state.last_buffering_update = None;
                        drop(state_guard);

                        // Drop old source after releasing the lock, it might call the pad-removed callback
                        // still
                        drop(old_source);

                        if source.sync_state_with_parent().is_err() {
                            gst_error!(CAT, obj: element, "Source failed to change state");
                            let _ = source.set_state(gst::State::Null);
                            let mut state_guard = src.state.lock().unwrap();
                            let state = state_guard.as_mut().expect("no state");
                            src.handle_source_error(element, state);
                        }
                    });
                })
                .expect("Failed to wait async");
            state.source_pending_restart_timeout = Some(timeout);
        });
    }

    #[allow(clippy::block_in_if_condition_stmt)]
    fn handle_switch_active_pad_change(&self, element: &gst::Bin) {
        let mut state_guard = self.state.lock().unwrap();
        let state = match &mut *state_guard {
            None => {
                return;
            }
            Some(state) => state,
        };

        let mut have_audio = false;
        let mut have_video = false;
        if let Some(ref streams) = state.streams {
            for stream in streams.iter() {
                have_audio =
                    have_audio || stream.get_stream_type().contains(gst::StreamType::AUDIO);
                have_video =
                    have_video || stream.get_stream_type().contains(gst::StreamType::VIDEO);
            }
        }

        // If we have neither audio nor video (no streams yet), or active pad for the ones we have
        // is the fallback pad then start the retry timeout unless it was started already.
        // Otherwise cancel the retry timeout.
        if (!have_audio && !have_video)
            || (have_audio
                && state
                    .audio_stream
                    .as_ref()
                    .and_then(|s| {
                        s.switch
                            .get_property("active-pad")
                            .unwrap()
                            .get::<gst::Pad>()
                            .unwrap()
                    })
                    .map(|p| p.get_name() == "fallback_sink")
                    .unwrap_or(true))
            || (have_video
                && state
                    .video_stream
                    .as_ref()
                    .and_then(|s| {
                        s.switch
                            .get_property("active-pad")
                            .unwrap()
                            .get::<gst::Pad>()
                            .unwrap()
                    })
                    .map(|p| p.get_name() == "fallback_sink")
                    .unwrap_or(true))
        {
            gst_warning!(CAT, obj: element, "Switched to fallback stream");

            // If we're not actively buffering right now let's restart the source
            if state.buffering_percent == 100
                || state
                    .last_buffering_update
                    .map(|i| i.elapsed() >= Duration::from_nanos(state.settings.timeout))
                    .unwrap_or(true)
            {
                gst_debug!(CAT, obj: element, "Not buffering, restarting source");
                self.handle_source_error(element, state);
            }

            if state.source_retry_timeout.is_none() {
                let clock = gst::SystemClock::obtain();
                let wait_time =
                    clock.get_time() + gst::ClockTime::from(state.settings.retry_timeout);
                assert!(wait_time.is_some());

                gst_debug!(CAT, obj: element, "Starting retry timeout");
                let timeout = clock
                    .new_single_shot_id(wait_time)
                    .expect("can't create clock id");

                let element_weak = element.downgrade();
                timeout
                    .wait_async(move |_clock, _time, _id| {
                        let element = match element_weak.upgrade() {
                            None => return,
                            Some(element) => element,
                        };

                        element.call_async(|element| {
                            let src = FallbackSrc::from_instance(element);
                            let mut state_guard = src.state.lock().unwrap();
                            let state = match &mut *state_guard {
                                None => return,
                                Some(ref mut state) => state,
                            };
                            if state.source_retry_timeout.take().is_none() {
                                return;
                            }
                            if let Some(timeout) = state.source_pending_restart_timeout.take() {
                                timeout.unschedule();
                            }
                            state.source_pending_restart = false;
                            drop(state_guard);

                            gst_element_warning!(
                                element,
                                gst::ResourceError::OpenRead,
                                ["Failed to start playback"]
                            );
                            gst_warning!(CAT, obj: element, "Retry timeout, finishing");

                            for pad in element.get_src_pads() {
                                element.call_async(move |_element| {
                                    pad.push_event(gst::Event::new_eos().build());
                                });
                            }
                        });
                    })
                    .expect("failed to wait async");

                state.source_retry_timeout = Some(timeout);

                drop(state_guard);
                element.notify("status");
            }
        } else if let Some(timeout) = state.source_retry_timeout.take() {
            gst_debug!(CAT, obj: element, "Unscheduling retry timeout");
            timeout.unschedule();

            drop(state_guard);
            element.notify("status");
        }
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "fallbacksrc",
        gst::Rank::None,
        FallbackSrc::get_type(),
    )
}
