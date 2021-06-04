// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_log, gst_trace};

use crate::caption_frame::{CaptionFrame, Status};
use atomic_refcell::AtomicRefCell;

use once_cell::sync::Lazy;

#[derive(Copy, Clone, Debug)]
enum Format {
    Srt,
    Vtt,
    Raw,
}

struct State {
    format: Option<Format>,
    wrote_header: bool,
    caption_frame: CaptionFrame,
    previous_text: Option<(gst::ClockTime, String)>,
    index: u64,
}

impl Default for State {
    fn default() -> Self {
        State {
            format: None,
            wrote_header: false,
            caption_frame: CaptionFrame::default(),
            previous_text: None,
            index: 1,
        }
    }
}

pub struct Cea608ToTt {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,

    state: AtomicRefCell<State>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "cea608tott",
        gst::DebugColorFlags::empty(),
        Some("CEA-608 to TT Element"),
    )
});

impl Cea608ToTt {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        _element: &super::Cea608ToTt,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.borrow_mut();
        let format = match state.format {
            Some(format) => format,
            None => {
                gst_error!(CAT, obj: pad, "Not negotiated yet");
                return Err(gst::FlowError::NotNegotiated);
            }
        };

        let buffer_pts = buffer.pts().ok_or_else(|| {
            gst_error!(CAT, obj: pad, "Require timestamped buffers");
            gst::FlowError::Error
        })?;

        let pts = (buffer_pts.nseconds() as f64) / 1_000_000_000.0;

        let data = buffer.map_readable().map_err(|_| {
            gst_error!(CAT, obj: pad, "Can't map buffer readable");

            gst::FlowError::Error
        })?;

        if data.len() < 2 {
            gst_error!(CAT, obj: pad, "Invalid closed caption packet size");

            return Ok(gst::FlowSuccess::Ok);
        }

        let previous_text = match state
            .caption_frame
            .decode((data[0] as u16) << 8 | data[1] as u16, pts)
        {
            Ok(Status::Ok) => return Ok(gst::FlowSuccess::Ok),
            Err(_) => {
                gst_error!(CAT, obj: pad, "Failed to decode closed caption packet");
                return Ok(gst::FlowSuccess::Ok);
            }
            Ok(Status::Clear) => {
                gst_debug!(CAT, obj: pad, "Clearing previous closed caption packet");
                state.previous_text.take()
            }
            Ok(Status::Ready) => {
                gst_debug!(CAT, obj: pad, "Have new closed caption packet");
                let text = match state.caption_frame.to_text(false) {
                    Ok(text) => text,
                    Err(_) => {
                        gst_error!(CAT, obj: pad, "Failed to convert caption frame to text");
                        return Ok(gst::FlowSuccess::Ok);
                    }
                };

                state.previous_text.replace((buffer_pts, text))
            }
        };

        let previous_text = match previous_text {
            Some(previous_text) => previous_text,
            None => {
                gst_debug!(CAT, obj: pad, "Have no previous text");
                return Ok(gst::FlowSuccess::Ok);
            }
        };

        let duration = buffer_pts.saturating_sub(previous_text.0);

        let (timestamp, text) = previous_text;

        let header_buffer = if !state.wrote_header {
            state.wrote_header = true;

            match format {
                Format::Vtt => Some(Self::create_vtt_header(timestamp)),
                Format::Srt | Format::Raw => None,
            }
        } else {
            None
        };

        let buffer = match format {
            Format::Vtt => Self::create_vtt_buffer(timestamp, duration, text),
            Format::Srt => Self::create_srt_buffer(timestamp, duration, state.index, text),
            Format::Raw => Self::create_raw_buffer(timestamp, duration, text),
        };
        state.index += 1;
        drop(state);

        if let Some(header_buffer) = header_buffer {
            self.srcpad.push(header_buffer)?;
        }

        self.srcpad.push(buffer)
    }

    fn create_vtt_header(timestamp: gst::ClockTime) -> gst::Buffer {
        use std::fmt::Write;

        let mut headers = String::new();
        writeln!(&mut headers, "WEBVTT\r").unwrap();
        writeln!(&mut headers, "\r").unwrap();

        let mut buffer = gst::Buffer::from_mut_slice(headers.into_bytes());
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(timestamp);
        }

        buffer
    }

    fn split_time(time: gst::ClockTime) -> (u64, u8, u8, u16) {
        let time = time.nseconds();

        let mut s = time / 1_000_000_000;
        let mut m = s / 60;
        let h = m / 60;
        s %= 60;
        m %= 60;
        let ns = time % 1_000_000_000;

        (h as u64, m as u8, s as u8, (ns / 1_000_000) as u16)
    }

    fn create_vtt_buffer(
        timestamp: gst::ClockTime,
        duration: gst::ClockTime,
        text: String,
    ) -> gst::Buffer {
        use std::fmt::Write;

        let mut data = String::new();

        let (h1, m1, s1, ms1) = Self::split_time(timestamp);
        let (h2, m2, s2, ms2) = Self::split_time(timestamp + duration);

        writeln!(
            &mut data,
            "{:02}:{:02}:{:02}.{:03} --> {:02}:{:02}:{:02}.{:03}\r",
            h1, m1, s1, ms1, h2, m2, s2, ms2
        )
        .unwrap();
        writeln!(&mut data, "{}\r", text).unwrap();
        writeln!(&mut data, "\r").unwrap();

        let mut buffer = gst::Buffer::from_mut_slice(data.into_bytes());
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(timestamp);
            buffer.set_duration(duration);
        }

        buffer
    }

    fn create_srt_buffer(
        timestamp: gst::ClockTime,
        duration: gst::ClockTime,
        index: u64,
        text: String,
    ) -> gst::Buffer {
        use std::fmt::Write;

        let mut data = String::new();

        let (h1, m1, s1, ms1) = Self::split_time(timestamp);
        let (h2, m2, s2, ms2) = Self::split_time(timestamp + duration);

        writeln!(&mut data, "{:02}\r", index).unwrap();
        writeln!(
            &mut data,
            "{}:{:02}:{:02},{:03} --> {:02}:{:02}:{:02},{:03}\r",
            h1, m1, s1, ms1, h2, m2, s2, ms2
        )
        .unwrap();
        writeln!(&mut data, "{}\r", text).unwrap();
        writeln!(&mut data, "\r").unwrap();

        let mut buffer = gst::Buffer::from_mut_slice(data.into_bytes());
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(timestamp);
            buffer.set_duration(duration);
        }

        buffer
    }

    fn create_raw_buffer(
        timestamp: gst::ClockTime,
        duration: gst::ClockTime,
        text: String,
    ) -> gst::Buffer {
        let mut buffer = gst::Buffer::from_mut_slice(text.into_bytes());
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(timestamp);
            buffer.set_duration(duration);
        }

        buffer
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::Cea608ToTt, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Caps(..) => {
                let mut state = self.state.borrow_mut();

                if state.format.is_some() {
                    return true;
                }

                let mut downstream_caps = match self.srcpad.allowed_caps() {
                    None => self.srcpad.pad_template_caps(),
                    Some(caps) => caps,
                };

                if downstream_caps.is_empty() {
                    gst_error!(CAT, obj: pad, "Empty downstream caps");
                    return false;
                }

                downstream_caps.fixate();

                gst_debug!(
                    CAT,
                    obj: pad,
                    "Negotiating for downstream caps {}",
                    downstream_caps
                );

                let s = downstream_caps.structure(0).unwrap();
                let new_caps = if s.name() == "application/x-subtitle-vtt" {
                    state.format = Some(Format::Vtt);
                    gst::Caps::builder("application/x-subtitle-vtt").build()
                } else if s.name() == "application/x-subtitle" {
                    state.format = Some(Format::Srt);
                    gst::Caps::builder("application/x-subtitle").build()
                } else if s.name() == "text/x-raw" {
                    state.format = Some(Format::Raw);
                    gst::Caps::builder("text/x-raw")
                        .field("format", &"utf8")
                        .build()
                } else {
                    unreachable!();
                };

                let new_event = gst::event::Caps::new(&new_caps);

                return self.srcpad.push_event(new_event);
            }
            EventView::FlushStop(..) => {
                let mut state = self.state.borrow_mut();
                state.caption_frame = CaptionFrame::default();
                state.previous_text = None;
            }
            EventView::Eos(..) => {
                let mut state = self.state.borrow_mut();
                if let Some((timestamp, text)) = state.previous_text.take() {
                    gst_debug!(CAT, obj: pad, "Outputting final text on EOS");

                    let format = state.format.unwrap();

                    let header_buffer = if !state.wrote_header {
                        state.wrote_header = true;

                        match format {
                            Format::Vtt => Some(Self::create_vtt_header(timestamp)),
                            Format::Srt | Format::Raw => None,
                        }
                    } else {
                        None
                    };

                    let buffer = match format {
                        Format::Vtt => {
                            Self::create_vtt_buffer(timestamp, gst::ClockTime::ZERO, text)
                        }
                        Format::Srt => Self::create_srt_buffer(
                            timestamp,
                            gst::ClockTime::ZERO,
                            state.index,
                            text,
                        ),
                        Format::Raw => {
                            Self::create_raw_buffer(timestamp, gst::ClockTime::ZERO, text)
                        }
                    };
                    state.index += 1;
                    drop(state);

                    if let Some(header_buffer) = header_buffer {
                        let _ = self.srcpad.push(header_buffer);
                    }

                    let _ = self.srcpad.push(buffer);
                }
            }
            _ => (),
        }

        pad.event_default(Some(element), event)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Cea608ToTt {
    const NAME: &'static str = "Cea608ToTt";
    type Type = super::Cea608ToTt;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                Cea608ToTt::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |this, element| this.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Cea608ToTt::catch_panic_pad_function(
                    parent,
                    || false,
                    |this, element| this.sink_event(pad, element, event),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            state: AtomicRefCell::new(State::default()),
        }
    }
}

impl ObjectImpl for Cea608ToTt {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for Cea608ToTt {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "CEA-608 to TT",
                "Generic",
                "Converts CEA-608 Closed Captions to SRT/VTT timed text",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let mut caps = gst::Caps::new_empty();
            {
                let caps = caps.get_mut().unwrap();

                // WebVTT
                let s = gst::Structure::builder("application/x-subtitle-vtt").build();
                caps.append_structure(s);

                // SRT
                let s = gst::Structure::builder("application/x-subtitle").build();
                caps.append_structure(s);

                // Raw timed text
                let s = gst::Structure::builder("text/x-raw")
                    .field("format", &"utf8")
                    .build();
                caps.append_structure(s);
            }

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst::Caps::builder("closedcaption/x-cea-608")
                .field("format", &"raw")
                .build();

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
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.borrow_mut();
                *state = State::default();
            }
            _ => (),
        }

        let ret = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                let mut state = self.state.borrow_mut();
                *state = State::default();
            }
            _ => (),
        }

        Ok(ret)
    }
}
