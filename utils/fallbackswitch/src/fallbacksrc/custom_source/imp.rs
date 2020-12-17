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
use gst::{gst_debug, gst_element_error, gst_error, gst_error_msg};

use std::{mem, sync::Mutex};

use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "fallbacksrc-custom-source",
        gst::DebugColorFlags::empty(),
        Some("Fallback Custom Source Bin"),
    )
});

static PROPERTIES: [subclass::Property; 1] = [subclass::Property("source", |name| {
    glib::ParamSpec::object(
        name,
        "Source",
        "Source",
        gst::Element::static_type(),
        glib::ParamFlags::WRITABLE | glib::ParamFlags::CONSTRUCT_ONLY,
    )
})];

struct Stream {
    source_pad: gst::Pad,
    ghost_pad: gst::GhostPad,
    // Dummy stream we created
    stream: gst::Stream,
}

struct State {
    pads: Vec<Stream>,
    num_audio: usize,
    num_video: usize,
}

pub struct CustomSource {
    source: OnceCell<gst::Element>,
    state: Mutex<State>,
}

impl ObjectSubclass for CustomSource {
    const NAME: &'static str = "FallbackSrcCustomSource";
    type Type = super::CustomSource;
    type ParentType = gst::Bin;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib::object_subclass!();

    fn new() -> Self {
        Self {
            source: OnceCell::default(),
            state: Mutex::new(State {
                pads: vec![],
                num_audio: 0,
                num_video: 0,
            }),
        }
    }

    fn class_init(klass: &mut Self::Class) {
        let src_pad_template = gst::PadTemplate::new(
            "audio_%u",
            gst::PadDirection::Src,
            gst::PadPresence::Sometimes,
            &gst::Caps::new_any(),
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        let src_pad_template = gst::PadTemplate::new(
            "video_%u",
            gst::PadDirection::Src,
            gst::PadPresence::Sometimes,
            &gst::Caps::new_any(),
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);
        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for CustomSource {
    fn set_property(&self, obj: &Self::Type, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("source", ..) => {
                let source = value.get::<gst::Element>().unwrap().unwrap();
                self.source.set(source.clone()).unwrap();
                obj.add(&source).unwrap();
            }
            _ => unreachable!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.set_suppressed_flags(gst::ElementFlags::SOURCE | gst::ElementFlags::SINK);
        obj.set_element_flags(gst::ElementFlags::SOURCE);
        obj.set_bin_flags(gst::BinFlags::STREAMS_AWARE);
    }
}

impl ElementImpl for CustomSource {
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

        let res = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToNull => {
                self.stop(element)?;
            }
            _ => (),
        }

        Ok(res)
    }
}

impl BinImpl for CustomSource {
    #[allow(clippy::single_match)]
    fn handle_message(&self, bin: &Self::Type, msg: gst::Message) {
        use gst::MessageView;

        match msg.view() {
            MessageView::StreamCollection(_) => {
                // TODO: Drop stream collection message for now, we only create a simple custom
                // one here so that fallbacksrc can know about our streams. It is never
                // forwarded.
                if let Err(msg) = self.handle_source_no_more_pads(&bin) {
                    bin.post_error_message(msg);
                }
            }
            _ => self.parent_handle_message(bin, msg),
        }
    }
}

impl CustomSource {
    fn start(
        &self,
        element: &super::CustomSource,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_debug!(CAT, obj: element, "Starting");
        let source = self.source.get().unwrap();

        let templates = source.get_pad_template_list();

        if templates
            .iter()
            .any(|templ| templ.get_property_presence() == gst::PadPresence::Request)
        {
            gst_error!(CAT, obj: element, "Request pads not supported");
            gst_element_error!(
                element,
                gst::LibraryError::Settings,
                ["Request pads not supported"]
            );
            return Err(gst::StateChangeError);
        }

        let has_sometimes_pads = templates
            .iter()
            .any(|templ| templ.get_property_presence() == gst::PadPresence::Sometimes);

        // Handle all source pads that already exist
        for pad in source.get_src_pads() {
            if let Err(msg) = self.handle_source_pad_added(&element, &pad) {
                element.post_error_message(msg);
                return Err(gst::StateChangeError);
            }
        }

        if !has_sometimes_pads {
            if let Err(msg) = self.handle_source_no_more_pads(&element) {
                element.post_error_message(msg);
                return Err(gst::StateChangeError);
            }
        } else {
            gst_debug!(CAT, obj: element, "Found sometimes pads");

            let element_weak = element.downgrade();
            source.connect_pad_added(move |_, pad| {
                let element = match element_weak.upgrade() {
                    None => return,
                    Some(element) => element,
                };
                let src = CustomSource::from_instance(&element);

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
                let src = CustomSource::from_instance(&element);

                if let Err(msg) = src.handle_source_pad_removed(&element, pad) {
                    element.post_error_message(msg);
                }
            });

            let element_weak = element.downgrade();
            source.connect_no_more_pads(move |_| {
                let element = match element_weak.upgrade() {
                    None => return,
                    Some(element) => element,
                };
                let src = CustomSource::from_instance(&element);

                if let Err(msg) = src.handle_source_no_more_pads(&element) {
                    element.post_error_message(msg);
                }
            });
        }

        Ok(gst::StateChangeSuccess::Success)
    }

    fn handle_source_pad_added(
        &self,
        element: &super::CustomSource,
        pad: &gst::Pad,
    ) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Source added pad {}", pad.get_name());

        let mut state = self.state.lock().unwrap();

        let mut stream_type = None;

        // Take stream type from stream-start event if we can
        if let Some(event) = pad.get_sticky_event(gst::EventType::StreamStart, 0) {
            if let gst::EventView::StreamStart(ev) = event.view() {
                stream_type = ev.get_stream().map(|s| s.get_stream_type());
            }
        }

        // Otherwise from the caps
        if stream_type.is_none() {
            let caps = match pad
                .get_current_caps()
                .unwrap_or_else(|| pad.query_caps(None))
            {
                caps if !caps.is_any() && !caps.is_empty() => caps,
                _ => {
                    gst_error!(CAT, obj: element, "Pad {} had no caps", pad.get_name());
                    return Err(gst_error_msg!(
                        gst::CoreError::Negotiation,
                        ["Pad had no caps"]
                    ));
                }
            };

            let s = caps.get_structure(0).unwrap();

            if s.get_name().starts_with("audio/") {
                stream_type = Some(gst::StreamType::AUDIO);
            } else if s.get_name().starts_with("video/") {
                stream_type = Some(gst::StreamType::VIDEO);
            } else {
                return Ok(());
            }
        }

        let stream_type = stream_type.unwrap();

        let (templ, name) = if stream_type.contains(gst::StreamType::AUDIO) {
            let name = format!("audio_{}", state.num_audio);
            state.num_audio += 1;
            (element.get_pad_template("audio_%u").unwrap(), name)
        } else {
            let name = format!("video_{}", state.num_video);
            state.num_video += 1;
            (element.get_pad_template("video_%u").unwrap(), name)
        };

        let ghost_pad = gst::GhostPad::builder_with_template(&templ, Some(&name))
            .build_with_target(pad)
            .unwrap();

        let stream = Stream {
            source_pad: pad.clone(),
            ghost_pad: ghost_pad.clone().upcast(),
            // TODO: We only add the stream type right now
            stream: gst::Stream::new(None, None, stream_type, gst::StreamFlags::empty()),
        };
        state.pads.push(stream);
        drop(state);

        ghost_pad.set_active(true).unwrap();
        element.add_pad(&ghost_pad).unwrap();

        Ok(())
    }

    fn handle_source_pad_removed(
        &self,
        element: &super::CustomSource,
        pad: &gst::Pad,
    ) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Source removed pad {}", pad.get_name());

        let mut state = self.state.lock().unwrap();
        let (i, stream) = match state
            .pads
            .iter()
            .enumerate()
            .find(|(_i, p)| &p.source_pad == pad)
        {
            None => return Ok(()),
            Some(v) => v,
        };

        let ghost_pad = stream.ghost_pad.clone();
        state.pads.remove(i);
        drop(state);

        ghost_pad.set_active(false).unwrap();
        let _ = ghost_pad.set_target(None::<&gst::Pad>);
        let _ = element.remove_pad(&ghost_pad);

        Ok(())
    }

    fn handle_source_no_more_pads(
        &self,
        element: &super::CustomSource,
    ) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Source signalled no-more-pads");

        let state = self.state.lock().unwrap();
        let streams = state
            .pads
            .iter()
            .map(|p| p.stream.clone())
            .collect::<Vec<_>>();
        let collection = gst::StreamCollection::builder(None)
            .streams(&streams)
            .build();
        drop(state);

        element.no_more_pads();

        let _ = element.post_message(
            gst::message::StreamsSelected::builder(&collection)
                .src(element)
                .build(),
        );

        Ok(())
    }

    fn stop(
        &self,
        element: &super::CustomSource,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_debug!(CAT, obj: element, "Stopping");

        let mut state = self.state.lock().unwrap();
        let pads = mem::replace(&mut state.pads, vec![]);
        state.num_audio = 0;
        state.num_video = 0;
        drop(state);

        for pad in pads {
            let _ = pad.ghost_pad.set_target(None::<&gst::Pad>);
            let _ = element.remove_pad(&pad.ghost_pad);
        }

        Ok(gst::StateChangeSuccess::Success)
    }
}
