// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::glib::SignalHandlerId;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::{mem, sync::Mutex};

use gst::glib::once_cell::sync::Lazy;
use gst::glib::once_cell::sync::OnceCell;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "fallbacksrc-custom-source",
        gst::DebugColorFlags::empty(),
        Some("Fallback Custom Source Bin"),
    )
});

struct Stream {
    source_pad: gst::Pad,
    ghost_pad: gst::GhostPad,
    // Dummy stream we created
    stream: gst::Stream,
}

#[derive(Default)]
struct State {
    pads: Vec<Stream>,
    num_audio: usize,
    num_video: usize,
    pad_added_sig_id: Option<SignalHandlerId>,
    pad_removed_sig_id: Option<SignalHandlerId>,
    no_more_pads_sig_id: Option<SignalHandlerId>,
}

#[derive(Default)]
pub struct CustomSource {
    source: OnceCell<gst::Element>,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for CustomSource {
    const NAME: &'static str = "GstFallbackSrcCustomSource";
    type Type = super::CustomSource;
    type ParentType = gst::Bin;
}

impl ObjectImpl for CustomSource {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpecObject::builder::<gst::Element>("source")
                .nick("Source")
                .blurb("Source")
                .write_only()
                .construct_only()
                .build()]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "source" => {
                let source = value.get::<gst::Element>().unwrap();
                self.source.set(source.clone()).unwrap();
                self.obj().add(&source).unwrap();
            }
            _ => unreachable!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.set_suppressed_flags(gst::ElementFlags::SOURCE | gst::ElementFlags::SINK);
        obj.set_element_flags(gst::ElementFlags::SOURCE);
        obj.set_bin_flags(gst::BinFlags::STREAMS_AWARE);
    }
}

impl GstObjectImpl for CustomSource {}

impl ElementImpl for CustomSource {
    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let audio_src_pad_template = gst::PadTemplate::new(
                "audio_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &gst::Caps::new_any(),
            )
            .unwrap();

            let video_src_pad_template = gst::PadTemplate::new(
                "video_%u",
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
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                self.start()?;
            }
            _ => (),
        }

        let res = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::ReadyToNull | gst::StateChange::NullToNull => {
                self.stop();
            }
            _ => (),
        }

        Ok(res)
    }
}

impl BinImpl for CustomSource {
    #[allow(clippy::single_match)]
    fn handle_message(&self, msg: gst::Message) {
        use gst::MessageView;

        match msg.view() {
            MessageView::StreamCollection(_) => {
                // TODO: Drop stream collection message for now, we only create a simple custom
                // one here so that fallbacksrc can know about our streams. It is never
                // forwarded.
                self.handle_source_no_more_pads();
            }
            _ => self.parent_handle_message(msg),
        }
    }
}

impl CustomSource {
    fn start(&self) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::debug!(CAT, imp: self, "Starting");
        let source = self.source.get().unwrap();

        let templates = source.pad_template_list();

        if templates
            .iter()
            .any(|templ| templ.presence() == gst::PadPresence::Request)
        {
            gst::error!(CAT, imp: self, "Request pads not supported");
            gst::element_imp_error!(
                self,
                gst::LibraryError::Settings,
                ["Request pads not supported"]
            );
            return Err(gst::StateChangeError);
        }

        let has_sometimes_pads = templates
            .iter()
            .any(|templ| templ.presence() == gst::PadPresence::Sometimes);

        // Handle all source pads that already exist
        for pad in source.src_pads() {
            if let Err(msg) = self.handle_source_pad_added(&pad) {
                self.post_error_message(msg);
                return Err(gst::StateChangeError);
            }
        }

        if !has_sometimes_pads {
            self.handle_source_no_more_pads();
        } else {
            gst::debug!(CAT, imp: self, "Found sometimes pads");

            let pad_added_sig_id = source.connect_pad_added(move |source, pad| {
                let element = match source
                    .parent()
                    .and_then(|p| p.downcast::<super::CustomSource>().ok())
                {
                    Some(element) => element,
                    None => return,
                };
                let src = element.imp();

                if let Err(msg) = src.handle_source_pad_added(pad) {
                    element.post_error_message(msg);
                }
            });
            let pad_removed_sig_id = source.connect_pad_removed(move |source, pad| {
                let element = match source
                    .parent()
                    .and_then(|p| p.downcast::<super::CustomSource>().ok())
                {
                    Some(element) => element,
                    None => return,
                };
                let src = element.imp();

                src.handle_source_pad_removed(pad);
            });

            let no_more_pads_sig_id = source.connect_no_more_pads(move |source| {
                let element = match source
                    .parent()
                    .and_then(|p| p.downcast::<super::CustomSource>().ok())
                {
                    Some(element) => element,
                    None => return,
                };
                let src = element.imp();

                src.handle_source_no_more_pads();
            });

            let mut state = self.state.lock().unwrap();
            state.pad_added_sig_id = Some(pad_added_sig_id);
            state.pad_removed_sig_id = Some(pad_removed_sig_id);
            state.no_more_pads_sig_id = Some(no_more_pads_sig_id);
        }

        Ok(gst::StateChangeSuccess::Success)
    }

    fn handle_source_pad_added(&self, pad: &gst::Pad) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Source added pad {}", pad.name());

        let mut state = self.state.lock().unwrap();

        let mut stream_type = None;

        // Take stream type from stream-start event if we can
        if let Some(ev) = pad.sticky_event::<gst::event::StreamStart>(0) {
            stream_type = ev.stream().map(|s| s.stream_type());
        }

        // Otherwise from the caps
        if stream_type.is_none() {
            let caps = match pad.current_caps().unwrap_or_else(|| pad.query_caps(None)) {
                caps if !caps.is_any() && !caps.is_empty() => caps,
                _ => {
                    gst::error!(CAT, imp: self, "Pad {} had no caps", pad.name());
                    return Err(gst::error_msg!(
                        gst::CoreError::Negotiation,
                        ["Pad had no caps"]
                    ));
                }
            };

            let s = caps.structure(0).unwrap();

            if s.name().starts_with("audio/") {
                stream_type = Some(gst::StreamType::AUDIO);
            } else if s.name().starts_with("video/") {
                stream_type = Some(gst::StreamType::VIDEO);
            } else {
                return Ok(());
            }
        }

        let stream_type = stream_type.unwrap();

        let (templ, name) = if stream_type.contains(gst::StreamType::AUDIO) {
            let name = format!("audio_{}", state.num_audio);
            state.num_audio += 1;
            (self.obj().pad_template("audio_%u").unwrap(), name)
        } else {
            let name = format!("video_{}", state.num_video);
            state.num_video += 1;
            (self.obj().pad_template("video_%u").unwrap(), name)
        };

        let ghost_pad = gst::GhostPad::builder_from_template_with_target(&templ, pad)
            .unwrap()
            .name(name)
            .build();

        let stream = Stream {
            source_pad: pad.clone(),
            ghost_pad: ghost_pad.clone().upcast(),
            // TODO: We only add the stream type right now
            stream: gst::Stream::new(None, None, stream_type, gst::StreamFlags::empty()),
        };
        state.pads.push(stream);
        drop(state);

        ghost_pad.set_active(true).unwrap();
        self.obj().add_pad(&ghost_pad).unwrap();

        Ok(())
    }

    fn handle_source_pad_removed(&self, pad: &gst::Pad) {
        gst::debug!(CAT, imp: self, "Source removed pad {}", pad.name());

        let mut state = self.state.lock().unwrap();
        let (i, stream) = match state
            .pads
            .iter()
            .enumerate()
            .find(|(_i, p)| &p.source_pad == pad)
        {
            None => return,
            Some(v) => v,
        };

        let ghost_pad = stream.ghost_pad.clone();
        state.pads.remove(i);
        drop(state);

        ghost_pad.set_active(false).unwrap();
        let _ = ghost_pad.set_target(None::<&gst::Pad>);
        let _ = self.obj().remove_pad(&ghost_pad);
    }

    fn handle_source_no_more_pads(&self) {
        gst::debug!(CAT, imp: self, "Source signalled no-more-pads");

        let state = self.state.lock().unwrap();
        let collection = gst::StreamCollection::builder(None)
            .streams(state.pads.iter().map(|p| p.stream.clone()))
            .build();
        drop(state);

        self.obj().no_more_pads();

        let _ = self.obj().post_message(
            gst::message::StreamsSelected::builder(&collection)
                .src(&*self.obj())
                .build(),
        );
    }

    fn stop(&self) {
        gst::debug!(CAT, imp: self, "Stopping");

        let mut state = self.state.lock().unwrap();
        let source = self.source.get().unwrap();
        if let Some(id) = state.pad_added_sig_id.take() {
            source.disconnect(id)
        }

        if let Some(id) = state.pad_removed_sig_id.take() {
            source.disconnect(id)
        }

        if let Some(id) = state.no_more_pads_sig_id.take() {
            source.disconnect(id)
        }

        let pads = mem::take(&mut state.pads);
        state.num_audio = 0;
        state.num_video = 0;
        drop(state);

        for pad in pads {
            let _ = pad.ghost_pad.set_target(None::<&gst::Pad>);
            let _ = self.obj().remove_pad(&pad.ghost_pad);
        }
    }
}
