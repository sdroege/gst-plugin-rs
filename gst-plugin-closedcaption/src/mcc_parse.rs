// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
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

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_video::{self, ValidVideoTimeCode};

use std::sync::{Mutex, MutexGuard};

use crate::line_reader::LineReader;
use crate::mcc_parser::{MccLine, MccParser, TimeCode};

lazy_static! {
    static ref CAT: gst::DebugCategory = {
        gst::DebugCategory::new(
            "mccparse",
            gst::DebugColorFlags::empty(),
            "Mcc Parser Element",
        )
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Cea708Cdp,
    Cea608,
}

#[derive(Debug)]
struct State {
    reader: LineReader<gst::MappedBuffer<gst::buffer::Readable>>,
    parser: MccParser,
    format: Option<Format>,
    need_segment: bool,
    pending_events: Vec<gst::Event>,
    start_position: gst::ClockTime,
    last_position: gst::ClockTime,
    last_timecode: Option<gst_video::ValidVideoTimeCode>,
    timecode_rate: Option<(u8, bool)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            reader: LineReader::new(),
            parser: MccParser::new(),
            format: None,
            need_segment: true,
            pending_events: Vec::new(),
            start_position: gst::CLOCK_TIME_NONE,
            last_position: gst::CLOCK_TIME_NONE,
            last_timecode: None,
            timecode_rate: None,
        }
    }
}

impl State {
    fn get_line(
        &mut self,
        drain: bool,
    ) -> Result<
        Option<MccLine>,
        (
            &[u8],
            combine::easy::Errors<u8, &[u8], combine::stream::PointerOffset>,
        ),
    > {
        let line = match self.reader.get_line_with_drain(drain) {
            None => {
                return Ok(None);
            }
            Some(line) => line,
        };

        self.parser
            .parse_line(line)
            .map(Option::Some)
            .map_err(|err| (line, err))
    }

    fn handle_timecode(
        &mut self,
        element: &gst::Element,
        tc: TimeCode,
        framerate: gst::Fraction,
        drop_frame: bool,
    ) -> Result<ValidVideoTimeCode, gst::FlowError> {
        let timecode = gst_video::VideoTimeCode::new(
            framerate,
            None,
            if drop_frame {
                gst_video::VideoTimeCodeFlags::DROP_FRAME
            } else {
                gst_video::VideoTimeCodeFlags::empty()
            },
            tc.hours,
            tc.minutes,
            tc.seconds,
            tc.frames,
            0,
        );

        match timecode.try_into() {
            Ok(timecode) => Ok(timecode),
            Err(timecode) => {
                let last_timecode =
                    self.last_timecode
                        .as_ref()
                        .map(Clone::clone)
                        .ok_or_else(|| {
                            gst_element_error!(
                                element,
                                gst::StreamError::Decode,
                                ["Invalid first timecode {:?}", timecode]
                            );

                            gst::FlowError::Error
                        })?;

                gst_warning!(
                    CAT,
                    obj: element,
                    "Invalid timecode {:?}, using previous {:?}",
                    timecode,
                    last_timecode
                );

                Ok(last_timecode)
            }
        }
    }

    /// Calculate a timestamp from the timecode and make sure to
    /// not produce timestamps jumping backwards
    fn update_timestamp(
        &mut self,
        element: &gst::Element,
        timecode: &gst_video::ValidVideoTimeCode,
    ) {
        let nsecs = gst::ClockTime::from(timecode.nsec_since_daily_jam());
        if self.start_position.is_none() {
            self.start_position = nsecs;
        }

        let nsecs = if nsecs < self.start_position {
            gst_fixme!(
                CAT,
                obj: element,
                "New position {} < start position {}",
                nsecs,
                self.start_position
            );
            self.start_position
        } else {
            nsecs - self.start_position
        };

        if nsecs >= self.last_position {
            self.last_position = nsecs;
        } else {
            gst_fixme!(
                CAT,
                obj: element,
                "New position {} < last position {}",
                nsecs,
                self.last_position
            );
        }
    }

    fn add_buffer_metadata(
        &mut self,
        element: &gst::Element,
        buffer: &mut gst::buffer::Buffer,
        timecode: &gst_video::ValidVideoTimeCode,
        framerate: &gst::Fraction,
    ) {
        let buffer = buffer.get_mut().unwrap();
        gst_video::VideoTimeCodeMeta::add(buffer, &timecode);

        self.update_timestamp(element, &timecode);

        buffer.set_pts(self.last_position);
        buffer.set_duration(
            gst::SECOND
                .mul_div_ceil(*framerate.denom() as u64, *framerate.numer() as u64)
                .unwrap_or(gst::CLOCK_TIME_NONE),
        );
    }

    fn create_events(
        &mut self,
        element: &gst::Element,
        format: Format,
        framerate: &gst::Fraction,
    ) -> Vec<gst::Event> {
        let mut events = Vec::new();

        if self.format != Some(format) {
            self.format = Some(format);

            let caps = match format {
                Format::Cea708Cdp => gst::Caps::builder("closedcaption/x-cea-708")
                    .field("format", &"cdp")
                    .field("framerate", framerate)
                    .build(),
                Format::Cea608 => gst::Caps::builder("closedcaption/x-cea-608")
                    .field("format", &"s334-1a")
                    .field("framerate", framerate)
                    .build(),
            };

            events.push(gst::Event::new_caps(&caps).build());
            gst_info!(CAT, obj: element, "Caps changed to {:?}", &caps);
        }

        if self.need_segment {
            let segment = gst::FormattedSegment::<gst::format::Time>::new();
            events.push(gst::Event::new_segment(&segment).build());
            self.need_segment = false;
        }

        events.extend(self.pending_events.drain(..));
        events
    }
}

struct MccParse {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

#[derive(Debug)]
struct OffsetVec {
    vec: Vec<u8>,
    offset: usize,
    len: usize,
}

impl AsRef<[u8]> for OffsetVec {
    fn as_ref(&self) -> &[u8] {
        &self.vec[self.offset..(self.offset + self.len)]
    }
}

impl AsMut<[u8]> for OffsetVec {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.vec[self.offset..(self.offset + self.len)]
    }
}

impl MccParse {
    fn set_pad_functions(sinkpad: &gst::Pad, srcpad: &gst::Pad) {
        sinkpad.set_chain_function(|pad, parent, buffer| {
            MccParse::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |parse, element| parse.sink_chain(pad, element, buffer),
            )
        });
        sinkpad.set_event_function(|pad, parent, event| {
            MccParse::catch_panic_pad_function(
                parent,
                || false,
                |parse, element| parse.sink_event(pad, element, event),
            )
        });

        srcpad.set_event_function(|pad, parent, event| {
            MccParse::catch_panic_pad_function(
                parent,
                || false,
                |parse, element| parse.src_event(pad, element, event),
            )
        });
        srcpad.set_query_function(|pad, parent, query| {
            MccParse::catch_panic_pad_function(
                parent,
                || false,
                |parse, element| parse.src_query(pad, element, query),
            )
        });
    }

    fn handle_buffer(
        &self,
        element: &gst::Element,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        let drain;
        if let Some(buffer) = buffer {
            let buffer = buffer.into_mapped_buffer_readable().map_err(|_| {
                gst_element_error!(
                    element,
                    gst::ResourceError::Read,
                    ["Failed to map buffer readable"]
                );

                gst::FlowError::Error
            })?;

            state.reader.push(buffer);
            drain = false;
        } else {
            drain = true;
        }

        loop {
            let line = state.get_line(drain);
            match line {
                Ok(Some(MccLine::Caption(tc, data))) => {
                    gst_trace!(
                        CAT,
                        obj: element,
                        "Got caption buffer with timecode {:?} and size {}",
                        tc,
                        data.len()
                    );

                    if data.len() < 3 {
                        gst_debug!(
                            CAT,
                            obj: element,
                            "Too small caption packet: {}",
                            data.len(),
                        );
                        continue;
                    }

                    let format = match (data[0], data[1]) {
                        (0x61, 0x01) => Format::Cea708Cdp,
                        (0x61, 0x02) => Format::Cea608,
                        (did, sdid) => {
                            gst_debug!(CAT, obj: element, "Unknown DID {:x} SDID {:x}", did, sdid);
                            continue;
                        }
                    };

                    let len = data[2];
                    if data.len() < 3 + len as usize {
                        gst_debug!(
                            CAT,
                            obj: element,
                            "Too small caption packet: {} < {}",
                            data.len(),
                            3 + len,
                        );
                        continue;
                    }

                    state = self.handle_line(element, tc, data, format, state)?;
                }
                Ok(Some(MccLine::TimeCodeRate(rate, df))) => {
                    gst_debug!(
                        CAT,
                        obj: element,
                        "Got timecode rate {} (drop frame {})",
                        rate,
                        df
                    );
                    state.timecode_rate = Some((rate, df));
                }
                Ok(Some(line)) => {
                    gst_debug!(CAT, obj: element, "Got line '{:?}'", line);
                }
                Err((line, err)) => {
                    gst_element_error!(
                        element,
                        gst::StreamError::Decode,
                        ["Couldn't parse line '{:?}': {:?}", line, err]
                    );

                    break Err(gst::FlowError::Error);
                }
                Ok(None) => break Ok(gst::FlowSuccess::Ok),
            }
        }
    }

    fn handle_line(
        &self,
        element: &gst::Element,
        tc: TimeCode,
        data: Vec<u8>,
        format: Format,
        mut state: MutexGuard<State>,
    ) -> Result<MutexGuard<State>, gst::FlowError> {
        let (framerate, drop_frame) = match state.timecode_rate {
            Some((rate, false)) => (gst::Fraction::new(rate as i32, 1), false),
            Some((rate, true)) => (gst::Fraction::new(rate as i32 * 1000, 1001), true),
            None => {
                gst_element_error!(
                    element,
                    gst::StreamError::Decode,
                    ["Got caption before time code rate"]
                );

                return Err(gst::FlowError::Error);
            }
        };

        let events = state.create_events(element, format, &framerate);
        let timecode = state.handle_timecode(element, tc, framerate, drop_frame)?;

        let len = data[2] as usize;
        let mut buffer = gst::Buffer::from_mut_slice(OffsetVec {
            vec: data,
            offset: 3,
            len,
        });

        state.add_buffer_metadata(element, &mut buffer, &timecode, &framerate);

        // Update the last_timecode to the current one
        state.last_timecode = Some(timecode);

        // Drop our state mutex while we push out buffers or events
        drop(state);

        for event in events {
            gst_debug!(CAT, obj: element, "Pushing event {:?}", event);
            self.srcpad.push_event(event);
        }

        self.srcpad.push(buffer).map_err(|err| {
            gst_error!(CAT, obj: element, "Pushing buffer returned {:?}", err);
            err
        })?;

        Ok(self.state.lock().unwrap())
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        self.handle_buffer(element, Some(buffer))
    }

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Caps(_) => {
                // We send a proper caps event from the chain function later
                gst_log!(CAT, obj: pad, "Dropping caps event");
                true
            }
            EventView::Segment(_) => {
                // We send a gst::Format::Time segment event later when needed
                gst_log!(CAT, obj: pad, "Dropping segment event");
                true
            }
            EventView::FlushStop(_) => {
                let mut state = self.state.lock().unwrap();
                state.reader.clear();
                state.parser.reset();
                state.need_segment = true;
                state.pending_events.clear();
                state.start_position = gst::ClockTime::from_seconds(0);
                state.last_position = gst::ClockTime::from_seconds(0);
                state.last_timecode = None;
                state.timecode_rate = None;

                pad.event_default(element, event)
            }
            EventView::Eos(_) => {
                gst_log!(CAT, obj: pad, "Draining");
                if let Err(err) = self.handle_buffer(element, None) {
                    gst_error!(CAT, obj: pad, "Failed to drain parser: {:?}", err);
                }
                pad.event_default(element, event)
            }
            _ => {
                if event.is_sticky()
                    && !self.srcpad.has_current_caps()
                    && event.get_type() > gst::EventType::Caps
                {
                    gst_log!(CAT, obj: pad, "Deferring sticky event until we have caps");
                    let mut state = self.state.lock().unwrap();
                    state.pending_events.push(event);
                    true
                } else {
                    pad.event_default(element, event)
                }
            }
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Seek(_) => {
                gst_log!(CAT, obj: pad, "Dropping seek event");
                false
            }
            _ => pad.event_default(element, event),
        }
    }

    fn src_query(&self, pad: &gst::Pad, element: &gst::Element, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Seeking(mut q) => {
                // We don't support any seeking at all
                let fmt = q.get_format();
                q.set(
                    false,
                    gst::GenericFormattedValue::Other(fmt, -1),
                    gst::GenericFormattedValue::Other(fmt, -1),
                );
                true
            }
            QueryView::Position(ref mut q) => {
                // For Time answer ourselfs, otherwise forward
                if q.get_format() == gst::Format::Time {
                    let state = self.state.lock().unwrap();
                    q.set(state.last_position);
                    true
                } else {
                    self.sinkpad.peer_query(query)
                }
            }
            _ => pad.query_default(element, query),
        }
    }
}

impl ObjectSubclass for MccParse {
    const NAME: &'static str = "RsMccParse";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, "sink");
        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::new_from_template(&templ, "src");

        MccParse::set_pad_functions(&sinkpad, &srcpad);

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Mcc Parse",
            "Parser/ClosedCaption",
            "Parses MCC Closed Caption Files",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();

            let s = gst::Structure::builder("closedcaption/x-cea-708")
                .field("format", &"cdp")
                .build();
            caps.append_structure(s);

            let s = gst::Structure::builder("closedcaption/x-cea-608")
                .field("format", &"s334-1a")
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
        klass.add_pad_template(src_pad_template);

        let caps = gst::Caps::builder("application/x-mcc")
            .field("version", &gst::List::new(&[&1i32, &2i32]))
            .build();
        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);
    }
}

impl ObjectImpl for MccParse {
    glib_object_impl!();

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sinkpad).unwrap();
        element.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for MccParse {
    fn change_state(
        &self,
        element: &gst::Element,
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

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(plugin, "mccparse", 0, MccParse::get_type())
}
