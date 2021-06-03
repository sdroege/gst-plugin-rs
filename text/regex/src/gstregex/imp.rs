// Copyright (C) 2021 Mathieu Duponchelle <mathieu@centricular.com>
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
use gst::gst_error;
use gst::prelude::*;
use gst::subclass::prelude::*;

use regex::Regex;
use std::default::Default;
use std::sync::Mutex;

use once_cell::sync::Lazy;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "regex",
        gst::DebugColorFlags::empty(),
        Some("Regular Expression element"),
    )
});

enum Operation {
    ReplaceAll(String),
}

struct Command {
    pattern: String,
    regex: Regex,
    operation: Operation,
}

struct State {
    commands: Vec<Command>,
}

impl Default for State {
    fn default() -> Self {
        Self { commands: vec![] }
    }
}

pub struct RegEx {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

impl RegEx {
    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        element: &super::RegEx,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let data = buffer.map_readable().map_err(|_| {
            gst_error!(CAT, obj: element, "Can't map buffer readable");
            gst::element_error!(element, gst::CoreError::Failed, ["Failed to map buffer"]);
            gst::FlowError::Error
        })?;

        let mut data = std::str::from_utf8(&data)
            .map_err(|err| {
                gst_error!(CAT, obj: element, "Can't decode utf8: {}", err);
                gst::element_error!(
                    element,
                    gst::StreamError::Decode,
                    ["Failed to decode utf8: {}", err]
                );

                gst::FlowError::Error
            })?
            .to_string();

        let state = self.state.lock().unwrap();

        for command in &state.commands {
            match &command.operation {
                Operation::ReplaceAll(replacement) => {
                    data = command
                        .regex
                        .replace_all(&data, replacement.as_str())
                        .to_string();
                }
            }
        }

        let mut outbuf = gst::Buffer::from_mut_slice(data.into_bytes());

        {
            let outbuf_mut = outbuf.get_mut().unwrap();
            let _ = buffer.copy_into(
                outbuf_mut,
                gst::BufferCopyFlags::FLAGS
                    | gst::BufferCopyFlags::TIMESTAMPS
                    | gst::BufferCopyFlags::META,
                0,
                None,
            );
        }

        self.srcpad.push(outbuf)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RegEx {
    const NAME: &'static str = "RsRegEx";
    type Type = super::RegEx;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                RegEx::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |regex, element| regex.sink_chain(pad, element, buffer),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        let state = Mutex::new(State::default());

        Self {
            srcpad,
            sinkpad,
            state,
        }
    }
}

impl ObjectImpl for RegEx {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpec::new_array(
                "commands",
                "Commands",
                "A set of commands to apply on input text",
                &glib::ParamSpec::new_boxed(
                    "command",
                    "Command",
                    "A command to apply on input text",
                    gst::Structure::static_type(),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
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
            "commands" => {
                let mut state = self.state.lock().unwrap();
                state.commands = vec![];
                let commands: gst::Array = value.get().expect("type checked upstream");
                for command in commands.as_slice() {
                    let s = match command
                        .get::<Option<gst::Structure>>()
                        .expect("type checked upstream")
                    {
                        Some(s) => s,
                        None => {
                            continue;
                        }
                    };
                    let operation = s.name();

                    let pattern = match s.get::<Option<String>>("pattern") {
                        Ok(Some(pattern)) => pattern,
                        Ok(None) | Err(_) => {
                            gst_error!(CAT, "All commands require a pattern field as a string");
                            continue;
                        }
                    };

                    let regex = match Regex::new(&pattern) {
                        Ok(regex) => regex,
                        Err(err) => {
                            gst_error!(CAT, "Failed to compile regex: {:?}", err);
                            continue;
                        }
                    };

                    match operation {
                        "replace-all" | "replace_all" => {
                            let replacement = match s.get::<Option<String>>("replacement") {
                                Ok(Some(pattern)) => pattern,
                                Ok(None) | Err(_) => {
                                    gst_error!(
                                        CAT,
                                        "Replace operations require a replacement field as a string"
                                    );
                                    continue;
                                }
                            };
                            state.commands.push(Command {
                                pattern,
                                regex,
                                operation: Operation::ReplaceAll(replacement),
                            });
                        }
                        val => {
                            gst_error!(CAT, "Unknown operation {}", val);
                        }
                    }
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "commands" => {
                let state = self.state.lock().unwrap();
                let mut commands = vec![];
                for command in &state.commands {
                    match command.operation {
                        Operation::ReplaceAll(ref replacement) => {
                            commands.push(
                                gst::Structure::new(
                                    &"replace-all",
                                    &[("pattern", &command.pattern), ("replacement", &replacement)],
                                )
                                .to_send_value(),
                            );
                        }
                    }
                }
                gst::Array::from_owned(commands).to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for RegEx {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Regular Expression processor",
                "Text/Filter",
                "Applies operations according to regular expressions",
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
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}
