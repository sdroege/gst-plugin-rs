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

use std::cmp;
use std::sync::{Mutex, MutexGuard};

use crate::line_reader::LineReader;
use crate::mcc_parser::{MccLine, MccParser, TimeCode};

lazy_static! {
    static ref CAT: gst::DebugCategory = {
        gst::DebugCategory::new(
            "mccparse",
            gst::DebugColorFlags::empty(),
            Some("Mcc Parser Element"),
        )
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Cea708Cdp,
    Cea608,
}

#[derive(Debug)]
struct PullState {
    need_stream_start: bool,
    stream_id: String,
    offset: u64,
    duration: gst::ClockTime,
}

impl PullState {
    fn new(element: &gst::Element, pad: &gst::Pad) -> Self {
        Self {
            need_stream_start: true,
            stream_id: pad
                .create_stream_id(element, Some("src"))
                .unwrap()
                .to_string(),
            offset: 0,
            duration: gst::CLOCK_TIME_NONE,
        }
    }
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
    segment: gst::FormattedSegment<gst::ClockTime>,

    // Pull mode
    pull: Option<PullState>,

    // seeking
    seeking: bool,
    discont: bool,
    seek_seqnum: gst::Seqnum,
    last_raw_line: Vec<u8>,
    replay_last_line: bool,
    need_flush_stop: bool,
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
            segment: gst::FormattedSegment::<gst::ClockTime>::new(),
            pull: None,
            seeking: false,
            discont: false,
            seek_seqnum: gst::event::SEQNUM_INVALID,
            last_raw_line: Vec::new(),
            replay_last_line: false,
            need_flush_stop: false,
        }
    }
}

fn parse_timecode(
    framerate: gst::Fraction,
    drop_frame: bool,
    tc: TimeCode,
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

    timecode.try_into().map_err(|_| gst::FlowError::Error)
}

fn parse_timecode_rate(
    timecode_rate: Option<(u8, bool)>,
) -> Result<(gst::Fraction, bool), gst::FlowError> {
    let (framerate, drop_frame) = match timecode_rate {
        Some((rate, false)) => (gst::Fraction::new(rate as i32, 1), false),
        Some((rate, true)) => (gst::Fraction::new(rate as i32 * 1000, 1001), true),
        None => {
            return Err(gst::FlowError::Error);
        }
    };

    Ok((framerate, drop_frame))
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
        let line = match self.replay_last_line {
            true => {
                self.replay_last_line = false;
                &self.last_raw_line
            }
            false => match self.reader.get_line_with_drain(drain) {
                None => {
                    return Ok(None);
                }
                Some(line) => {
                    self.last_raw_line = line.to_vec();
                    line
                }
            },
        };

        self.parser
            .parse_line(line, !self.seeking)
            .map(Option::Some)
            .map_err(|err| (line, err))
    }

    fn handle_timecode(
        &mut self,
        element: &gst::Element,
        framerate: gst::Fraction,
        drop_frame: bool,
        tc: TimeCode,
    ) -> Result<ValidVideoTimeCode, gst::FlowError> {
        match parse_timecode(framerate, drop_frame, tc) {
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

        if self.discont {
            buffer.set_flags(gst::BufferFlags::DISCONT);
            self.discont = false;
        }

        buffer.set_duration(
            gst::SECOND
                .mul_div_ceil(*framerate.denom() as u64, *framerate.numer() as u64)
                .unwrap_or(gst::CLOCK_TIME_NONE),
        );
    }

    fn create_events(
        &mut self,
        element: &gst::Element,
        format: Option<Format>,
        framerate: &gst::Fraction,
    ) -> Vec<gst::Event> {
        let mut events = Vec::new();

        if self.need_flush_stop {
            let mut b = gst::Event::new_flush_stop(true);

            if self.seek_seqnum != gst::event::SEQNUM_INVALID {
                b = b.seqnum(self.seek_seqnum);
            }

            events.push(b.build());
            self.need_flush_stop = false;
        }

        if let Some(pull) = &mut self.pull {
            if pull.need_stream_start {
                events.push(gst::Event::new_stream_start(&pull.stream_id).build());
                pull.need_stream_start = false;
            }
        }

        if let Some(format) = format {
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
        }

        if self.need_segment {
            let mut b = gst::Event::new_segment(&self.segment);

            if self.seek_seqnum != gst::event::SEQNUM_INVALID {
                b = b.seqnum(self.seek_seqnum);
            }

            events.push(b.build());
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
        sinkpad.set_activate_function(|pad, parent| {
            MccParse::catch_panic_pad_function(
                parent,
                || Err(gst_loggable_error!(CAT, "Panic activating sink pad")),
                |parse, element| parse.sink_activate(pad, element),
            )
        });

        sinkpad.set_activatemode_function(|pad, parent, mode, active| {
            MccParse::catch_panic_pad_function(
                parent,
                || {
                    Err(gst_loggable_error!(
                        CAT,
                        "Panic activating sink pad with mode"
                    ))
                },
                |parse, element| parse.sink_activatemode(pad, element, mode, active),
            )
        });
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
        scan_tc_rate: bool,
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
                Ok(Some(MccLine::Caption(tc, Some(data)))) => {
                    assert!(!state.seeking);

                    gst_debug!(
                        CAT,
                        obj: element,
                        "Got caption buffer with timecode {:?} and size {}",
                        tc,
                        data.len()
                    );

                    if scan_tc_rate {
                        gst_element_error!(
                            element,
                            gst::StreamError::Decode,
                            ["Found caption line while scanning for timecode rate"]
                        );

                        break Err(gst::FlowError::Error);
                    }

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
                Ok(Some(MccLine::Caption(tc, None))) => {
                    assert!(state.seeking);

                    if scan_tc_rate {
                        gst_element_error!(
                            element,
                            gst::StreamError::Decode,
                            ["Found caption line while scanning for timecode rate"]
                        );

                        break Err(gst::FlowError::Error);
                    }

                    state = self.handle_skipped_line(element, tc, state)?;
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

                    if scan_tc_rate {
                        break Ok(gst::FlowSuccess::Ok);
                    }
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
                Ok(None) => {
                    if scan_tc_rate {
                        gst_element_error!(
                            element,
                            gst::StreamError::Decode,
                            ["Found end of input while scanning for timecode rate"]
                        );

                        break Err(gst::FlowError::Error);
                    }

                    if drain && state.pull.is_some() {
                        break Err(gst::FlowError::Eos);
                    }
                    break Ok(gst::FlowSuccess::Ok);
                }
            }
        }
    }

    fn handle_skipped_line(
        &self,
        element: &gst::Element,
        tc: TimeCode,
        mut state: MutexGuard<State>,
    ) -> Result<MutexGuard<State>, gst::FlowError> {
        let (framerate, drop_frame) = parse_timecode_rate(state.timecode_rate)?;
        let timecode = state.handle_timecode(element, framerate, drop_frame, tc)?;
        let nsecs = gst::ClockTime::from(timecode.nsec_since_daily_jam());

        state.last_timecode = Some(timecode);

        if nsecs >= state.segment.get_start() {
            state.seeking = false;
            state.discont = true;
            state.replay_last_line = true;
            state.need_flush_stop = true;
        }

        drop(state);

        Ok(self.state.lock().unwrap())
    }

    fn handle_line(
        &self,
        element: &gst::Element,
        tc: TimeCode,
        data: Vec<u8>,
        format: Format,
        mut state: MutexGuard<State>,
    ) -> Result<MutexGuard<State>, gst::FlowError> {
        let (framerate, drop_frame) = parse_timecode_rate(state.timecode_rate)?;
        let events = state.create_events(element, Some(format), &framerate);
        let timecode = state.handle_timecode(element, framerate, drop_frame, tc)?;

        let len = data[2] as usize;
        let mut buffer = gst::Buffer::from_mut_slice(OffsetVec {
            vec: data,
            offset: 3,
            len,
        });

        state.add_buffer_metadata(element, &mut buffer, &timecode, &framerate);

        // Update the last_timecode to the current one
        state.last_timecode = Some(timecode);

        let send_eos = state.segment.get_stop().is_some()
            && buffer.get_pts() + buffer.get_duration() >= state.segment.get_stop();

        // Drop our state mutex while we push out buffers or events
        drop(state);

        for event in events {
            gst_debug!(CAT, obj: element, "Pushing event {:?}", event);
            self.srcpad.push_event(event);
        }

        self.srcpad.push(buffer).map_err(|err| {
            if err != gst::FlowError::Flushing {
                gst_error!(CAT, obj: element, "Pushing buffer returned {:?}", err);
            }
            err
        })?;

        if send_eos {
            return Err(gst::FlowError::Eos);
        }

        Ok(self.state.lock().unwrap())
    }

    fn sink_activate(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
    ) -> Result<(), gst::LoggableError> {
        let mode = {
            let mut query = gst::Query::new_scheduling();
            let mut state = self.state.lock().unwrap();

            state.pull = None;

            if !pad.peer_query(&mut query) {
                gst_debug!(CAT, obj: pad, "Scheduling query failed on peer");
                gst::PadMode::Push
            } else if query
                .has_scheduling_mode_with_flags(gst::PadMode::Pull, gst::SchedulingFlags::SEEKABLE)
            {
                gst_debug!(CAT, obj: pad, "Activating in Pull mode");

                state.pull = Some(PullState::new(element, &self.srcpad));

                gst::PadMode::Pull
            } else {
                gst_debug!(CAT, obj: pad, "Activating in Push mode");
                gst::PadMode::Push
            }
        };

        pad.activate_mode(mode, true)?;
        Ok(())
    }

    fn start_task(&self, element: &gst::Element) -> Result<(), gst::LoggableError> {
        let element_weak = element.downgrade();
        let pad_weak = self.sinkpad.downgrade();
        if let Err(_) = self.sinkpad.start_task(move || {
            let element = match element_weak.upgrade() {
                Some(element) => element,
                None => {
                    if let Some(pad) = pad_weak.upgrade() {
                        pad.pause_task().unwrap();
                    }
                    return;
                }
            };

            let parse = Self::from_instance(&element);
            parse.loop_fn(&element);
        }) {
            return Err(gst_loggable_error!(CAT, "Failed to start pad task"));
        }
        Ok(())
    }

    fn sink_activatemode(
        &self,
        _pad: &gst::Pad,
        element: &gst::Element,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if active {
            if mode == gst::PadMode::Pull {
                self.start_task(element)?;
            }
        } else {
            if mode == gst::PadMode::Pull {
                let _ = self.sinkpad.stop_task();
            }
        }

        Ok(())
    }

    fn scan_duration(
        &self,
        element: &gst::Element,
    ) -> Result<Option<ValidVideoTimeCode>, gst::LoggableError> {
        gst_debug!(CAT, obj: element, "Scanning duration");

        /* First let's query the bytes duration upstream */
        let mut q = gst::query::Query::new_duration(gst::Format::Bytes);

        if !self.sinkpad.peer_query(&mut q) {
            return Err(gst_loggable_error!(
                CAT,
                "Failed to query upstream duration"
            ));
        }

        let size = match q.get_result().try_into_bytes().unwrap() {
            gst::format::Bytes(Some(size)) => size,
            gst::format::Bytes(None) => {
                return Err(gst_loggable_error!(
                    CAT,
                    "Failed to query upstream duration"
                ));
            }
        };

        let mut offset = size;
        let mut buffers = Vec::new();
        let mut last_tc = None;

        loop {
            let scan_size = cmp::min(offset, 4096);

            offset -= scan_size;

            match self.sinkpad.pull_range(offset, scan_size as u32) {
                Ok(buffer) => {
                    buffers.push(buffer);
                }
                Err(flow) => {
                    return Err(gst_loggable_error!(
                        CAT,
                        "Failed to pull buffer while scanning duration: {:?}",
                        flow
                    ));
                }
            }

            let mut reader = LineReader::new();
            let mut parser = MccParser::new_scan_captions();

            for buf in buffers.iter().rev() {
                let buf = buf
                    .clone()
                    .into_mapped_buffer_readable()
                    .map_err(|_| gst_loggable_error!(CAT, "Failed to map buffer readable"))?;

                reader.push(buf);
            }

            loop {
                let line = match reader.get_line_with_drain(true) {
                    Some(line) => line,
                    None => {
                        break;
                    }
                };

                match parser.parse_line(line, false).map_err(|err| (line, err)) {
                    Ok(MccLine::Caption(tc, None)) => {
                        let state = self.state.lock().unwrap();
                        let (framerate, drop_frame) = parse_timecode_rate(state.timecode_rate)
                            .map_err(|_| {
                                gst_loggable_error!(CAT, "Failed to parse timecode rate")
                            })?;
                        last_tc = match parse_timecode(framerate, drop_frame, tc) {
                            Ok(mut timecode) => {
                                /* We're looking for the total duration */
                                timecode.increment_frame();
                                Some(timecode)
                            }
                            Err(_) => None,
                        }
                    }
                    _ => { /* We ignore everything else including errors */ }
                }
            }

            if last_tc.is_some() || offset == 0 {
                gst_debug!(
                    CAT,
                    obj: element,
                    "Duration scan done, last_tc: {:?}",
                    last_tc
                );
                break (Ok(last_tc));
            }
        }
    }

    fn push_eos(&self, element: &gst::Element) {
        let mut state = self.state.lock().unwrap();

        if state.seeking {
            state.need_flush_stop = true;
        }

        match parse_timecode_rate(state.timecode_rate) {
            Ok((framerate, _)) => {
                let mut events = state.create_events(element, None, &framerate);
                let mut eos_event = gst::Event::new_eos();

                if state.seek_seqnum != gst::event::SEQNUM_INVALID {
                    eos_event = eos_event.seqnum(state.seek_seqnum);
                }

                events.push(eos_event.build());

                // Drop our state mutex while we push out events
                drop(state);

                for event in events {
                    gst_debug!(CAT, obj: element, "Pushing event {:?}", event);
                    self.srcpad.push_event(event);
                }
            }
            Err(_) => {
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Streaming stopped, failed to parse timecode rate"]
                );
            }
        }
    }

    fn loop_fn(&self, element: &gst::Element) {
        let mut state = self.state.lock().unwrap();
        let State {
            timecode_rate: ref tc_rate,
            ref mut pull,
            ..
        } = *state;
        let mut pull = pull.as_mut().unwrap();
        let scan_tc_rate = tc_rate.is_none() && pull.duration.is_none();
        let offset = pull.offset;

        pull.offset += 4096;

        drop(state);

        let buffer = match self.sinkpad.pull_range(offset, 4096) {
            Ok(buffer) => Some(buffer),
            Err(gst::FlowError::Eos) => None,
            Err(gst::FlowError::Flushing) => {
                gst_debug!(CAT, obj: &self.sinkpad, "Pausing after pulling buffer, reason: flushing");

                self.sinkpad.pause_task().unwrap();
                return;
            }
            Err(flow) => {
                gst_error!(CAT, obj: &self.sinkpad, "Failed to pull, reason: {:?}", flow);

                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Streaming stopped, failed to pull buffer"]
                );

                self.sinkpad.pause_task().unwrap();
                return;
            }
        };

        match self.handle_buffer(element, buffer, scan_tc_rate) {
            Ok(_) => {
                let tc_rate = self.state.lock().unwrap().timecode_rate;
                if scan_tc_rate && tc_rate.is_some() {
                    match self.scan_duration(element) {
                        Ok(Some(tc)) => {
                            let mut state = self.state.lock().unwrap();
                            let mut pull = state.pull.as_mut().unwrap();
                            pull.duration = tc.nsec_since_daily_jam().into();
                        }
                        Ok(None) => {
                            let mut state = self.state.lock().unwrap();
                            let mut pull = state.pull.as_mut().unwrap();
                            pull.duration = 0.into();
                        }
                        Err(err) => {
                            err.log();

                            gst_element_error!(
                                element,
                                gst::StreamError::Decode,
                                ["Failed to scan duration"]
                            );

                            self.sinkpad.pause_task().unwrap();
                        }
                    }
                }
            }
            Err(flow) => {
                match flow {
                    gst::FlowError::Flushing => {
                        gst_debug!(CAT, obj: element, "Pausing after flow {:?}", flow);
                    }
                    gst::FlowError::Eos => {
                        self.push_eos(element);

                        gst_debug!(CAT, obj: element, "Pausing after flow {:?}", flow);
                    }
                    _ => {
                        self.push_eos(element);

                        gst_error!(CAT, obj: element, "Pausing after flow {:?}", flow);

                        gst_element_error!(
                            element,
                            gst::StreamError::Failed,
                            ["Streaming stopped, reason: {:?}", flow]
                        );
                    }
                }

                self.sinkpad.pause_task().unwrap();
            }
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        self.handle_buffer(element, Some(buffer), false)
    }

    fn flush(&self, mut state: MutexGuard<State>) -> MutexGuard<State> {
        state.reader.clear();
        state.parser.reset();
        if let Some(pull) = &mut state.pull {
            pull.offset = 0;
        }
        state.segment = gst::FormattedSegment::<gst::ClockTime>::new();
        state.need_segment = true;
        state.pending_events.clear();
        state.start_position = 0.into();
        state.last_position = 0.into();
        state.last_timecode = None;
        state.timecode_rate = None;
        state.last_raw_line = [].to_vec();

        drop(state);

        self.state.lock().unwrap()
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
                let state = self.state.lock().unwrap();

                let _ = self.flush(state);

                pad.event_default(Some(element), event)
            }
            EventView::Eos(_) => {
                gst_log!(CAT, obj: pad, "Draining");
                if let Err(err) = self.handle_buffer(element, None, false) {
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

    fn perform_seek(&self, event: &gst::event::Seek, element: &gst::Element) -> bool {
        let mut state = self.state.lock().unwrap();

        if state.pull.is_none() {
            gst_error!(CAT, obj: element, "seeking is only supported in pull mode");
            return false;
        }

        let (rate, flags, start_type, start, stop_type, stop) = event.get();

        let mut start = match start.try_into_time() {
            Ok(start) => start,
            Err(_) => {
                gst_error!(CAT, obj: element, "seek has invalid format");
                return false;
            }
        };

        let mut stop = match stop.try_into_time() {
            Ok(stop) => stop,
            Err(_) => {
                gst_error!(CAT, obj: element, "seek has invalid format");
                return false;
            }
        };

        if !flags.contains(gst::SeekFlags::FLUSH) {
            gst_error!(CAT, obj: element, "only flushing seeks are supported");
            return false;
        }

        if start_type == gst::SeekType::End || stop_type == gst::SeekType::End {
            gst_error!(CAT, obj: element, "Relative seeks are not supported");
            return false;
        }

        let pull = state.pull.as_ref().unwrap();

        if start_type == gst::SeekType::Set && pull.duration.is_some() {
            start = cmp::min(start, pull.duration);
        }

        if stop_type == gst::SeekType::Set && pull.duration.is_some() {
            stop = cmp::min(stop, pull.duration);
        }

        state.seeking = true;
        state.seek_seqnum = event.get_seqnum();

        let event = gst::Event::new_flush_start()
            .seqnum(state.seek_seqnum)
            .build();

        gst_debug!(CAT, obj: element, "Sending event {:?} upstream", event);
        self.sinkpad.push_event(event);

        let event = gst::Event::new_flush_start()
            .seqnum(state.seek_seqnum)
            .build();

        gst_debug!(CAT, obj: element, "Pushing event {:?}", event);
        self.srcpad.push_event(event);

        self.sinkpad.pause_task().unwrap();

        state = self.flush(state);

        let event = gst::Event::new_flush_stop(true)
            .seqnum(state.seek_seqnum)
            .build();

        /* Drop our state while we push a serialized event upstream */
        drop(state);

        gst_debug!(CAT, obj: element, "Sending event {:?} upstream", event);
        self.sinkpad.push_event(event);

        state = self.state.lock().unwrap();

        state
            .segment
            .do_seek(rate, flags, start_type, start, stop_type, stop);

        match self.start_task(element) {
            Err(error) => {
                error.log();
                false
            }
            _ => true,
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Seek(e) => self.perform_seek(&e, element),
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_query(&self, pad: &gst::Pad, element: &gst::Element, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Seeking(mut q) => {
                let state = self.state.lock().unwrap();

                let fmt = q.get_format();

                if fmt == gst::Format::Time {
                    if let Some(pull) = state.pull.as_ref() {
                        q.set(
                            true,
                            gst::GenericFormattedValue::Time(0.into()),
                            gst::GenericFormattedValue::Time(pull.duration),
                        );
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
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
            QueryView::Duration(ref mut q) => {
                // For Time answer ourselfs, otherwise forward
                let state = self.state.lock().unwrap();
                if q.get_format() == gst::Format::Time {
                    if let Some(pull) = state.pull.as_ref() {
                        if pull.duration.is_some() {
                            q.set(state.pull.as_ref().unwrap().duration);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    self.sinkpad.peer_query(query)
                }
            }
            _ => pad.query_default(Some(element), query),
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
        let sinkpad = gst::Pad::new_from_template(&templ, Some("sink"));
        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::new_from_template(&templ, Some("src"));

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
    gst::Element::register(Some(plugin), "mccparse", 0, MccParse::get_type())
}
