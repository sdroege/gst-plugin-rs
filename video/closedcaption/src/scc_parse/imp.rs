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

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{
    element_error, gst_debug, gst_error, gst_fixme, gst_info, gst_log, gst_trace, gst_warning,
    loggable_error,
};

use std::cmp;
use std::convert::TryInto;
use std::sync::{Mutex, MutexGuard};

use once_cell::sync::Lazy;

use super::parser::{SccLine, SccParser};
use crate::line_reader::LineReader;
use crate::parser_utils::TimeCode;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "sccparse",
        gst::DebugColorFlags::empty(),
        Some("Scc Parser Element"),
    )
});

#[derive(Debug)]
struct PullState {
    need_stream_start: bool,
    stream_id: String,
    offset: u64,
    duration: Option<gst::ClockTime>,
}

impl PullState {
    fn new(element: &super::SccParse, pad: &gst::Pad) -> Self {
        Self {
            need_stream_start: true,
            stream_id: pad.create_stream_id(element, Some("src")).to_string(),
            offset: 0,
            duration: gst::ClockTime::NONE,
        }
    }
}

#[derive(Debug)]
struct State {
    reader: LineReader<gst::MappedBuffer<gst::buffer::Readable>>,
    parser: SccParser,
    need_segment: bool,
    pending_events: Vec<gst::Event>,
    framerate: Option<gst::Fraction>,
    last_position: Option<gst::ClockTime>,
    last_timecode: Option<gst_video::ValidVideoTimeCode>,
    segment: gst::FormattedSegment<gst::ClockTime>,

    // Pull mode
    pull: Option<PullState>,

    // seeking
    seeking: bool,
    discont: bool,
    seek_seqnum: Option<gst::Seqnum>,
    need_flush_stop: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            reader: LineReader::new(),
            parser: SccParser::new(),
            need_segment: true,
            pending_events: Vec::new(),
            framerate: None,
            last_position: None,
            last_timecode: None,
            segment: gst::FormattedSegment::new(),
            pull: None,
            seeking: false,
            discont: false,
            seek_seqnum: None,
            need_flush_stop: false,
        }
    }
}

fn parse_timecode(
    framerate: gst::Fraction,
    tc: &TimeCode,
) -> Result<gst_video::ValidVideoTimeCode, gst::FlowError> {
    use std::convert::TryFrom;

    let mut tc = tc.clone();
    // Workaround for various SCC files having invalid drop frame timecodes:
    // Every full minute the first two timecodes are skipped, except for every tenth minute.
    if tc.drop_frame
        && tc.seconds == 0
        && tc.minutes % 10 != 0
        && (tc.frames == 0 || tc.frames == 1)
    {
        tc.frames = 2;
    }

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

    gst_video::ValidVideoTimeCode::try_from(timecode).map_err(|_| gst::FlowError::Error)
}

impl State {
    #[allow(clippy::type_complexity)]
    fn line(&mut self, drain: bool) -> Result<Option<SccLine>, (&[u8], nom::error::Error<&[u8]>)> {
        let line = match self.reader.line_with_drain(drain) {
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
        tc: &TimeCode,
        framerate: gst::Fraction,
        element: &super::SccParse,
    ) -> Result<gst_video::ValidVideoTimeCode, gst::FlowError> {
        match parse_timecode(framerate, &tc) {
            Ok(timecode) => Ok(timecode),
            Err(err) => {
                let last_timecode =
                    self.last_timecode
                        .as_ref()
                        .map(Clone::clone)
                        .ok_or_else(|| {
                            element_error!(
                                element,
                                gst::StreamError::Decode,
                                ["Invalid first timecode {:?}", err]
                            );

                            gst::FlowError::Error
                        })?;

                gst_warning!(
                    CAT,
                    obj: element,
                    "Invalid timecode {:?}, using previous {:?}",
                    err,
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
        let nsecs = timecode.time_since_daily_jam();

        if self
            .last_position
            .map_or(true, |last_position| nsecs >= last_position)
        {
            self.last_position = Some(nsecs);
        } else {
            gst_fixme!(
                CAT,
                obj: element,
                "New position {} < last position {}",
                nsecs,
                self.last_position.display(),
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
            gst::ClockTime::SECOND
                .mul_div_ceil(*framerate.denom() as u64, *framerate.numer() as u64),
        );
    }

    fn create_events(
        &mut self,
        element: &super::SccParse,
        framerate: Option<gst::Fraction>,
    ) -> Vec<gst::Event> {
        let mut events = Vec::new();

        if self.need_flush_stop {
            let mut b = gst::event::FlushStop::builder(true);

            if let Some(seek_seqnum) = self.seek_seqnum {
                b = b.seqnum(seek_seqnum);
            }

            events.push(b.build());
            self.need_flush_stop = false;
        }

        if let Some(pull) = &mut self.pull {
            if pull.need_stream_start {
                events.push(gst::event::StreamStart::new(&pull.stream_id));
                pull.need_stream_start = false;
            }
        }

        if let Some(framerate) = framerate {
            if self.framerate != Some(framerate) {
                self.framerate = Some(framerate);

                let caps = gst::Caps::builder("closedcaption/x-cea-608")
                    .field("format", &"raw")
                    .field("framerate", &framerate)
                    .build();
                self.framerate = Some(framerate);

                events.push(gst::event::Caps::new(&caps));
                gst_info!(CAT, obj: element, "Caps changed to {:?}", &caps);
            }
        }

        if self.need_segment {
            let mut b = gst::event::Segment::builder(&self.segment);

            if let Some(seek_seqnum) = self.seek_seqnum {
                b = b.seqnum(seek_seqnum);
            }

            events.push(b.build());
            self.need_segment = false;
        }

        events.extend(self.pending_events.drain(..));
        events
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
                element_error!(
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
            let line = state.line(drain);
            match line {
                Ok(Some(SccLine::Caption(tc, data))) => {
                    state = self.handle_line(tc, data, element, state)?;
                }
                Ok(Some(line)) => {
                    gst_debug!(CAT, obj: element, "Got line '{:?}'", line);
                }
                Err((line, err)) => {
                    element_error!(
                        element,
                        gst::StreamError::Decode,
                        ["Couldn't parse line '{:?}': {:?}", line, err]
                    );

                    break Err(gst::FlowError::Error);
                }
                Ok(None) => {
                    if drain && state.pull.is_some() {
                        break Err(gst::FlowError::Eos);
                    }
                    break Ok(gst::FlowSuccess::Ok);
                }
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

        let mut timecode = state.handle_timecode(&tc, framerate, element)?;
        let start_time = timecode.time_since_daily_jam();
        let segment_start = state.segment.start();
        let clip_buffers = if state.seeking {
            // If we are in the middle of seeking, check whether this line
            // contains start frame, and if so, unset seeking flag
            let num_bufs = (data.len() / 2) as i64;
            let mut end_timecode = timecode.clone();
            // add one more frame here so that add duration of the last frame
            end_timecode.add_frames(num_bufs + 1);
            let stop_time = end_timecode.time_since_daily_jam();

            gst_trace!(
                CAT,
                obj: element,
                "Checking inside of segment, line start {} line stop {} segment start {} num bufs {}",
                start_time,
                stop_time,
                segment_start.display(),
                num_bufs,
            );

            if segment_start.map_or(false, |seg_start| stop_time > seg_start) {
                state.seeking = false;
                state.discont = true;
                state.need_flush_stop = true;
            }

            // Still need to scan lines to find the first buffer
            if state.seeking {
                drop(state);
                return Ok(self.state.lock().unwrap());
            }

            true
        } else {
            false
        };

        let mut buffers = if clip_buffers {
            gst::BufferList::new()
        } else {
            gst::BufferList::new_sized(data.len() / 2)
        };

        let mut send_eos = false;
        for d in data.chunks_exact(2) {
            let mut buffer = gst::Buffer::with_size(d.len()).unwrap();
            {
                let buf_mut = buffer.get_mut().unwrap();
                buf_mut.copy_from_slice(0, d).unwrap();
            }

            state.add_buffer_metadata(&mut buffer, &timecode, framerate, element);
            timecode.increment_frame();

            if clip_buffers {
                let end_time = buffer
                    .pts()
                    .zip(buffer.duration())
                    .map(|(pts, duration)| pts + duration);
                if end_time
                    .zip(segment_start)
                    .map_or(false, |(end_time, segment_start)| end_time < segment_start)
                {
                    gst_trace!(
                        CAT,
                        obj: element,
                        "Skip segment clipped buffer {:?}",
                        buffer,
                    );

                    continue;
                }
            }

            send_eos = state.segment.stop().map_or(false, |stop| {
                buffer
                    .pts()
                    .zip(buffer.duration())
                    .map_or(false, |(pts, duration)| pts + duration >= stop)
            });

            let buffers = buffers.get_mut().unwrap();
            buffers.add(buffer);

            // Terminate loop once we found EOS boundary buffer
            if send_eos {
                break;
            }
        }

        // Update the last_timecode to the current one
        state.last_timecode = Some(timecode);

        let events = state.create_events(element, Some(framerate));

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

        if send_eos {
            return Err(gst::FlowError::Eos);
        }

        Ok(self.state.lock().unwrap())
    }

    fn sink_activate(
        &self,
        pad: &gst::Pad,
        element: &super::SccParse,
    ) -> Result<(), gst::LoggableError> {
        let mode = {
            let mut query = gst::query::Scheduling::new();
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

    fn start_task(&self, element: &super::SccParse) -> Result<(), gst::LoggableError> {
        let element_weak = element.downgrade();
        let pad_weak = self.sinkpad.downgrade();
        let res = self.sinkpad.start_task(move || {
            let element = match element_weak.upgrade() {
                Some(element) => element,
                None => {
                    if let Some(pad) = pad_weak.upgrade() {
                        let _ = pad.pause_task();
                    }
                    return;
                }
            };

            let parse = Self::from_instance(&element);
            parse.loop_fn(&element);
        });
        if res.is_err() {
            return Err(loggable_error!(CAT, "Failed to start pad task"));
        }
        Ok(())
    }

    fn sink_activatemode(
        &self,
        _pad: &gst::Pad,
        element: &super::SccParse,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        if mode == gst::PadMode::Pull {
            if active {
                self.start_task(element)?;
            } else {
                let _ = self.sinkpad.stop_task();
            }
        }

        Ok(())
    }

    fn scan_duration(
        &self,
        element: &super::SccParse,
    ) -> Result<Option<gst_video::ValidVideoTimeCode>, gst::LoggableError> {
        gst_debug!(CAT, obj: element, "Scanning duration");

        /* First let's query the bytes duration upstream */
        let mut q = gst::query::Duration::new(gst::Format::Bytes);

        if !self.sinkpad.peer_query(&mut q) {
            return Err(loggable_error!(CAT, "Failed to query upstream duration"));
        }

        let size = match q.result().try_into().unwrap() {
            Some(gst::format::Bytes(size)) => size,
            None => {
                return Err(loggable_error!(CAT, "Failed to query upstream duration"));
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
                    return Err(loggable_error!(
                        CAT,
                        "Failed to pull buffer while scanning duration: {:?}",
                        flow
                    ));
                }
            }

            let mut reader = LineReader::new();
            let mut parser = SccParser::new_scan_captions();

            for buf in buffers.iter().rev() {
                let buf = buf
                    .clone()
                    .into_mapped_buffer_readable()
                    .map_err(|_| loggable_error!(CAT, "Failed to map buffer readable"))?;

                reader.push(buf);
            }

            while let Some(line) = reader.line_with_drain(true) {
                if let Ok(SccLine::Caption(tc, data)) =
                    parser.parse_line(line).map_err(|err| (line, err))
                {
                    let framerate = if tc.drop_frame {
                        gst::Fraction::new(30000, 1001)
                    } else {
                        gst::Fraction::new(30, 1)
                    };

                    if let Ok(mut timecode) = parse_timecode(framerate, &tc) {
                        /* We're looking for the total duration */
                        timecode.add_frames((data.len() / 2) as i64 + 1);
                        last_tc = Some(timecode);
                    }
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

    fn push_eos(&self, element: &super::SccParse) {
        let mut state = self.state.lock().unwrap();

        if state.seeking {
            state.need_flush_stop = true;
        }

        let mut events = state.create_events(element, None);
        let mut eos_event = gst::event::Eos::builder();

        if let Some(seek_seqnum) = state.seek_seqnum {
            eos_event = eos_event.seqnum(seek_seqnum);
        }

        events.push(eos_event.build());

        // Drop our state mutex while we push out events
        drop(state);

        for event in events {
            gst_debug!(CAT, obj: element, "Pushing event {:?}", event);
            self.srcpad.push_event(event);
        }
    }

    fn loop_fn(&self, element: &super::SccParse) {
        let mut state = self.state.lock().unwrap();
        let State {
            ref framerate,
            ref mut pull,
            ..
        } = *state;
        let mut pull = pull.as_mut().unwrap();
        let scan_duration = framerate.is_none() && pull.duration.is_none();
        let offset = pull.offset;

        pull.offset += 4096;

        drop(state);

        let buffer = match self.sinkpad.pull_range(offset, 4096) {
            Ok(buffer) => Some(buffer),
            Err(gst::FlowError::Eos) => None,
            Err(gst::FlowError::Flushing) => {
                gst_debug!(CAT, obj: &self.sinkpad, "Pausing after pulling buffer, reason: flushing");

                let _ = self.sinkpad.pause_task();
                return;
            }
            Err(flow) => {
                gst_error!(CAT, obj: &self.sinkpad, "Failed to pull, reason: {:?}", flow);

                element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Streaming stopped, failed to pull buffer"]
                );

                let _ = self.sinkpad.pause_task();
                return;
            }
        };

        match self.handle_buffer(element, buffer) {
            Ok(_) => {
                if scan_duration {
                    match self.scan_duration(element) {
                        Ok(Some(tc)) => {
                            let mut state = self.state.lock().unwrap();
                            let mut pull = state.pull.as_mut().unwrap();
                            pull.duration = Some(tc.time_since_daily_jam());
                        }
                        Ok(None) => {
                            let mut state = self.state.lock().unwrap();
                            let mut pull = state.pull.as_mut().unwrap();
                            pull.duration = Some(gst::ClockTime::ZERO);
                        }
                        Err(err) => {
                            err.log();

                            element_error!(
                                element,
                                gst::StreamError::Decode,
                                ["Failed to scan duration"]
                            );

                            let _ = self.sinkpad.pause_task();
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

                        element_error!(
                            element,
                            gst::StreamError::Failed,
                            ["Streaming stopped, reason: {:?}", flow]
                        );
                    }
                }

                let _ = self.sinkpad.pause_task();
            }
        }
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

    fn flush(&self, mut state: MutexGuard<State>) -> MutexGuard<State> {
        state.reader.clear();
        state.parser.reset();
        if let Some(pull) = &mut state.pull {
            pull.offset = 0;
        }
        state.segment = gst::FormattedSegment::new();
        state.need_segment = true;
        state.pending_events.clear();
        state.last_position = None;
        state.last_timecode = None;

        drop(state);

        self.state.lock().unwrap()
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
                let state = self.state.lock().unwrap();
                let state = self.flush(state);
                drop(state);

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
                    && event.type_() > gst::EventType::Caps
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

    fn perform_seek(&self, event: &gst::event::Seek, element: &super::SccParse) -> bool {
        if self.state.lock().unwrap().pull.is_none() {
            gst_error!(CAT, obj: element, "seeking is only supported in pull mode");
            return false;
        }

        let (rate, flags, start_type, start, stop_type, stop) = event.get();

        let mut start: Option<gst::ClockTime> = match start.try_into() {
            Ok(start) => start,
            Err(_) => {
                gst_error!(CAT, obj: element, "seek has invalid format");
                return false;
            }
        };

        let mut stop: Option<gst::ClockTime> = match stop.try_into() {
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

        let seek_seqnum = event.seqnum();

        let event = gst::event::FlushStart::builder()
            .seqnum(seek_seqnum)
            .build();

        gst_debug!(CAT, obj: element, "Sending event {:?} upstream", event);
        self.sinkpad.push_event(event);

        let event = gst::event::FlushStart::builder()
            .seqnum(seek_seqnum)
            .build();

        gst_debug!(CAT, obj: element, "Pushing event {:?}", event);
        self.srcpad.push_event(event);

        let _ = self.sinkpad.pause_task();

        let mut state = self.state.lock().unwrap();
        let pull = state.pull.as_ref().unwrap();

        if start_type == gst::SeekType::Set {
            start = start
                .zip(pull.duration)
                .map(|(start, duration)| start.min(duration))
                .or(start);
        }

        if stop_type == gst::SeekType::Set {
            stop = stop
                .zip(pull.duration)
                .map(|(stop, duration)| stop.min(duration))
                .or(stop);
        }

        state.seeking = true;
        state.seek_seqnum = Some(seek_seqnum);

        state = self.flush(state);

        let event = gst::event::FlushStop::builder(true)
            .seqnum(seek_seqnum)
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

    fn src_event(&self, pad: &gst::Pad, element: &super::SccParse, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);
        match event.view() {
            EventView::Seek(e) => self.perform_seek(&e, element),
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
                let state = self.state.lock().unwrap();

                let fmt = q.format();

                if fmt == gst::Format::Time {
                    if let Some(pull) = state.pull.as_ref() {
                        q.set(
                            true,
                            gst::GenericFormattedValue::Time(gst::ClockTime::ZERO.into()),
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
                if q.format() == gst::Format::Time {
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
                if q.format() == gst::Format::Time {
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

#[glib::object_subclass]
impl ObjectSubclass for SccParse {
    const NAME: &'static str = "RsSccParse";
    type Type = super::SccParse;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .activate_function(|pad, parent| {
                SccParse::catch_panic_pad_function(
                    parent,
                    || Err(loggable_error!(CAT, "Panic activating sink pad")),
                    |parse, element| parse.sink_activate(pad, element),
                )
            })
            .activatemode_function(|pad, parent, mode, active| {
                SccParse::catch_panic_pad_function(
                    parent,
                    || Err(loggable_error!(CAT, "Panic activating sink pad with mode")),
                    |parse, element| parse.sink_activatemode(pad, element, mode, active),
                )
            })
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

        let templ = klass.pad_template("src").unwrap();
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
}

impl ObjectImpl for SccParse {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for SccParse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
            "Scc Parse",
            "Parser/ClosedCaption",
            "Parses SCC Closed Caption Files",
            "Sebastian Dröge <sebastian@centricular.com>, Jordan Petridis <jordan@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("closedcaption/x-cea-608")
                .field("format", &"raw")
                .field(
                    "framerate",
                    &gst::List::new(&[
                        &gst::Fraction::new(30000, 1001),
                        &gst::Fraction::new(30, 1),
                    ]),
                )
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst::Caps::builder("application/x-scc").build();
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
