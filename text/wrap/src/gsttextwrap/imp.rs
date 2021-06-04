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

use glib::translate::{from_glib, IntoGlib};
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info, gst_log};

use std::default::Default;
use std::fs::File;
use std::io;
use std::mem;
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
const DEFAULT_ACCUMULATE: i64 = -1;

#[derive(Debug, Clone)]
struct Settings {
    dictionary: Option<String>,
    columns: u32,
    lines: u32,
    accumulate_time: Option<gst::ClockTime>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dictionary: DEFAULT_DICTIONARY,
            columns: DEFAULT_COLUMNS, /* CEA 608 max columns */
            lines: DEFAULT_LINES,
            accumulate_time: None,
        }
    }
}

struct State {
    options: Option<textwrap::Options<'static, Box<dyn textwrap::WordSplitter + Send>>>,

    current_text: String,
    start_ts: Option<gst::ClockTime>,
    end_ts: Option<gst::ClockTime>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            options: None,

            current_text: "".to_string(),
            start_ts: None,
            end_ts: None,
        }
    }
}

pub struct TextWrap {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

fn is_punctuation(word: &str) -> bool {
    word == "." || word == "," || word == "?" || word == "!" || word == ";" || word == ":"
}

impl TextWrap {
    fn update_wrapper(&self, element: &super::TextWrap) {
        let settings = self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();

        if state.options.is_some() {
            return;
        }

        state.options = if let Some(dictionary) = &settings.dictionary {
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

            Some(textwrap::Options::with_splitter(
                settings.columns as usize,
                Box::new(standard),
            ))
        } else {
            Some(textwrap::Options::with_splitter(
                settings.columns as usize,
                Box::new(textwrap::NoHyphenation),
            ))
        };
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        element: &super::TextWrap,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.update_wrapper(element);

        let mut pts = buffer.pts().ok_or_else(|| {
            gst_error!(CAT, obj: element, "Need timestamped buffers");
            gst::FlowError::Error
        })?;

        let duration = buffer.duration().ok_or_else(|| {
            gst_error!(CAT, obj: element, "Need buffers with duration");
            gst::FlowError::Error
        })?;

        let data = buffer.map_readable().map_err(|_| {
            gst_error!(CAT, obj: element, "Can't map buffer readable");
            gst::FlowError::Error
        })?;

        let data = std::str::from_utf8(&data).map_err(|err| {
            gst_error!(CAT, obj: element, "Can't decode utf8: {}", err);

            gst::FlowError::Error
        })?;

        let accumulate_time = self.settings.lock().unwrap().accumulate_time;
        let mut state = self.state.lock().unwrap();

        if accumulate_time.is_some() {
            let mut bufferlist = gst::BufferList::new();
            let n_lines = std::cmp::max(self.settings.lock().unwrap().lines, 1);

            if state
                .start_ts
                .zip(accumulate_time)
                .map_or(false, |(start_ts, accumulate_time)| {
                    start_ts + accumulate_time < pts
                })
            {
                let mut buf =
                    gst::Buffer::from_mut_slice(mem::take(&mut state.current_text).into_bytes());
                {
                    let buf_mut = buf.get_mut().unwrap();
                    buf_mut.set_pts(state.start_ts);
                    buf_mut.set_duration(
                        state
                            .end_ts
                            .zip(state.start_ts)
                            .and_then(|(end_ts, start_ts)| end_ts.checked_sub(start_ts)),
                    );
                }
                bufferlist.get_mut().unwrap().add(buf);

                state.start_ts = None;
                state.end_ts = None;
            }

            let duration_per_word: gst::ClockTime =
                duration / data.split_whitespace().count() as u64;

            if state.start_ts.is_none() {
                state.start_ts = buffer.pts();
            }

            state.end_ts = buffer.pts();

            let words = data.split_whitespace();
            let mut current_text = state.current_text.to_string();

            for word in words {
                if !current_text.is_empty() && !is_punctuation(word) {
                    current_text.push(' ');
                }
                current_text.push_str(word);

                let options = state
                    .options
                    .as_ref()
                    .expect("We should have a wrapper by now");

                let lines = textwrap::wrap(&current_text, options);
                let mut chunks = lines.chunks(n_lines as usize).peekable();
                let mut trailing = "".to_string();

                while let Some(chunk) = chunks.next() {
                    if chunks.peek().is_none() {
                        trailing = chunk
                            .iter()
                            .map(|l| l.to_string())
                            .collect::<Vec<String>>()
                            .join("\n");
                    } else {
                        let duration = state
                            .end_ts
                            .zip(state.start_ts)
                            .and_then(|(end_ts, start_ts)| end_ts.checked_sub(start_ts));
                        let contents = chunk
                            .iter()
                            .map(|l| l.to_string())
                            .collect::<Vec<String>>()
                            .join("\n");
                        gst_info!(
                            CAT,
                            obj: element,
                            "Outputting contents {}, ts: {}, duration: {}",
                            contents.to_string(),
                            state.start_ts.display(),
                            duration.display(),
                        );
                        let mut buf = gst::Buffer::from_mut_slice(contents.into_bytes());
                        {
                            let buf_mut = buf.get_mut().unwrap();
                            buf_mut.set_pts(state.start_ts);
                            buf_mut.set_duration(duration);
                        }
                        bufferlist.get_mut().unwrap().add(buf);
                        state.start_ts = state.end_ts;
                    }
                }

                current_text = trailing;
                state.end_ts = state.end_ts.map(|end_ts| end_ts + duration_per_word);
            }

            state.current_text = current_text;

            if state.current_text.is_empty() {
                state.start_ts = None;
                state.end_ts = None;
            }

            drop(state);

            if bufferlist.is_empty() {
                Ok(gst::FlowSuccess::Ok)
            } else {
                self.srcpad.push_list(bufferlist)
            }
        } else {
            let lines = self.settings.lock().unwrap().lines;

            let data = {
                let options = state
                    .options
                    .as_ref()
                    .expect("We should have a wrapper by now");
                textwrap::fill(data, options)
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
                    gst_info!(CAT, "Pushing lines {}", data);
                    let mut buf = gst::Buffer::from_mut_slice(data.into_bytes());

                    {
                        let buf = buf.get_mut().unwrap();

                        buf.set_pts(pts);
                        buf.set_duration(duration);
                        pts += duration;
                    }

                    bufferlist.get_mut().unwrap().add(buf);
                }

                drop(state);

                self.srcpad.push_list(bufferlist)
            } else {
                let mut buf = gst::Buffer::from_mut_slice(data.into_bytes());

                {
                    let buf = buf.get_mut().unwrap();

                    buf.set_pts(pts);
                    buf.set_duration(duration);
                }

                drop(state);

                self.srcpad.push(buf)
            }
        }
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::TextWrap, event: gst::Event) -> bool {
        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        use gst::EventView;

        match event.view() {
            EventView::Gap(_) => {
                let state = self.state.lock().unwrap();
                /* We are currently accumulating text, no need to forward the gap */
                if state.start_ts.is_some() {
                    true
                } else {
                    pad.event_default(Some(element), event)
                }
            }
            EventView::FlushStart(_) => {
                let mut state = self.state.lock().unwrap();
                let options = state.options.take();
                *state = State::default();
                state.options = options;
                drop(state);
                pad.event_default(Some(element), event)
            }
            EventView::Eos(_) => {
                let mut state = self.state.lock().unwrap();
                if !state.current_text.is_empty() {
                    let mut buf = gst::Buffer::from_mut_slice(
                        mem::take(&mut state.current_text).into_bytes(),
                    );
                    {
                        let buf_mut = buf.get_mut().unwrap();
                        buf_mut.set_pts(state.start_ts);
                        buf_mut.set_duration(
                            state
                                .end_ts
                                .zip(state.start_ts)
                                .and_then(|(end_ts, start_ts)| end_ts.checked_sub(start_ts)),
                        );
                    }

                    state.start_ts = None;
                    state.end_ts = None;

                    drop(state);
                    let _ = self.srcpad.push(buf);
                } else {
                    drop(state);
                }
                pad.event_default(Some(element), event)
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        element: &super::TextWrap,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = self.sinkpad.peer_query(&mut peer_query);

                if ret {
                    let (live, min, _) = peer_query.result();
                    let our_latency: gst::ClockTime = self
                        .settings
                        .lock()
                        .unwrap()
                        .accumulate_time
                        .unwrap_or(gst::ClockTime::ZERO);
                    gst_info!(
                        CAT,
                        obj: element,
                        "Reporting our latency {} + {}",
                        our_latency,
                        min
                    );
                    q.set(live, our_latency + min, gst::ClockTime::NONE);
                }
                ret
            }
            _ => pad.query_default(Some(element), query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TextWrap {
    const NAME: &'static str = "RsTextWrap";
    type Type = super::TextWrap;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                TextWrap::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |textwrap, element| textwrap.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                TextWrap::catch_panic_pad_function(
                    parent,
                    || false,
                    |textwrap, element| textwrap.sink_event(pad, element, event),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .query_function(|pad, parent, query| {
                TextWrap::catch_panic_pad_function(
                    parent,
                    || false,
                    |textwrap, element| textwrap.src_query(pad, element, query),
                )
            })
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
}

impl ObjectImpl for TextWrap {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_string(
                    "dictionary",
                    "Dictionary",
                    "Path to a dictionary to load at runtime to perform hyphenation, see \
                        <https://docs.rs/crate/hyphenation/0.7.1> for more information",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_uint(
                    "columns",
                    "Columns",
                    "Maximum number of columns for any given line",
                    1,
                    std::u32::MAX,
                    DEFAULT_COLUMNS,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_uint(
                    "lines",
                    "Lines",
                    "Split input buffer into output buffers with max lines (0=do not split)",
                    0,
                    std::u32::MAX,
                    DEFAULT_LINES,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_int64(
                    "accumulate-time",
                    "accumulate-time",
                    "Cut-off time for input text accumulation (-1=do not accumulate)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_ACCUMULATE,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
            ]
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
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "dictionary" => {
                let mut settings = self.settings.lock().unwrap();
                let mut state = self.state.lock().unwrap();
                settings.dictionary = value.get().expect("type checked upstream");
                state.options = None;
            }
            "columns" => {
                let mut settings = self.settings.lock().unwrap();
                let mut state = self.state.lock().unwrap();
                settings.columns = value.get().expect("type checked upstream");
                state.options = None;
            }
            "lines" => {
                let mut settings = self.settings.lock().unwrap();
                settings.lines = value.get().expect("type checked upstream");
            }
            "accumulate-time" => {
                let mut settings = self.settings.lock().unwrap();
                let old_accumulate_time = settings.accumulate_time;
                settings.accumulate_time =
                    unsafe { from_glib(value.get::<i64>().expect("type checked upstream")) };
                if settings.accumulate_time != old_accumulate_time {
                    gst_debug!(
                        CAT,
                        obj: obj,
                        "Accumulate time changed: {}",
                        settings.accumulate_time.display(),
                    );
                    drop(settings);
                    let _ = obj.post_message(gst::message::Latency::builder().src(obj).build());
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "dictionary" => {
                let settings = self.settings.lock().unwrap();
                settings.dictionary.to_value()
            }
            "columns" => {
                let settings = self.settings.lock().unwrap();
                settings.columns.to_value()
            }
            "lines" => {
                let settings = self.settings.lock().unwrap();
                settings.lines.to_value()
            }
            "accumulate-time" => {
                let settings = self.settings.lock().unwrap();
                settings.accumulate_time.into_glib().to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for TextWrap {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Text Wrapper",
                "Text/Filter",
                "Breaks text into fixed-size lines, with optional hyphenation",
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

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_info!(CAT, obj: element, "Changing state {:?}", transition);

        if let gst::StateChange::PausedToReady = transition {
            let mut state = self.state.lock().unwrap();
            *state = State::default();
        }

        let success = self.parent_change_state(element, transition)?;

        Ok(success)
    }
}
