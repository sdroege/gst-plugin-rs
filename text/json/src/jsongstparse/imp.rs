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
use gst::{gst_debug, gst_error, gst_info, gst_log, gst_trace, gst_warning};

use once_cell::sync::Lazy;

use std::cmp;
use std::convert::TryInto;
use std::sync::{Mutex, MutexGuard};

use serde::Deserialize;

use crate::line_reader::LineReader;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "jsongstparse",
        gst::DebugColorFlags::empty(),
        Some("GStreamer Json Parser Element"),
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
    fn new(element: &super::JsonGstParse, pad: &gst::Pad) -> Self {
        Self {
            need_stream_start: true,
            stream_id: pad.create_stream_id(element, Some("src")).to_string(),
            offset: 0,
            duration: None,
        }
    }
}

#[derive(Debug)]
struct State {
    reader: LineReader<gst::MappedBuffer<gst::buffer::Readable>>,
    need_segment: bool,
    need_caps: bool,
    format: Option<String>,
    pending_events: Vec<gst::Event>,
    last_position: Option<gst::ClockTime>,
    segment: gst::FormattedSegment<gst::ClockTime>,

    // Pull mode
    pull: Option<PullState>,

    // seeking
    seeking: bool,
    discont: bool,
    seek_seqnum: Option<gst::Seqnum>,
    last_raw_line: Vec<u8>,
    replay_last_line: bool,
    need_flush_stop: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            reader: LineReader::new(),
            need_segment: true,
            need_caps: true,
            format: None,
            pending_events: Vec::new(),
            last_position: None,
            segment: gst::FormattedSegment::<gst::ClockTime>::new(),
            pull: None,
            seeking: false,
            discont: false,
            seek_seqnum: None,
            last_raw_line: Vec::new(),
            replay_last_line: false,
            need_flush_stop: false,
        }
    }
}

#[derive(Deserialize, Debug)]
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

impl State {
    fn line(&mut self, drain: bool) -> Result<Option<Line>, (&[u8], serde_json::Error)> {
        let line = if self.replay_last_line {
            self.replay_last_line = false;
            &self.last_raw_line
        } else {
            match self.reader.line_with_drain(drain) {
                None => {
                    return Ok(None);
                }
                Some(line) => {
                    self.last_raw_line = line.to_vec();
                    line
                }
            }
        };

        let line: Line = serde_json::from_slice(&line).map_err(|err| (line, err))?;

        Ok(Some(line))
    }

    fn create_events(&mut self, element: &super::JsonGstParse) -> Vec<gst::Event> {
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

        if self.need_caps {
            let mut caps_builder = gst::Caps::builder("application/x-json");

            if let Some(format) = &self.format {
                caps_builder = caps_builder.field("format", format);
            }

            let caps = caps_builder.build();

            events.push(gst::event::Caps::new(&caps));
            gst_info!(CAT, obj: element, "Caps changed to {:?}", &caps);
            self.need_caps = false;
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

    fn add_buffer_metadata(
        &mut self,
        _element: &super::JsonGstParse,
        buffer: &mut gst::buffer::Buffer,
        pts: Option<gst::ClockTime>,
        duration: Option<gst::ClockTime>,
    ) {
        let buffer = buffer.get_mut().unwrap();

        self.last_position = pts.zip(duration).map(|(pts, duration)| pts + duration);

        buffer.set_pts(pts);

        if self.discont {
            buffer.set_flags(gst::BufferFlags::DISCONT);
            self.discont = false;
        }

        buffer.set_duration(duration);
    }
}

pub struct JsonGstParse {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

impl JsonGstParse {
    fn handle_buffer(
        &self,
        element: &super::JsonGstParse,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        let drain;
        if let Some(buffer) = buffer {
            let buffer = buffer.into_mapped_buffer_readable().map_err(|_| {
                gst::element_error!(
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
            let seeking = state.seeking;
            let line = state.line(drain);
            match line {
                Ok(Some(Line::Buffer {
                    pts,
                    duration,
                    data,
                })) => {
                    gst_debug!(
                        CAT,
                        obj: element,
                        "Got buffer with timestamp {} and duration {}",
                        pts.display(),
                        duration.display(),
                    );

                    if !seeking {
                        let data = data.to_string().clone();
                        let mut events = state.create_events(element);

                        let mut buffer = gst::Buffer::from_slice(data);

                        if let Some(last_position) = state.last_position {
                            if let Some(duration) = pts.map(|pts| pts.checked_sub(last_position)) {
                                events.push(
                                    gst::event::Gap::builder(last_position)
                                        .duration(duration)
                                        .build(),
                                );
                            }
                        }

                        state.add_buffer_metadata(element, &mut buffer, pts, duration);

                        let send_eos = state
                            .segment
                            .stop()
                            .zip(buffer.pts())
                            .zip(buffer.duration())
                            .map_or(false, |((stop, pts), duration)| pts + duration >= stop);

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

                        state = self.state.lock().unwrap();
                    } else {
                        state = self.handle_skipped_line(element, pts, state);
                    }
                }
                Ok(Some(Line::Header { format })) => {
                    if state.format.is_none() {
                        state.format = Some(format);
                    } else {
                        gst_warning!(CAT, obj: element, "Ignoring format change",);
                    }
                }
                Err((line, err)) => {
                    gst_error!(
                        CAT,
                        obj: element,
                        "Couldn't parse line '{:?}': {:?}",
                        std::str::from_utf8(line),
                        err
                    );

                    gst::element_error!(
                        element,
                        gst::StreamError::Decode,
                        ["Couldn't parse line '{:?}': {:?}", line, err]
                    );

                    break Err(gst::FlowError::Error);
                }
                Ok(None) => {
                    if drain && state.pull.is_some() {
                        eprintln!("Finished draining");
                        break Err(gst::FlowError::Eos);
                    }
                    break Ok(gst::FlowSuccess::Ok);
                }
            }
        }
    }

    fn handle_skipped_line(
        &self,
        element: &super::JsonGstParse,
        pts: impl Into<Option<gst::ClockTime>>,
        mut state: MutexGuard<State>,
    ) -> MutexGuard<State> {
        if pts
            .into()
            .zip(state.segment.start())
            .map_or(false, |(pts, start)| pts >= start)
        {
            state.seeking = false;
            state.discont = true;
            state.replay_last_line = true;
            state.need_flush_stop = true;

            gst_debug!(CAT, obj: element, "Done seeking");
        }

        drop(state);

        self.state.lock().unwrap()
    }

    fn sink_activate(
        &self,
        pad: &gst::Pad,
        element: &super::JsonGstParse,
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

    fn start_task(&self, element: &super::JsonGstParse) -> Result<(), gst::LoggableError> {
        let element_weak = element.downgrade();
        let pad_weak = self.sinkpad.downgrade();
        let res = self.sinkpad.start_task(move || {
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
        });
        if res.is_err() {
            return Err(gst::loggable_error!(CAT, "Failed to start pad task"));
        }
        Ok(())
    }

    fn sink_activatemode(
        &self,
        _pad: &gst::Pad,
        element: &super::JsonGstParse,
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
        element: &super::JsonGstParse,
    ) -> Result<Option<gst::ClockTime>, gst::LoggableError> {
        gst_debug!(CAT, obj: element, "Scanning duration");

        /* First let's query the bytes duration upstream */
        let mut q = gst::query::Duration::new(gst::Format::Bytes);

        if !self.sinkpad.peer_query(&mut q) {
            return Err(gst::loggable_error!(
                CAT,
                "Failed to query upstream duration"
            ));
        }

        let size = match q.result().try_into().ok().flatten() {
            Some(gst::format::Bytes(size)) => size,
            None => {
                return Err(gst::loggable_error!(
                    CAT,
                    "Failed to query upstream duration"
                ));
            }
        };

        let mut offset = size;
        let mut buffers = Vec::new();
        let mut last_pts = None;

        loop {
            let scan_size = cmp::min(offset, 4096);

            offset -= scan_size;

            match self.sinkpad.pull_range(offset, scan_size as u32) {
                Ok(buffer) => {
                    buffers.push(buffer);
                }
                Err(flow) => {
                    return Err(gst::loggable_error!(
                        CAT,
                        "Failed to pull buffer while scanning duration: {:?}",
                        flow
                    ));
                }
            }

            let mut reader = LineReader::new();

            for buf in buffers.iter().rev() {
                let buf = buf
                    .clone()
                    .into_mapped_buffer_readable()
                    .map_err(|_| gst::loggable_error!(CAT, "Failed to map buffer readable"))?;

                reader.push(buf);
            }

            while let Some(line) = reader.line_with_drain(true) {
                if let Ok(Line::Buffer {
                    pts,
                    duration,
                    data: _data,
                }) = serde_json::from_slice(&line)
                {
                    last_pts = pts.zip(duration).map(|(pts, duration)| pts + duration);
                }
            }

            if last_pts.is_some() || offset == 0 {
                gst_debug!(
                    CAT,
                    obj: element,
                    "Duration scan done, last_pts: {:?}",
                    last_pts
                );
                break (Ok(last_pts));
            }
        }
    }

    fn push_eos(&self, element: &super::JsonGstParse) {
        let mut state = self.state.lock().unwrap();

        if state.seeking {
            state.need_flush_stop = true;
        }

        let mut events = state.create_events(element);
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

    fn loop_fn(&self, element: &super::JsonGstParse) {
        let mut state = self.state.lock().unwrap();
        let State { ref mut pull, .. } = *state;
        let mut pull = pull.as_mut().unwrap();
        let offset = pull.offset;
        let scan_duration = pull.duration.is_none();

        pull.offset += 4096;

        drop(state);

        if scan_duration {
            match self.scan_duration(element) {
                Ok(pts) => {
                    let mut state = self.state.lock().unwrap();
                    let mut pull = state.pull.as_mut().unwrap();
                    pull.duration = pts;
                }
                Err(err) => {
                    err.log();

                    gst::element_error!(
                        element,
                        gst::StreamError::Decode,
                        ["Failed to scan duration"]
                    );

                    self.sinkpad.pause_task().unwrap();
                }
            }
        }

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

                gst::element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Streaming stopped, failed to pull buffer"]
                );

                self.sinkpad.pause_task().unwrap();
                return;
            }
        };

        if let Err(flow) = self.handle_buffer(element, buffer) {
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

                    gst::element_error!(
                        element,
                        gst::StreamError::Failed,
                        ["Streaming stopped, reason: {:?}", flow]
                    );
                }
            }

            self.sinkpad.pause_task().unwrap();
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::JsonGstParse,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        self.handle_buffer(element, Some(buffer))
    }

    fn flush(&self, mut state: &mut State) {
        state.reader.clear();
        if let Some(pull) = &mut state.pull {
            pull.offset = 0;
        }
        state.segment = gst::FormattedSegment::<gst::ClockTime>::new();
        state.need_segment = true;
        state.need_caps = true;
        state.pending_events.clear();
        state.last_position = None;
        state.last_raw_line = [].to_vec();
        state.format = None;
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::JsonGstParse, event: gst::Event) -> bool {
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
                self.flush(&mut state);
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

    fn perform_seek(&self, event: &gst::event::Seek, element: &super::JsonGstParse) -> bool {
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

        self.sinkpad.pause_task().unwrap();

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

        self.flush(&mut state);

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

    fn src_event(&self, pad: &gst::Pad, element: &super::JsonGstParse, event: gst::Event) -> bool {
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
        element: &super::JsonGstParse,
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
                            gst::GenericFormattedValue::Time(Some(gst::ClockTime::ZERO)),
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
impl ObjectSubclass for JsonGstParse {
    const NAME: &'static str = "RsJsonGstParse";
    type Type = super::JsonGstParse;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .activate_function(|pad, parent| {
                JsonGstParse::catch_panic_pad_function(
                    parent,
                    || Err(gst::loggable_error!(CAT, "Panic activating sink pad")),
                    |parse, element| parse.sink_activate(pad, element),
                )
            })
            .activatemode_function(|pad, parent, mode, active| {
                JsonGstParse::catch_panic_pad_function(
                    parent,
                    || {
                        Err(gst::loggable_error!(
                            CAT,
                            "Panic activating sink pad with mode"
                        ))
                    },
                    |parse, element| parse.sink_activatemode(pad, element, mode, active),
                )
            })
            .chain_function(|pad, parent, buffer| {
                JsonGstParse::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |parse, element| parse.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                JsonGstParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.sink_event(pad, element, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .event_function(|pad, parent, event| {
                JsonGstParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.src_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                JsonGstParse::catch_panic_pad_function(
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

impl ObjectImpl for JsonGstParse {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl ElementImpl for JsonGstParse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "JSON GStreamer parser",
                "Parser/JSON",
                "Parses ndjson as output by jsongstenc",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("application/x-json").build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst::Caps::new_any();
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
