// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2020 Seungha Yang <seungha@centricular.com>
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

use std::sync::{atomic::AtomicBool, atomic::Ordering, Mutex};

use once_cell::sync::Lazy;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "fallbacksrc-video-source",
        gst::DebugColorFlags::empty(),
        Some("Fallback Video Source Bin"),
    )
});

#[derive(Debug, Clone)]
struct Settings {
    uri: Option<String>,
    min_latency: gst::ClockTime,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            uri: None,
            min_latency: gst::ClockTime::ZERO,
        }
    }
}

struct State {
    source: gst::Element,
}

pub struct VideoFallbackSource {
    srcpad: gst::GhostPad,
    got_error: AtomicBool,

    state: Mutex<Option<State>>,
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for VideoFallbackSource {
    const NAME: &'static str = "FallbackSrcVideoFallbackSource";
    type Type = super::VideoFallbackSource;
    type ParentType = gst::Bin;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::GhostPad::builder_with_template(&templ, Some(&templ.name())).build();

        Self {
            srcpad,
            got_error: AtomicBool::new(false),
            state: Mutex::new(None),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for VideoFallbackSource {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_string(
                    "uri",
                    "URI",
                    "URI to use for video in case the main stream doesn't work",
                    None,
                    glib::ParamFlags::READWRITE | glib::ParamFlags::CONSTRUCT_ONLY,
                ),
                glib::ParamSpec::new_uint64(
                    "min-latency",
                    "Minimum Latency",
                    "Minimum Latency",
                    0,
                    std::u64::MAX,
                    0,
                    glib::ParamFlags::READWRITE | glib::ParamFlags::CONSTRUCT_ONLY,
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
            "min-latency" => {
                let mut settings = self.settings.lock().unwrap();
                let new_value = value.get().expect("type checked upstream");
                gst_info!(
                    CAT,
                    obj: obj,
                    "Changing Minimum Latency from {} to {}",
                    settings.min_latency,
                    new_value,
                );
                settings.min_latency = new_value;
            }
            _ => unreachable!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "uri" => {
                let settings = self.settings.lock().unwrap();
                settings.uri.to_value()
            }
            "min-latency" => {
                let settings = self.settings.lock().unwrap();
                settings.min_latency.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.set_suppressed_flags(gst::ElementFlags::SOURCE | gst::ElementFlags::SINK);
        obj.set_element_flags(gst::ElementFlags::SOURCE);
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for VideoFallbackSource {
    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![src_pad_template]
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

        match transition {
            gst::StateChange::ReadyToNull => {
                self.stop(element);
            }
            _ => (),
        }

        Ok(gst::StateChangeSuccess::Success)
    }
}

impl BinImpl for VideoFallbackSource {
    #[allow(clippy::single_match)]
    fn handle_message(&self, bin: &Self::Type, msg: gst::Message) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Error(err) => {
                if self
                    .got_error
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    gst_warning!(CAT, obj: bin, "Got error {:?}", err);
                    self.parent_handle_message(bin, msg)
                } else {
                    // Suppress error message if we posted error previously.
                    // Otherwise parent fallbacksrc would be confused by
                    // multiple error message.
                    gst_debug!(CAT, obj: bin, "Ignore error {:?}", err);
                }
            }
            _ => self.parent_handle_message(bin, msg),
        }
    }
}

impl VideoFallbackSource {
    fn file_src_for_uri(
        &self,
        element: &super::VideoFallbackSource,
        uri: Option<&str>,
    ) -> Option<gst::Element> {
        uri?;

        let uri = uri.unwrap();
        let filesrc = gst::ElementFactory::make("filesrc", Some("fallback_filesrc"))
            .expect("No filesrc found");

        if let Err(err) = filesrc
            .dynamic_cast_ref::<gst::URIHandler>()
            .unwrap()
            .set_uri(uri)
        {
            gst_warning!(CAT, obj: element, "Failed to set URI: {}", err);
            return None;
        }

        if filesrc.set_state(gst::State::Ready).is_err() {
            gst_warning!(CAT, obj: element, "Couldn't set state READY");
            let _ = filesrc.set_state(gst::State::Null);
            return None;
        }

        // To invoke GstBaseSrc::start() method, activate pad manually.
        // filesrc will check whether given file is readable or not
        // via open() and fstat() in there.
        let pad = filesrc.static_pad("src").unwrap();
        if pad.set_active(true).is_err() {
            gst_warning!(CAT, obj: element, "Couldn't active pad");
            let _ = filesrc.set_state(gst::State::Null);
            return None;
        }

        Some(filesrc)
    }

    fn create_source(
        &self,
        element: &super::VideoFallbackSource,
        min_latency: gst::ClockTime,
        uri: Option<&str>,
    ) -> gst::Element {
        gst_debug!(CAT, obj: element, "Creating source with uri {:?}", uri);

        let source = gst::Bin::new(None);
        let filesrc = self.file_src_for_uri(element, uri);

        let srcpad = match filesrc {
            Some(filesrc) => {
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
                    .or_else(|_| -> Result<_, glib::BoolError> {
                        let identity =
                            gst::ElementFactory::make("identity", Some("fallback_clocksync"))?;
                        identity.set_property("sync", &true).unwrap();
                        Ok(identity)
                    })
                    .expect("No clocksync or identity found");
                let queue = gst::ElementFactory::make("queue", Some("fallback_queue"))
                    .expect("No queue found");
                queue
                    .set_properties(&[
                        ("max-size-buffers", &0u32),
                        ("max-size-bytes", &0u32),
                        (
                            "max-size-time",
                            &min_latency.max(5 * gst::ClockTime::SECOND).nseconds(),
                        ),
                    ])
                    .unwrap();

                source
                    .add_many(&[
                        &filesrc,
                        &typefind,
                        &videoconvert,
                        &videoscale,
                        &imagefreeze,
                        &clocksync,
                        &queue,
                    ])
                    .unwrap();
                gst::Element::link_many(&[&filesrc, &typefind]).unwrap();
                gst::Element::link_many(&[
                    &videoconvert,
                    &videoscale,
                    &imagefreeze,
                    &clocksync,
                    &queue,
                ])
                .unwrap();

                if imagefreeze.set_property("is-live", &true).is_err() {
                    gst_error!(
                        CAT,
                        obj: element,
                        "imagefreeze does not support live mode, this will probably misbehave"
                    );
                    gst::element_warning!(
                        element,
                        gst::LibraryError::Settings,
                        ["imagefreeze does not support live mode, this will probably misbehave"]
                    );
                }

                let element_weak = element.downgrade();
                let source_weak = source.downgrade();
                let videoconvert_weak = videoconvert.downgrade();
                typefind
                    .connect("have-type", false, move |args| {
                        let typefind = args[0].get::<gst::Element>().unwrap();
                        let _probability = args[1].get::<u32>().unwrap();
                        let caps = args[2].get::<gst::Caps>().unwrap();

                        let element = match element_weak.upgrade() {
                            Some(element) => element,
                            None => return None,
                        };

                        let source = match source_weak.upgrade() {
                            Some(element) => element,
                            None => return None,
                        };

                        let videoconvert = match videoconvert_weak.upgrade() {
                            Some(element) => element,
                            None => return None,
                        };

                        let s = caps.structure(0).unwrap();
                        let decoder;
                        if s.name() == "image/jpeg" {
                            decoder = gst::ElementFactory::make("jpegdec", Some("decoder"))
                                .expect("jpegdec not found");
                        } else if s.name() == "image/png" {
                            decoder = gst::ElementFactory::make("pngdec", Some("decoder"))
                                .expect("pngdec not found");
                        } else {
                            gst_error!(CAT, obj: &element, "Unsupported caps {}", caps);
                            gst::element_error!(
                                element,
                                gst::StreamError::Format,
                                ["Unsupported caps {}", caps]
                            );
                            return None;
                        }

                        source.add(&decoder).unwrap();
                        decoder.sync_state_with_parent().unwrap();
                        if let Err(_err) =
                            gst::Element::link_many(&[&typefind, &decoder, &videoconvert])
                        {
                            gst_error!(CAT, obj: &element, "Can't link fallback image decoder");
                            gst::element_error!(
                                element,
                                gst::StreamError::Format,
                                ["Can't link fallback image decoder"]
                            );
                            return None;
                        }

                        None
                    })
                    .unwrap();

                queue.static_pad("src").unwrap()
            }
            None => {
                let videotestsrc =
                    gst::ElementFactory::make("videotestsrc", Some("fallback_videosrc"))
                        .expect("No videotestsrc found");
                source.add_many(&[&videotestsrc]).unwrap();

                videotestsrc.set_property_from_str("pattern", "black");
                videotestsrc.set_property("is-live", &true).unwrap();

                videotestsrc.static_pad("src").unwrap()
            }
        };

        source
            .add_pad(
                &gst::GhostPad::builder(Some("src"), gst::PadDirection::Src)
                    .build_with_target(&srcpad)
                    .unwrap(),
            )
            .unwrap();

        source.upcast()
    }

    fn start(
        &self,
        element: &super::VideoFallbackSource,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_debug!(CAT, obj: element, "Starting");

        let mut state_guard = self.state.lock().unwrap();
        if state_guard.is_some() {
            gst_error!(CAT, obj: element, "State struct wasn't cleared");
            return Err(gst::StateChangeError);
        }

        let settings = self.settings.lock().unwrap().clone();
        let uri = &settings.uri;
        let source = self.create_source(element, settings.min_latency, uri.as_deref());

        element.add(&source).unwrap();

        let srcpad = source.static_pad("src").unwrap();
        let _ = self.srcpad.set_target(Some(&srcpad));

        *state_guard = Some(State { source });

        Ok(gst::StateChangeSuccess::Success)
    }

    fn stop(&self, element: &super::VideoFallbackSource) {
        gst_debug!(CAT, obj: element, "Stopping");

        let mut state_guard = self.state.lock().unwrap();
        let state = match state_guard.take() {
            Some(state) => state,
            None => return,
        };

        drop(state_guard);

        let _ = state.source.set_state(gst::State::Null);
        let _ = self.srcpad.set_target(None::<&gst::Pad>);
        element.remove(&state.source).unwrap();
        self.got_error.store(false, Ordering::Relaxed);
        gst_debug!(CAT, obj: element, "Stopped");
    }
}
