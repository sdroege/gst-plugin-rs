// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2019 Jordan Petridis <jordan@centricular.com>
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

use glib::subclass;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::{Mutex, MutexGuard};

use super::parser::{SccLine, SccParser, TimeCode};
use crate::line_reader::LineReader;

lazy_static! {
    static ref CAT: gst::DebugCategory = {
        gst::DebugCategory::new(
            "sccparse",
            gst::DebugColorFlags::empty(),
            Some("Scc Parser Element"),
        )
    };
}

#[derive(Debug)]
struct State {
    reader: LineReader<gst::MappedBuffer<gst::buffer::Readable>>,
    parser: SccParser,
    need_segment: bool,
    pending_events: Vec<gst::Event>,
    framerate: Option<gst::Fraction>,
    last_position: gst::ClockTime,
    last_timecode: Option<gst_video::ValidVideoTimeCode>,
}

type CombineError<'a> = combine::easy::ParseError<&'a [u8]>;

impl Default for State {
    fn default() -> Self {
        Self {
            reader: LineReader::new(),
            parser: SccParser::new(),
            need_segment: true,
            pending_events: Vec::new(),
            framerate: None,
            last_position: gst::CLOCK_TIME_NONE,
            last_timecode: None,
        }
    }
}

impl State {
    fn get_line(&mut self, drain: bool) -> Result<Option<SccLine>, (&[u8], CombineError)> {
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
        tc: TimeCode,
        framerate: gst::Fraction,
        element: &super::SccParse,
    ) -> Result<gst_video::ValidVideoTimeCode, gst::FlowError> {
        use std::convert::TryInto;

        let timecode = gst_video::VideoTimeCode::new(
            framerate,
            None,
            if tc.drop_frame {
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
        timecode: &gst_video::ValidVideoTimeCode,
        element: &super::SccParse,
    ) {
        let nsecs = gst::ClockTime::from(timecode.nsec_since_daily_jam());

        if self.last_position.is_none() || nsecs >= self.last_position {
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
        buffer: &mut gst::buffer::Buffer,
        timecode: &gst_video::ValidVideoTimeCode,
        framerate: gst::Fraction,
        element: &super::SccParse,
    ) {
        let buffer = buffer.get_mut().unwrap();
        gst_video::VideoTimeCodeMeta::add(buffer, &timecode);

        self.update_timestamp(timecode, element);

        buffer.set_pts(self.last_position);
        buffer.set_duration(
            gst::SECOND
                .mul_div_ceil(*framerate.denom() as u64, *framerate.numer() as u64)
                .unwrap_or(gst::CLOCK_TIME_NONE),
        );
    }
}

pub struct SccParse {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

impl SccParse {
    fn handle_buffer(
        &self,
        element: &super::SccParse,
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
                Ok(Some(SccLine::Caption(tc, data))) => {
                    state = self.handle_line(tc, data, element, state)?;
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
        tc: TimeCode,
        data: Vec<u8>,
        element: &super::SccParse,
        mut state: MutexGuard<State>,
    ) -> Result<MutexGuard<State>, gst::FlowError> {
        gst_trace!(
            CAT,
            obj: element,
            "Got caption buffer with timecode {:?} and size {}",
            tc,
            data.len()
        );

        // The framerate is defined as 30 or 30000/1001 according to:
        // http://www.theneitherworld.com/mcpoodle/SCC_TOOLS/DOCS/SCC_FORMAT.HTML
        let framerate = if tc.drop_frame {
            gst::Fraction::new(30000, 1001)
        } else {
            gst::Fraction::new(30, 1)
        };

        let mut events = Vec::new();

        if Some(framerate) != state.framerate {
            let caps = gst::Caps::builder("closedcaption/x-cea-608")
                .field("format", &"raw")
                .field("framerate", &framerate)
                .build();
            events.push(gst::event::Caps::new(&caps));
            state.framerate = Some(framerate);
        }

        if state.need_segment {
            let segment = gst::FormattedSegment::<gst::format::Time>::new();
            events.push(gst::event::Segment::new(&segment));
            state.need_segment = false;
        }

        events.extend(state.pending_events.drain(..));

        let mut timecode = state.handle_timecode(tc, framerate, element)?;
        let mut buffers = gst::BufferList::new_sized(data.len() / 2);
        for d in data.chunks_exact(2) {
            let mut buffer = gst::Buffer::with_size(d.len()).unwrap();
            {
                let buf_mut = buffer.get_mut().unwrap();
                buf_mut.copy_from_slice(0, d).unwrap();
            }

            state.add_buffer_metadata(&mut buffer, &timecode, framerate, element);
            timecode.increment_frame();
            let buffers = buffers.get_mut().unwrap();
            buffers.add(buffer);
        }

        // Update the last_timecode to the current one
        state.last_timecode = Some(timecode);

        // Drop our state mutex while we push out buffers or events
        drop(state);

        for event in events {
            gst_debug!(CAT, obj: element, "Pushing event {:?}", event);
            self.srcpad.push_event(event);
        }

        self.srcpad.push_list(buffers).map_err(|err| {
            gst_error!(CAT, obj: element, "Pushing buffer returned {:?}", err);
            err
        })?;

        Ok(self.state.lock().unwrap())
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::SccParse,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        self.handle_buffer(element, Some(buffer))
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::SccParse, event: gst::Event) -> bool {
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
                state.last_position = gst::ClockTime::from_seconds(0);
                state.last_timecode = None;

                pad.event_default(Some(element), event)
            }
            EventView::Eos(_) => {
                gst_log!(CAT, obj: pad, "Draining");
                if let Err(err) = self.handle_buffer(element, None) {
                    gst_error!(CAT, obj: pad, "Failed to drain parser: {:?}", err);
                }
                pad.event_default(Some(element), event)
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
                    pad.event_default(Some(element), event)
                }
            }
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &super::SccParse, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Seek(_) => {
                gst_log!(CAT, obj: pad, "Dropping seek event");
                false
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        element: &super::SccParse,
        query: &mut gst::QueryRef,
    ) -> bool {
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
            _ => pad.query_default(Some(element), query),
        }
    }
}

impl ObjectSubclass for SccParse {
    const NAME: &'static str = "RsSccParse";
    type Type = super::SccParse;
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                SccParse::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |parse, element| parse.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                SccParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.sink_event(pad, element, event),
                )
            })
            .build();

        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .event_function(|pad, parent, event| {
                SccParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.src_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                SccParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.src_query(pad, element, query),
                )
            })
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
        }
    }

    fn class_init(klass: &mut Self::Class) {
        klass.set_metadata(
            "Scc Parse",
            "Parser/ClosedCaption",
            "Parses SCC Closed Caption Files",
            "Sebastian Dröge <sebastian@centricular.com>, Jordan Petridis <jordan@centricular.com>",
        );

        let caps = gst::Caps::builder("closedcaption/x-cea-608")
            .field("format", &"raw")
            .field(
                "framerate",
                &gst::List::new(&[&gst::Fraction::new(30000, 1001), &gst::Fraction::new(30, 1)]),
            )
            .build();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        let caps = gst::Caps::builder("application/x-scc").build();
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

impl ObjectImpl for SccParse {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for SccParse {
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
