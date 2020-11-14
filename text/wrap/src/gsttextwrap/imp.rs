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

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::default::Default;
use std::fs::File;
use std::io;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use hyphenation::{Load, Standard};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "textwrap",
        gst::DebugColorFlags::empty(),
        Some("Text wrapper element"),
    )
});

const DEFAULT_DICTIONARY: Option<String> = None;
const DEFAULT_COLUMNS: u32 = 32; /* CEA 608 max columns */
const DEFAULT_LINES: u32 = 0;

static PROPERTIES: [subclass::Property; 3] = [
    subclass::Property("dictionary", |name| {
        glib::ParamSpec::string(
            name,
            "Dictionary",
            "Path to a dictionary to load at runtime to perform hyphenation, see \
                <https://docs.rs/crate/hyphenation/0.7.1> for more information",
            None,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("columns", |name| {
        glib::ParamSpec::uint(
            name,
            "Columns",
            "Maximum number of columns for any given line",
            1,
            std::u32::MAX,
            DEFAULT_COLUMNS,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("lines", |name| {
        glib::ParamSpec::uint(
            name,
            "Lines",
            "Split input buffer into output buffers with max lines (0=do not split)",
            0,
            std::u32::MAX,
            DEFAULT_LINES,
            glib::ParamFlags::READWRITE,
        )
    }),
];

#[derive(Debug, Clone)]
struct Settings {
    dictionary: Option<String>,
    columns: u32,
    lines: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dictionary: DEFAULT_DICTIONARY,
            columns: DEFAULT_COLUMNS, /* CEA 608 max columns */
            lines: DEFAULT_LINES,
        }
    }
}

#[allow(clippy::large_enum_variant)]
enum Wrapper {
    H(textwrap::Wrapper<'static, Standard>),
    N(textwrap::Wrapper<'static, textwrap::NoHyphenation>),
}

struct State {
    wrapper: Option<Wrapper>,
}

impl Wrapper {
    fn fill(&self, s: &str) -> String {
        match *self {
            Wrapper::H(ref w) => w.fill(s),
            Wrapper::N(ref w) => w.fill(s),
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self { wrapper: None }
    }
}

pub struct TextWrap {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl TextWrap {
    fn update_wrapper(&self, element: &super::TextWrap) {
        let settings = self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();

        if state.wrapper.is_some() {
            return;
        }

        state.wrapper = if let Some(dictionary) = &settings.dictionary {
            let dict_file = match File::open(dictionary) {
                Err(err) => {
                    gst_error!(CAT, obj: element, "Failed to open dictionary file: {}", err);
                    return;
                }
                Ok(dict_file) => dict_file,
            };

            let mut reader = io::BufReader::new(dict_file);
            let standard = match Standard::any_from_reader(&mut reader) {
                Err(err) => {
                    gst_error!(
                        CAT,
                        obj: element,
                        "Failed to load standard from file: {}",
                        err
                    );
                    return;
                }
                Ok(standard) => standard,
            };

            Some(Wrapper::H(textwrap::Wrapper::with_splitter(
                settings.columns as usize,
                standard,
            )))
        } else {
            Some(Wrapper::N(textwrap::Wrapper::with_splitter(
                settings.columns as usize,
                textwrap::NoHyphenation,
            )))
        };
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        element: &super::TextWrap,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.update_wrapper(element);

        let mut pts: gst::ClockTime = buffer
            .get_pts()
            .ok_or_else(|| {
                gst_error!(CAT, obj: element, "Need timestamped buffers");
                gst::FlowError::Error
            })?
            .into();

        let duration: gst::ClockTime = buffer
            .get_duration()
            .ok_or_else(|| {
                gst_error!(CAT, obj: element, "Need buffers with duration");
                gst::FlowError::Error
            })?
            .into();

        let data = buffer.map_readable().map_err(|_| {
            gst_error!(CAT, obj: element, "Can't map buffer readable");

            gst::FlowError::Error
        })?;

        let data = std::str::from_utf8(&data).map_err(|err| {
            gst_error!(CAT, obj: element, "Can't decode utf8: {}", err);

            gst::FlowError::Error
        })?;

        let lines = self.settings.lock().unwrap().lines;

        let data = {
            let state = self.state.lock().unwrap();
            let wrapper = state
                .wrapper
                .as_ref()
                .expect("We should have a wrapper by now");
            wrapper.fill(data)
        };

        // If the lines property was set, we want to split the result into buffers
        // of at most N lines. We compute the duration for each of those based on
        // the total number of words, and the number of words in each of the split-up
        // buffers.
        if lines > 0 {
            let mut bufferlist = gst::BufferList::new();
            let duration_per_word: gst::ClockTime =
                duration / data.split_whitespace().count() as u64;

            for chunk in data.lines().collect::<Vec<&str>>().chunks(lines as usize) {
                let data = chunk.join("\n");
                let duration: gst::ClockTime =
                    duration_per_word * data.split_whitespace().count() as u64;
                let mut buf = gst::Buffer::from_mut_slice(data.into_bytes());

                {
                    let buf = buf.get_mut().unwrap();

                    buf.set_pts(pts);
                    buf.set_duration(duration);
                    pts += duration;
                }

                bufferlist.get_mut().unwrap().add(buf);
            }

            self.srcpad.push_list(bufferlist)
        } else {
            let mut buf = gst::Buffer::from_mut_slice(data.into_bytes());

            {
                let buf = buf.get_mut().unwrap();

                buf.set_pts(pts);
                buf.set_duration(duration);
            }

            self.srcpad.push(buf)
        }
    }
}

impl ObjectSubclass for TextWrap {
    const NAME: &'static str = "RsTextWrap";
    type Type = super::TextWrap;
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                TextWrap::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |textwrap, element| textwrap.sink_chain(pad, element, buffer),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        let settings = Mutex::new(Settings::default());
        let state = Mutex::new(State::default());

        Self {
            srcpad,
            sinkpad,
            settings,
            state,
        }
    }

    fn class_init(klass: &mut Self::Class) {
        klass.set_metadata(
            "Text Wrapper",
            "Text/Filter",
            "Breaks text into fixed-size lines, with optional hyphenationz",
            "Mathieu Duponchelle <mathieu@centricular.com>",
        );

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
        klass.add_pad_template(src_pad_template);

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for TextWrap {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(&self, _obj: &Self::Type, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("dictionary", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let mut state = self.state.lock().unwrap();
                settings.dictionary = value.get().expect("type checked upstream");
                state.wrapper = None;
            }
            subclass::Property("columns", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let mut state = self.state.lock().unwrap();
                settings.columns = value.get_some().expect("type checked upstream");
                state.wrapper = None;
            }
            subclass::Property("lines", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.lines = value.get_some().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &Self::Type, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("dictionary", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.dictionary.to_value())
            }
            subclass::Property("columns", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.columns.to_value())
            }
            subclass::Property("lines", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.lines.to_value())
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for TextWrap {
    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_info!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                *state = State::default();
            }
            _ => (),
        }

        let success = self.parent_change_state(element, transition)?;

        Ok(success)
    }
}
