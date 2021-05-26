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
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_log, gst_trace};

use once_cell::sync::Lazy;

use std::sync::Mutex;

use serde::Serialize;

#[derive(Serialize, Debug)]
enum Line<'a> {
    Header {
        format: String,
    },
    Buffer {
        pts: Option<gst::ClockTime>,
        duration: Option<gst::ClockTime>,
        #[serde(borrow)]
        data: &'a serde_json::value::RawValue,
    },
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "jsongstenc",
        gst::DebugColorFlags::empty(),
        Some("GStreamer JSON Encoder Element"),
    )
});

#[derive(Debug)]
struct State {
    start_ts: Option<gst::ClockTime>,
    end_ts: Option<gst::ClockTime>,
    current_line: String,
    format: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            start_ts: None,
            end_ts: None,
            current_line: "".to_string(),
            format: None,
        }
    }
}

pub struct JsonGstEnc {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

impl JsonGstEnc {
    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        element: &super::JsonGstEnc,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let pts = buffer.pts();
        let duration = buffer.duration();

        let mut state = self.state.lock().unwrap();

        if let Some(format) = &state.format {
            let line = Line::Header {
                format: format.to_string(),
            };

            let mut json = serde_json::to_string(&line).map_err(|err| {
                gst::element_error!(
                    element,
                    gst::ResourceError::Write,
                    ["Failed to serialize as json {}", err]
                );

                gst::FlowError::Error
            })?;

            json.push('\n');

            let mut buf = gst::Buffer::from_mut_slice(json.into_bytes());
            {
                let buf_mut = buf.get_mut().unwrap();
                buf_mut.set_pts(pts);
            }

            state.format = None;
            drop(state);

            self.srcpad.push(buf)?;
        } else {
            drop(state);
        }

        let map = buffer.map_readable().map_err(|_| {
            gst::element_error!(
                element,
                gst::ResourceError::Read,
                ["Failed to map buffer readable"]
            );

            gst::FlowError::Error
        })?;

        let text = std::str::from_utf8(map.as_slice()).map_err(|err| {
            gst::element_error!(
                element,
                gst::ResourceError::Read,
                ["Failed to map decode as utf8: {}", err]
            );

            gst::FlowError::Error
        })?;

        let data: &serde_json::value::RawValue = serde_json::from_str(text).map_err(|err| {
            gst::element_error!(
                element,
                gst::ResourceError::Read,
                ["Failed to parse input as json: {}", err]
            );

            gst::FlowError::Error
        })?;

        let line = Line::Buffer {
            pts,
            duration,
            data,
        };

        let mut json = serde_json::to_string(&line).map_err(|err| {
            gst::element_error!(
                element,
                gst::ResourceError::Write,
                ["Failed to serialize as json {}", err]
            );

            gst::FlowError::Error
        })?;

        json.push('\n');

        let mut buf = gst::Buffer::from_mut_slice(json.into_bytes());
        {
            let buf_mut = buf.get_mut().unwrap();
            buf_mut.set_pts(pts);
            buf_mut.set_duration(duration);
        }

        self.srcpad.push(buf)
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::JsonGstEnc, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Caps(e) => {
                {
                    let mut state = self.state.lock().unwrap();
                    let caps = e.caps();
                    let s = caps.structure(0).unwrap();
                    state.format = match s.get::<Option<String>>("format") {
                        Err(_) => None,
                        Ok(format) => format,
                    };
                }

                // We send our own caps downstream
                let caps = gst::Caps::builder("application/x-json").build();
                self.srcpad.push_event(gst::event::Caps::new(&caps))
            }
            EventView::Eos(_) => pad.event_default(Some(element), event),
            _ => pad.event_default(Some(element), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for JsonGstEnc {
    const NAME: &'static str = "RsJsonGstEnc";
    type Type = super::JsonGstEnc;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                JsonGstEnc::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |enc, element| enc.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                JsonGstEnc::catch_panic_pad_function(
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
            state: Mutex::new(State::default()),
        }
    }
}

impl ObjectImpl for JsonGstEnc {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for JsonGstEnc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "GStreamer buffers to JSON",
                "Encoder/JSON",
                "Wraps buffers containing any valid top-level JSON structures \
            into higher level JSON objects, and outputs those as ndjson",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("application/x-json").build();
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

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused | gst::StateChange::PausedToReady => {
                // Reset the whole state
                let mut state = self.state.lock().unwrap();
                *state = State::default();
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }
}
