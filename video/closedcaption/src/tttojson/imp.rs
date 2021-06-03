// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
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
use gst::gst_log;
use gst::prelude::*;
use gst::subclass::prelude::*;

use once_cell::sync::Lazy;

use std::sync::Mutex;

use crate::ttutils::{Cea608Mode, Chunk, Line, Lines, TextStyle};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "tttojson",
        gst::DebugColorFlags::empty(),
        Some("Timed Text to JSON"),
    )
});

const DEFAULT_MODE: Cea608Mode = Cea608Mode::RollUp2;

#[derive(Debug, Clone)]
struct Settings {
    mode: Cea608Mode,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { mode: DEFAULT_MODE }
    }
}

pub struct TtToJson {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
}

impl TtToJson {
    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        element: &super::TtToJson,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let pts = buffer.pts();
        let duration = buffer.duration();

        let buffer = buffer.into_mapped_buffer_readable().map_err(|_| {
            gst::element_error!(
                element,
                gst::ResourceError::Read,
                ["Failed to map buffer readable"]
            );

            gst::FlowError::Error
        })?;

        let text = std::str::from_utf8(buffer.as_slice()).map_err(|err| {
            gst::element_error!(
                element,
                gst::ResourceError::Read,
                ["Failed to map decode as utf8: {}", err]
            );

            gst::FlowError::Error
        })?;

        let mode = self.settings.lock().unwrap().mode;

        let mut lines = Lines {
            lines: Vec::new(),
            mode: Some(mode),
            clear: Some(false),
        };

        for phrase in text.lines() {
            lines.lines.push(Line {
                carriage_return: Some(true),
                column: Some(0),
                row: Some(13),
                chunks: vec![Chunk {
                    // Default CEA 608 styling
                    style: TextStyle::White,
                    underline: false,
                    text: phrase.to_string(),
                }],
            });
        }

        let json = serde_json::to_string(&lines).map_err(|err| {
            gst::element_error!(
                element,
                gst::ResourceError::Write,
                ["Failed to serialize as json {}", err]
            );

            gst::FlowError::Error
        })?;

        let mut buf = gst::Buffer::from_mut_slice(json.into_bytes());
        {
            let buf_mut = buf.get_mut().unwrap();
            buf_mut.set_pts(pts);
            buf_mut.set_duration(duration);
        }

        self.srcpad.push(buf)
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::TtToJson, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Caps(_) => {
                // We send our own caps downstream
                let caps = gst::Caps::builder("application/x-json")
                    .field("format", &"cea608")
                    .build();
                self.srcpad.push_event(gst::event::Caps::new(&caps))
            }
            EventView::Eos(_) => pad.event_default(Some(element), event),
            _ => pad.event_default(Some(element), event),
        }
    }
}

impl ElementImpl for TtToJson {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Timed text to JSON encoder",
                "Encoder/ClosedCaption",
                "Encodes Timed Text to JSON",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("text/x-raw")
                .field("format", &"utf8")
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst::Caps::builder("application/x-json").build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TtToJson {
    const NAME: &'static str = "RsTtToJson";
    type Type = super::TtToJson;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                TtToJson::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |enc, element| enc.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                TtToJson::catch_panic_pad_function(
                    parent,
                    || false,
                    |enc, element| enc.sink_event(pad, element, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src")).build();

        Self {
            srcpad,
            sinkpad,
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for TtToJson {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpec::new_enum(
                "mode",
                "Mode",
                "Which mode to operate in",
                Cea608Mode::static_type(),
                DEFAULT_MODE as i32,
                glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
            )]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "mode" => {
                let mut settings = self.settings.lock().unwrap();
                settings.mode = value.get::<Cea608Mode>().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "mode" => {
                let settings = self.settings.lock().unwrap();
                settings.mode.to_value()
            }
            _ => unimplemented!(),
        }
    }
}
