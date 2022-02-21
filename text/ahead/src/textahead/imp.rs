// Copyright (C) 2021 Guillaume Desmottes <guillaume@desmottes.be>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::sync::{Mutex, MutexGuard};

use once_cell::sync::Lazy;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "textahead",
        gst::DebugColorFlags::empty(),
        Some("textahead debug category"),
    )
});

struct Settings {
    n_ahead: u32,
    separator: String,
    current_attributes: String,
    ahead_attributes: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            n_ahead: 1,
            separator: "\n".to_string(),
            current_attributes: "size=\"larger\"".to_string(),
            ahead_attributes: "size=\"smaller\"".to_string(),
        }
    }
}

struct Input {
    text: String,
    pts: Option<gst::ClockTime>,
    duration: Option<gst::ClockTime>,
}

#[derive(Default)]
struct State {
    pending: Vec<Input>,
    done: bool,
}

pub struct TextAhead {
    sink_pad: gst::Pad,
    src_pad: gst::Pad,

    state: Mutex<State>,
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for TextAhead {
    const NAME: &'static str = "GstTextAhead";
    type Type = super::TextAhead;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sink_pad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                TextAhead::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp, element| imp.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                TextAhead::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp, element| imp.sink_event(pad, element, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let src_pad = gst::Pad::builder_with_template(&templ, Some("src")).build();

        Self {
            sink_pad,
            src_pad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for TextAhead {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            let default = Settings::default();

            vec![
                glib::ParamSpecUInt::new(
                    "n-ahead",
                    "n-ahead",
                    "The number of ahead text buffers to display along with the current one",
                    0,
                    u32::MAX,
                    default.n_ahead,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecString::new(
                    "separator",
                    "Separator",
                    "Text inserted between each text buffers",
                    Some(&default.separator),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                // See https://developer.gimp.org/api/2.0/pango/PangoMarkupFormat.html for pango attributes
                glib::ParamSpecString::new(
                    "current-attributes",
                    "Current attributes",
                    "Pango span attributes to set on the text from the current buffer",
                    Some(&default.current_attributes),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpecString::new(
                    "ahead-attributes",
                    "Ahead attributes",
                    "Pango span attributes to set on the ahead text",
                    Some(&default.ahead_attributes),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "n-ahead" => {
                settings.n_ahead = value.get().expect("type checked upstream");
            }
            "separator" => {
                settings.separator = value.get().expect("type checked upstream");
            }
            "current-attributes" => {
                settings.current_attributes = value.get().expect("type checked upstream");
            }
            "ahead-attributes" => {
                settings.ahead_attributes = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "n-ahead" => settings.n_ahead.to_value(),
            "separator" => settings.separator.to_value(),
            "current-attributes" => settings.current_attributes.to_value(),
            "ahead-attributes" => settings.ahead_attributes.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sink_pad).unwrap();
        obj.add_pad(&self.src_pad).unwrap();
    }
}

impl GstObjectImpl for TextAhead {}

impl ElementImpl for TextAhead {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Text Ahead",
                "Text/Filter",
                "Display upcoming text buffers ahead",
                "Guillaume Desmottes <guillaume@desmottes.be>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_caps = gst::Caps::builder("text/x-raw")
                .field("format", gst::List::new(["utf8", "pango-markup"]))
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder("text/x-raw")
                .field("format", "pango-markup")
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let res = self.parent_change_state(element, transition);

        match transition {
            gst::StateChange::ReadyToPaused => *self.state.lock().unwrap() = State::default(),
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                state.done = true;
            }
            _ => {}
        }

        res
    }
}

impl TextAhead {
    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        element: &super::TextAhead,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let pts = buffer.pts();
        let duration = buffer.duration();

        let buffer = buffer
            .into_mapped_buffer_readable()
            .map_err(|_| gst::FlowError::Error)?;
        let text =
            String::from_utf8(Vec::from(buffer.as_slice())).map_err(|_| gst::FlowError::Error)?;

        // queue buffer
        let mut state = self.state.lock().unwrap();

        gst::log!(CAT, obj: element, "input {:?}: {}", pts, text);

        state.pending.push(Input {
            text,
            pts,
            duration,
        });

        let n_ahead = {
            let settings = self.settings.lock().unwrap();
            settings.n_ahead as usize
        };

        // then check if we can output
        // FIXME: this won't work on live pipelines as we can't really report latency
        if state.pending.len() > n_ahead {
            self.push_pending(element, &mut state)
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::TextAhead, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::Eos(_) => {
                let mut state = self.state.lock().unwrap();

                gst::debug!(CAT, obj: element, "eos");

                while !state.pending.is_empty() {
                    let _ = self.push_pending(element, &mut state);
                }
                pad.event_default(Some(element), event)
            }
            gst::EventView::Caps(_caps) => {
                // set caps on src pad
                let templ = element.class().pad_template("src").unwrap();
                let _ = self
                    .src_pad
                    .push_event(gst::event::Caps::new(&templ.caps()));
                true
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    /// push first pending buffer as current and all the other ones as ahead text
    fn push_pending(
        &self,
        element: &super::TextAhead,
        state: &mut MutexGuard<State>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if state.done {
            return Err(gst::FlowError::Flushing);
        }
        let settings = self.settings.lock().unwrap();

        let first = state.pending.remove(0);
        let mut text = if settings.current_attributes.is_empty() {
            first.text
        } else {
            format!(
                "<span {}>{}</span>",
                settings.current_attributes, first.text
            )
        };

        for input in state.pending.iter() {
            if !settings.separator.is_empty() {
                text.push_str(&settings.separator);
            }

            if settings.ahead_attributes.is_empty() {
                text.push_str(&input.text);
            } else {
                text.push_str(&format!(
                    "<span {}>{}</span>",
                    settings.ahead_attributes, input.text
                ));
            }
        }

        gst::log!(CAT, obj: element, "output {:?}: {}", first.pts, text);

        let mut output = gst::Buffer::from_mut_slice(text.into_bytes());
        {
            let output = output.get_mut().unwrap();

            output.set_pts(first.pts);
            output.set_duration(first.duration);
        }

        self.src_pad.push(output)
    }
}
