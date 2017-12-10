// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
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
use gst;
use gst::prelude::*;
use gst_video;

use gst_plugin::properties::*;
use gst_plugin::object::*;
use gst_plugin::element::*;

use std::sync::{Arc, Condvar, Mutex};
use std::collections::HashMap;
use std::iter;
use std::cmp;
use std::f64;

const DEFAULT_RECORD: bool = false;

#[derive(Debug, Clone, Copy)]
struct Settings {
    record: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            record: DEFAULT_RECORD,
        }
    }
}

static PROPERTIES: [Property; 2] = [
    Property::Boolean(
        "record",
        "Record",
        "Enable/disable recording",
        DEFAULT_RECORD,
        PropertyMutability::ReadWrite,
    ),
    Property::Boolean(
        "recording",
        "Recording",
        "Whether recording is currently taking place",
        DEFAULT_RECORD,
        PropertyMutability::Readable,
    ),
];

#[derive(Clone)]
struct Stream {
    sinkpad: gst::Pad,
    srcpad: gst::Pad,
    state: Arc<Mutex<StreamState>>,
}

impl PartialEq for Stream {
    fn eq(&self, other: &Self) -> bool {
        self.sinkpad == other.sinkpad && self.srcpad == other.srcpad
    }
}

impl Eq for Stream {}

impl Stream {
    fn new(sinkpad: gst::Pad, srcpad: gst::Pad) -> Self {
        Self {
            sinkpad: sinkpad,
            srcpad: srcpad,
            state: Arc::new(Mutex::new(StreamState::default())),
        }
    }
}

struct StreamState {
    in_segment: gst::FormattedSegment<gst::ClockTime>,
    out_segment: gst::FormattedSegment<gst::ClockTime>,
    segment_seqnum: gst::Seqnum,
    current_running_time: gst::ClockTime,
    eos: bool,
    flushing: bool,
    segment_pending: bool,
    pending_events: Vec<gst::Event>,
}

impl Default for StreamState {
    fn default() -> Self {
        Self {
            in_segment: gst::FormattedSegment::new(),
            out_segment: gst::FormattedSegment::new(),
            segment_seqnum: gst::util_seqnum_next(),
            current_running_time: gst::CLOCK_TIME_NONE,
            eos: false,
            flushing: false,
            segment_pending: false,
            pending_events: Vec::new(),
        }
    }
}

// Recording behaviour:
//
// Secondary streams are *always* behind main stream
// Main stream EOS stops recording (-> Stopping), makes secondary streams go EOS
//
// Recording: Passing through all data
// Stopping: Main stream remembering current last_recording_stop, waiting for all
//           other streams to reach this position
// Stopped: Dropping all data
// Starting: Main stream waiting until next keyframe and setting last_recording_start, waiting
//           for all other streams to reach this position
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordingState {
    Recording,
    Stopping,
    Stopped,
    Starting,
}

#[derive(Debug)]
struct State {
    recording_state: RecordingState,
    last_recording_start: gst::ClockTime,
    last_recording_stop: gst::ClockTime,
    // Accumulated duration of previous recording segments,
    // updated whenever going to Stopped
    recording_duration: gst::ClockTime,
    // Updated whenever going to Recording
    running_time_offset: gst::ClockTime,
}

impl Default for State {
    fn default() -> Self {
        Self {
            recording_state: RecordingState::Stopped,
            last_recording_start: gst::CLOCK_TIME_NONE,
            last_recording_stop: gst::CLOCK_TIME_NONE,
            recording_duration: 0.into(),
            running_time_offset: gst::CLOCK_TIME_NONE,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HandleResult {
    Pass,
    Drop,
    Eos,
    Flushing,
}

struct ToggleRecord {
    cat: gst::DebugCategory,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    main_stream: Stream,
    // Always must have main_stream.state locked!
    // If multiple stream states have to be locked, the
    // main_stream always comes first
    main_stream_cond: Condvar,
    other_streams: Mutex<(Vec<Stream>, u32)>,
    pads: Mutex<HashMap<gst::Pad, Stream>>,
}

impl ToggleRecord {
    fn new(_element: &Element, sinkpad: gst::Pad, srcpad: gst::Pad) -> Self {
        let main_stream = Stream::new(sinkpad, srcpad);

        let mut pads = HashMap::new();
        pads.insert(main_stream.sinkpad.clone(), main_stream.clone());
        pads.insert(main_stream.srcpad.clone(), main_stream.clone());

        Self {
            cat: gst::DebugCategory::new(
                "togglerecord",
                gst::DebugColorFlags::empty(),
                "Toggle Record Element",
            ),
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            main_stream: main_stream,
            main_stream_cond: Condvar::new(),
            other_streams: Mutex::new((Vec::new(), 0)),
            pads: Mutex::new(pads),
        }
    }

    fn class_init(klass: &mut ElementClass) {
        klass.set_metadata(
            "Toggle Record",
            "Generic",
            "Valve that ensures multiple streams start/end at the same time",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_any();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        );
        klass.add_pad_template(src_pad_template);

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        );
        klass.add_pad_template(sink_pad_template);

        let src_pad_template = gst::PadTemplate::new(
            "src_%u",
            gst::PadDirection::Src,
            gst::PadPresence::Sometimes,
            &caps,
        );
        klass.add_pad_template(src_pad_template);

        let sink_pad_template = gst::PadTemplate::new(
            "sink_%u",
            gst::PadDirection::Sink,
            gst::PadPresence::Request,
            &caps,
        );
        klass.add_pad_template(sink_pad_template);

        klass.install_properties(&PROPERTIES);
    }

    fn init(element: &Element) -> Box<ElementImpl<Element>> {
        let templ = element.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, "sink");
        let templ = element.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::new_from_template(&templ, "src");

        ToggleRecord::set_pad_functions(&sinkpad, &srcpad);
        element.add_pad(&sinkpad).unwrap();
        element.add_pad(&srcpad).unwrap();

        let imp = Self::new(element, sinkpad, srcpad);
        Box::new(imp)
    }

    fn catch_panic_pad_function<T, F: FnOnce(&Self, &Element) -> T, G: FnOnce() -> T>(
        parent: &Option<gst::Object>,
        fallback: G,
        f: F,
    ) -> T {
        let element = parent
            .as_ref()
            .cloned()
            .unwrap()
            .downcast::<Element>()
            .unwrap();
        let togglerecord = element.get_impl().downcast_ref::<ToggleRecord>().unwrap();
        element.catch_panic(fallback, |element| f(togglerecord, &element))
    }

    fn set_pad_functions(sinkpad: &gst::Pad, srcpad: &gst::Pad) {
        sinkpad.set_chain_function(|pad, parent, buffer| {
            ToggleRecord::catch_panic_pad_function(
                parent,
                || gst::FlowReturn::Error,
                |togglerecord, element| togglerecord.sink_chain(pad, element, buffer),
            )
        });
        sinkpad.set_event_function(|pad, parent, event| {
            ToggleRecord::catch_panic_pad_function(
                parent,
                || false,
                |togglerecord, element| togglerecord.sink_event(pad, element, event),
            )
        });
        sinkpad.set_query_function(|pad, parent, query| {
            ToggleRecord::catch_panic_pad_function(
                parent,
                || false,
                |togglerecord, element| togglerecord.sink_query(pad, element, query),
            )
        });
        sinkpad.set_iterate_internal_links_function(|pad, parent| {
            ToggleRecord::catch_panic_pad_function(
                parent,
                || gst::Iterator::from_vec(vec![]),
                |togglerecord, element| togglerecord.iterate_internal_links(pad, element),
            )
        });

        srcpad.set_event_function(|pad, parent, event| {
            ToggleRecord::catch_panic_pad_function(
                parent,
                || false,
                |togglerecord, element| togglerecord.src_event(pad, element, event),
            )
        });
        srcpad.set_query_function(|pad, parent, query| {
            ToggleRecord::catch_panic_pad_function(
                parent,
                || false,
                |togglerecord, element| togglerecord.src_query(pad, element, query),
            )
        });
        srcpad.set_iterate_internal_links_function(|pad, parent| {
            ToggleRecord::catch_panic_pad_function(
                parent,
                || gst::Iterator::from_vec(vec![]),
                |togglerecord, element| togglerecord.iterate_internal_links(pad, element),
            )
        });
    }

    fn handle_main_stream(
        &self,
        element: &Element,
        pad: &gst::Pad,
        stream: &Stream,
        is_keyframe: bool,
        mut dts_or_pts: gst::ClockTime,
        duration: gst::ClockTime,
    ) -> HandleResult {
        let mut state = stream.state.lock().unwrap();

        let mut dts_or_pts_end = if duration.is_some() {
            dts_or_pts + duration
        } else {
            dts_or_pts
        };

        dts_or_pts = cmp::max(state.in_segment.get_start(), dts_or_pts);
        dts_or_pts_end = cmp::max(state.in_segment.get_start(), dts_or_pts_end);
        if state.in_segment.get_stop().is_some() {
            dts_or_pts = cmp::min(state.in_segment.get_stop(), dts_or_pts);
            dts_or_pts_end = cmp::min(state.in_segment.get_stop(), dts_or_pts_end);
        }

        let mut current_running_time = state.in_segment.to_running_time(dts_or_pts);
        current_running_time = cmp::max(current_running_time, state.current_running_time);
        state.current_running_time = current_running_time;

        // Wake up everybody, we advanced a bit
        // Important: They will only be able to advance once we're done with this
        // function or waiting for them to catch up below, otherwise they might
        // get the wrong state
        self.main_stream_cond.notify_all();

        let current_running_time_end = state.in_segment.to_running_time(dts_or_pts_end);

        gst_log!(
            self.cat,
            obj: pad,
            "Main stream current running time {}-{} (position: {}-{})",
            current_running_time,
            current_running_time_end,
            dts_or_pts,
            dts_or_pts_end
        );

        let settings = *self.settings.lock().unwrap();

        // First check if we have to update our recording state
        let mut rec_state = self.state.lock().unwrap();
        let settings_changed = match rec_state.recording_state {
            RecordingState::Recording if !settings.record => {
                gst_debug!(self.cat, obj: pad, "Stopping recording");
                rec_state.recording_state = RecordingState::Stopping;
                true
            }
            RecordingState::Stopped if settings.record => {
                gst_debug!(self.cat, obj: pad, "Starting recording");
                rec_state.recording_state = RecordingState::Starting;
                true
            }
            _ => false,
        };

        if settings_changed {
            drop(rec_state);
            drop(state);
            gst_debug!(self.cat, obj: pad, "Requesting a new keyframe");
            stream.sinkpad.push_event(
                gst_video::new_upstream_force_key_unit_event(gst::CLOCK_TIME_NONE, true, 0).build(),
            );

            state = stream.state.lock().unwrap();
            rec_state = self.state.lock().unwrap();
        }

        match rec_state.recording_state {
            RecordingState::Recording => {
                // Remember where we stopped last, in case of EOS
                rec_state.last_recording_stop = current_running_time_end;
                gst_log!(self.cat, obj: pad, "Passing buffer (recording)");
                HandleResult::Pass
            }
            RecordingState::Stopping => {
                if !is_keyframe {
                    // Remember where we stopped last, in case of EOS
                    rec_state.last_recording_stop = current_running_time_end;
                    gst_log!(self.cat, obj: pad, "Passing non-keyframe buffer (stopping)");
                    return HandleResult::Pass;
                }

                // Remember the time when we stopped: now!
                rec_state.last_recording_stop = current_running_time;
                gst_debug!(self.cat, obj: pad, "Stopping at {}", current_running_time);

                // Then unlock and wait for all other streams to reach
                // it or go EOS instead.
                drop(rec_state);

                while !self.other_streams.lock().unwrap().0.iter().all(|s| {
                    let s = s.state.lock().unwrap();
                    s.eos
                        || (s.current_running_time.is_some()
                            && s.current_running_time >= current_running_time)
                }) {
                    gst_log!(self.cat, obj: pad, "Waiting for other streams to stop");
                    state = self.main_stream_cond.wait(state).unwrap();
                }

                if state.flushing {
                    gst_debug!(self.cat, obj: pad, "Flushing");
                    return HandleResult::Flushing;
                }

                let mut rec_state = self.state.lock().unwrap();
                rec_state.recording_state = RecordingState::Stopped;
                rec_state.recording_duration +=
                    rec_state.last_recording_stop - rec_state.last_recording_start;
                rec_state.last_recording_start = gst::CLOCK_TIME_NONE;
                rec_state.last_recording_stop = gst::CLOCK_TIME_NONE;

                gst_debug!(
                    self.cat,
                    obj: pad,
                    "Stopped at {}, recording duration {}",
                    current_running_time,
                    rec_state.recording_duration
                );

                // Then become Stopped and drop this buffer. We always stop right before
                // a keyframe
                gst_log!(self.cat, obj: pad, "Dropping buffer (stopped)");

                drop(rec_state);
                drop(state);
                self.notify(&element.clone().upcast(), "recording");

                HandleResult::Drop
            }
            RecordingState::Stopped => {
                gst_log!(self.cat, obj: pad, "Dropping buffer (stopped)");
                HandleResult::Drop
            }
            RecordingState::Starting => {
                // If this is no keyframe, we can directly go out again here and drop the frame
                if !is_keyframe {
                    gst_log!(
                        self.cat,
                        obj: pad,
                        "Dropping non-keyframe buffer (starting)"
                    );
                    return HandleResult::Drop;
                }

                // Remember the time when we started: now!
                rec_state.last_recording_start = current_running_time;
                rec_state.running_time_offset = current_running_time - rec_state.recording_duration;
                gst_debug!(self.cat, obj: pad, "Starting at {}", current_running_time);

                state.segment_pending = true;
                for other_stream in &self.other_streams.lock().unwrap().0 {
                    other_stream.state.lock().unwrap().segment_pending = true;
                }

                // Then unlock and wait for all other streams to reach
                // it or go EOS instead
                drop(rec_state);

                while !self.other_streams.lock().unwrap().0.iter().all(|s| {
                    let s = s.state.lock().unwrap();
                    s.eos
                        || (s.current_running_time.is_some()
                            && s.current_running_time >= current_running_time)
                }) {
                    gst_log!(self.cat, obj: pad, "Waiting for other streams to start");
                    state = self.main_stream_cond.wait(state).unwrap();
                }

                if state.flushing {
                    gst_debug!(self.cat, obj: pad, "Flushing");
                    return HandleResult::Flushing;
                }

                let mut rec_state = self.state.lock().unwrap();
                rec_state.recording_state = RecordingState::Recording;
                gst_debug!(
                    self.cat,
                    obj: pad,
                    "Started at {}, recording duration {}",
                    current_running_time,
                    rec_state.recording_duration
                );

                gst_log!(self.cat, obj: pad, "Passing buffer (recording)");

                drop(rec_state);
                drop(state);
                self.notify(&element.clone().upcast(), "recording");

                HandleResult::Pass
            }
        }
    }

    fn handle_secondary_stream(
        &self,
        pad: &gst::Pad,
        stream: &Stream,
        mut pts: gst::ClockTime,
        duration: gst::ClockTime,
    ) -> HandleResult {
        // Calculate end pts & current running time and make sure we stay in the segment
        let mut state = stream.state.lock().unwrap();

        let mut pts_end = if duration.is_some() {
            pts + duration
        } else {
            pts
        };

        pts = cmp::max(state.in_segment.get_start(), pts);
        if state.in_segment.get_stop().is_some() && pts >= state.in_segment.get_stop() {
            state.current_running_time = state
                .in_segment
                .to_running_time(state.in_segment.get_stop());
            state.eos = true;
            gst_debug!(
                self.cat,
                obj: pad,
                "After segment end {} >= {}, EOS",
                pts,
                state.in_segment.get_stop()
            );

            return HandleResult::Eos;
        }
        pts_end = cmp::max(state.in_segment.get_start(), pts_end);
        if state.in_segment.get_stop().is_some() {
            pts_end = cmp::min(state.in_segment.get_stop(), pts_end);
        }

        let mut current_running_time = state.in_segment.to_running_time(pts);
        current_running_time = cmp::max(current_running_time, state.current_running_time);
        state.current_running_time = current_running_time;

        let current_running_time_end = state.in_segment.to_running_time(pts_end);
        gst_log!(
            self.cat,
            obj: pad,
            "Secondary stream current running time {}-{} (position: {}-{}",
            current_running_time,
            current_running_time_end,
            pts,
            pts_end
        );

        drop(state);

        let mut main_state = self.main_stream.state.lock().unwrap();

        // Wake up, in case the main stream is waiting for us to progress up to here. We progressed
        // above but all notifying must happen while the main_stream state is locked as per above.
        self.main_stream_cond.notify_all();

        while (main_state.current_running_time == gst::CLOCK_TIME_NONE
            || main_state.current_running_time < current_running_time)
            && !main_state.eos && !stream.state.lock().unwrap().flushing
        {
            gst_log!(
                self.cat,
                obj: pad,
                "Waiting for reaching {} / EOS / flushing, main stream at {}",
                current_running_time,
                main_state.current_running_time
            );

            main_state = self.main_stream_cond.wait(main_state).unwrap();
        }
        if stream.state.lock().unwrap().flushing {
            gst_debug!(self.cat, obj: pad, "Flushing");
            return HandleResult::Flushing;
        }

        let rec_state = self.state.lock().unwrap();

        // If the main stream is EOS, we are also EOS unless we are
        // before the final last recording stop running time
        if main_state.eos {
            // If we have no start or stop position (we never recorded), or are after the current
            // stop position that we're EOS now
            // If we're before the start position (we were starting before EOS),
            // drop the buffer
            if rec_state.last_recording_stop.is_none() || rec_state.last_recording_start.is_none()
                || current_running_time_end > rec_state.last_recording_stop
            {
                gst_debug!(
                    self.cat,
                    obj: pad,
                    "Main stream EOS and we're EOS ({} > {})",
                    current_running_time_end,
                    rec_state.last_recording_stop
                );
                return HandleResult::Eos;
            } else if current_running_time < rec_state.last_recording_start {
                gst_debug!(
                    self.cat,
                    obj: pad,
                    "Main stream EOS and we're not EOS yet (before recording start, {} <= {})",
                    current_running_time,
                    rec_state.last_recording_start
                );
                return HandleResult::Drop;
            } else {
                gst_debug!(
                    self.cat,
                    obj: pad,
                    "Main stream EOS and we're not EOS yet (before recording end, {} <= {} < {})",
                    rec_state.last_recording_start,
                    current_running_time,
                    rec_state.last_recording_stop
                );
                return HandleResult::Pass;
            }
        }

        match rec_state.recording_state {
            RecordingState::Recording => {
                // We're properly started, must have a start position and
                // be actually after that start position
                assert!(rec_state.last_recording_start.is_some());
                assert!(current_running_time >= rec_state.last_recording_start);
                gst_log!(self.cat, obj: pad, "Passing buffer (recording)");
                HandleResult::Pass
            }
            RecordingState::Stopping => {
                // If we have no start position yet, the main stream is waiting for a key-frame
                if rec_state.last_recording_stop.is_none() {
                    gst_log!(
                        self.cat,
                        obj: pad,
                        "Passing buffer (stopping: waiting for keyframe)",
                    );
                    HandleResult::Pass
                } else if current_running_time_end <= rec_state.last_recording_stop {
                    gst_log!(
                        self.cat,
                        obj: pad,
                        "Passing buffer (stopping: {} <= {})",
                        current_running_time_end,
                        rec_state.last_recording_stop
                    );
                    HandleResult::Pass
                } else {
                    gst_log!(
                        self.cat,
                        obj: pad,
                        "Dropping buffer (stopping: {} > {})",
                        current_running_time_end,
                        rec_state.last_recording_stop
                    );
                    HandleResult::Drop
                }
            }
            RecordingState::Stopped => {
                // We're properly stopped
                gst_log!(self.cat, obj: pad, "Dropping buffer (stopped)");
                HandleResult::Drop
            }
            RecordingState::Starting => {
                // If we have no start position yet, the main stream is waiting for a key-frame
                if rec_state.last_recording_start.is_none() {
                    gst_log!(
                        self.cat,
                        obj: pad,
                        "Dropping buffer (starting: waiting for keyframe)",
                    );
                    HandleResult::Drop
                } else if current_running_time >= rec_state.last_recording_start {
                    gst_log!(
                        self.cat,
                        obj: pad,
                        "Passing buffer (starting: {} >= {})",
                        current_running_time,
                        rec_state.last_recording_start
                    );
                    HandleResult::Pass
                } else {
                    gst_log!(
                        self.cat,
                        obj: pad,
                        "Dropping buffer (starting: {} < {})",
                        current_running_time,
                        rec_state.last_recording_start
                    );
                    HandleResult::Drop
                }
            }
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &Element,
        buffer: gst::Buffer,
    ) -> gst::FlowReturn {
        let stream = match self.pads.lock().unwrap().get(pad) {
            None => {
                gst_element_error!(
                    element,
                    gst::CoreError::Pad,
                    ["Unknown pad {:?}", pad.get_name()]
                );
                return gst::FlowReturn::Error;
            }
            Some(stream) => stream.clone(),
        };

        gst_log!(self.cat, obj: pad, "Handling buffer {:?}", buffer);

        {
            let state = stream.state.lock().unwrap();
            if state.eos {
                return gst::FlowReturn::Eos;
            }
        }

        let handle_result = if stream != self.main_stream {
            let pts = buffer.get_pts();
            let dts = buffer.get_dts();
            if dts.is_some() && pts.is_some() && dts != pts {
                gst_element_error!(
                    element,
                    gst::StreamError::Format,
                    ["DTS != PTS not supported for secondary streams"]
                );
                return gst::FlowReturn::Error;
            }
            if !pts.is_some() {
                gst_element_error!(element, gst::StreamError::Format, ["Buffer without PTS"]);
                return gst::FlowReturn::Error;
            }
            if buffer.get_flags().contains(gst::BufferFlags::DELTA_UNIT) {
                gst_element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Delta-units not supported for secondary streams"]
                );
                return gst::FlowReturn::Error;
            }

            self.handle_secondary_stream(pad, &stream, pts, buffer.get_duration())
        } else {
            let dts_or_pts = buffer.get_dts_or_pts();
            if !dts_or_pts.is_some() {
                gst_element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Buffer without DTS or PTS"]
                );
                return gst::FlowReturn::Error;
            }

            self.handle_main_stream(
                element,
                pad,
                &stream,
                !buffer.get_flags().contains(gst::BufferFlags::DELTA_UNIT),
                dts_or_pts,
                buffer.get_duration(),
            )
        };

        match handle_result {
            HandleResult::Drop => {
                return gst::FlowReturn::Ok;
            }
            HandleResult::Flushing => {
                return gst::FlowReturn::Flushing;
            }
            HandleResult::Eos => {
                stream.srcpad.push_event(
                    gst::Event::new_eos()
                        .seqnum(stream.state.lock().unwrap().segment_seqnum)
                        .build(),
                );
                return gst::FlowReturn::Eos;
            }
            HandleResult::Pass => {
                // Pass through and actually push the buffer
            }
        }

        let out_running_time = {
            let mut state = stream.state.lock().unwrap();
            let mut events = Vec::with_capacity(state.pending_events.len() + 1);

            if state.segment_pending {
                let rec_state = self.state.lock().unwrap();

                // Adjust so that last_recording_start has running time of
                // recording_duration

                state.out_segment = state.in_segment.clone();
                let offset = rec_state.running_time_offset.unwrap_or(0);
                let res = state.out_segment.offset_running_time(-(offset as i64));
                assert!(res);
                events.push(
                    gst::Event::new_segment(&state.out_segment)
                        .seqnum(state.segment_seqnum)
                        .build(),
                );
                state.segment_pending = false;
                gst_debug!(
                    self.cat,
                    obj: pad,
                    "Pending Segment {:?}",
                    &state.out_segment
                );
            }

            if !state.pending_events.is_empty() {
                gst_debug!(self.cat, obj: pad, "Pushing pending events");
            }

            events.append(&mut state.pending_events);

            let out_running_time = state.out_segment.to_running_time(buffer.get_pts());

            // Unlock before pushing
            drop(state);

            for e in events.drain(..) {
                stream.srcpad.push_event(e);
            }

            out_running_time
        };

        gst_log!(
            self.cat,
            obj: pad,
            "Pushing buffer with running time {}: {:?}",
            out_running_time,
            buffer
        );
        stream.srcpad.push(buffer)
    }

    fn sink_event(&self, pad: &gst::Pad, element: &Element, event: gst::Event) -> bool {
        use gst::EventView;

        let stream = match self.pads.lock().unwrap().get(pad) {
            None => {
                gst_element_error!(
                    element,
                    gst::CoreError::Pad,
                    ["Unknown pad {:?}", pad.get_name()]
                );
                return false;
            }
            Some(stream) => stream.clone(),
        };

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        let mut forward = true;
        let mut send_pending = false;

        match event.view() {
            EventView::FlushStart(..) => {
                let _main_state = if stream != self.main_stream {
                    Some(self.main_stream.state.lock().unwrap())
                } else {
                    None
                };
                let mut state = stream.state.lock().unwrap();

                state.flushing = true;
                self.main_stream_cond.notify_all();
            }
            EventView::FlushStop(..) => {
                let mut state = stream.state.lock().unwrap();

                state.eos = false;
                state.flushing = false;
                state.segment_pending = false;
                state.current_running_time = gst::CLOCK_TIME_NONE;
            }
            EventView::Segment(e) => {
                let mut state = stream.state.lock().unwrap();

                let segment = match e.get_segment().downcast::<gst::ClockTime>() {
                    Err(segment) => {
                        gst_element_error!(
                            element,
                            gst::StreamError::Format,
                            [
                                "Only Time segments supported, got {:?}",
                                segment.get_format()
                            ]
                        );
                        return false;
                    }
                    Ok(segment) => segment,
                };

                if (segment.get_rate() - 1.0).abs() > f64::EPSILON {
                    gst_element_error!(
                        element,
                        gst::StreamError::Format,
                        [
                            "Only rate==1.0 segments supported, got {:?}",
                            segment.get_rate()
                        ]
                    );
                    return false;
                }

                state.in_segment = segment;
                state.segment_seqnum = event.get_seqnum();
                state.segment_pending = true;
                state.current_running_time = gst::CLOCK_TIME_NONE;

                gst_debug!(self.cat, obj: pad, "Got new Segment {:?}", state.in_segment);

                forward = false;
            }
            EventView::Gap(e) => {
                gst_debug!(self.cat, obj: pad, "Handling Gap event {:?}", event);
                let (pts, duration) = e.get();
                let handle_result = if stream == self.main_stream {
                    self.handle_main_stream(element, pad, &stream, false, pts, duration)
                } else {
                    self.handle_secondary_stream(pad, &stream, pts, duration)
                };

                forward = handle_result == HandleResult::Pass;
            }
            EventView::Eos(..) => {
                let _main_state = if stream != self.main_stream {
                    Some(self.main_stream.state.lock().unwrap())
                } else {
                    None
                };
                let mut state = stream.state.lock().unwrap();

                state.eos = true;
                self.main_stream_cond.notify_all();
                gst_debug!(
                    self.cat,
                    obj: pad,
                    "Stream is EOS now, sending any pending events"
                );

                send_pending = true;
            }
            _ => (),
        };

        // If a serialized event and coming after Segment and a new Segment is pending,
        // queue up and send at a later time (buffer/gap) after we sent the Segment
        let type_ = event.get_type();
        if forward && type_ != gst::EventType::Eos && type_.is_serialized()
            && type_.partial_cmp(&gst::EventType::Segment) == Some(cmp::Ordering::Greater)
        {
            let mut state = stream.state.lock().unwrap();
            if state.segment_pending {
                gst_log!(self.cat, obj: pad, "Storing event for later pushing");
                state.pending_events.push(event);
                return true;
            }
        }

        if send_pending {
            let mut state = stream.state.lock().unwrap();
            let mut events = Vec::with_capacity(state.pending_events.len() + 1);

            // Got not a single buffer on this stream before EOS, forward
            // the input segment
            if state.segment_pending {
                events.push(
                    gst::Event::new_segment(&state.in_segment)
                        .seqnum(state.segment_seqnum)
                        .build(),
                );
            }
            events.append(&mut state.pending_events);
            drop(state);

            for e in events.drain(..) {
                stream.srcpad.push_event(e);
            }
        }

        if forward {
            gst_log!(self.cat, obj: pad, "Forwarding event {:?}", event);
            stream.srcpad.push_event(event)
        } else {
            gst_log!(self.cat, obj: pad, "Dropping event {:?}", event);
            true
        }
    }

    fn sink_query(&self, pad: &gst::Pad, element: &Element, query: &mut gst::QueryRef) -> bool {
        let stream = match self.pads.lock().unwrap().get(pad) {
            None => {
                gst_element_error!(
                    element,
                    gst::CoreError::Pad,
                    ["Unknown pad {:?}", pad.get_name()]
                );
                return false;
            }
            Some(stream) => stream.clone(),
        };

        gst_log!(self.cat, obj: pad, "Handling query {:?}", query);

        stream.srcpad.peer_query(query)
    }

    fn src_event(&self, pad: &gst::Pad, element: &Element, mut event: gst::Event) -> bool {
        use gst::EventView;

        let stream = match self.pads.lock().unwrap().get(pad) {
            None => {
                gst_element_error!(
                    element,
                    gst::CoreError::Pad,
                    ["Unknown pad {:?}", pad.get_name()]
                );
                return false;
            }
            Some(stream) => stream.clone(),
        };

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        let mut forward = true;
        match event.view() {
            EventView::Seek(..) => {
                forward = false;
            }
            _ => (),
        }

        let rec_state = self.state.lock().unwrap();
        let running_time_offset = rec_state.running_time_offset.unwrap_or(0) as i64;
        let offset = event.get_running_time_offset();
        event
            .make_mut()
            .set_running_time_offset(offset + running_time_offset);
        drop(rec_state);

        if forward {
            gst_log!(self.cat, obj: pad, "Forwarding event {:?}", event);
            stream.sinkpad.push_event(event)
        } else {
            gst_log!(self.cat, obj: pad, "Dropping event {:?}", event);
            false
        }
    }

    fn src_query(&self, pad: &gst::Pad, element: &Element, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        let stream = match self.pads.lock().unwrap().get(pad) {
            None => {
                gst_element_error!(
                    element,
                    gst::CoreError::Pad,
                    ["Unknown pad {:?}", pad.get_name()]
                );
                return false;
            }
            Some(stream) => stream.clone(),
        };

        gst_log!(self.cat, obj: pad, "Handling query {:?}", query);
        match query.view_mut() {
            QueryView::Scheduling(ref mut q) => {
                let mut new_query = gst::Query::new_scheduling();
                let res = stream.sinkpad.peer_query(new_query.get_mut().unwrap());
                if !res {
                    return res;
                }

                gst_log!(self.cat, obj: pad, "Downstream returned {:?}", new_query);

                match new_query.view() {
                    QueryView::Scheduling(ref n) => {
                        let (flags, min, max, align) = n.get_result();
                        q.set(flags, min, max, align);
                        q.add_scheduling_modes(&n.get_scheduling_modes()
                            .iter()
                            .cloned()
                            .filter(|m| m != &gst::PadMode::Pull)
                            .collect::<Vec<_>>());
                        gst_log!(self.cat, obj: pad, "Returning {:?}", q.get_mut_query());
                        return true;
                    }
                    _ => unreachable!(),
                }
            }
            QueryView::Seeking(ref mut q) => {
                // Seeking is not possible here
                let format = q.get_format();
                q.set(
                    false,
                    gst::GenericFormattedValue::new(format, -1),
                    gst::GenericFormattedValue::new(format, -1),
                );

                gst_log!(self.cat, obj: pad, "Returning {:?}", q.get_mut_query());
                return true;
            }
            // Position and duration is always the current recording position
            QueryView::Position(ref mut q) => if q.get_format() == gst::Format::Time {
                let state = stream.state.lock().unwrap();
                let rec_state = self.state.lock().unwrap();
                let mut recording_duration = rec_state.recording_duration;
                if rec_state.recording_state == RecordingState::Recording
                    || rec_state.recording_state == RecordingState::Stopping
                {
                    recording_duration +=
                        state.current_running_time - rec_state.last_recording_start;
                }
                q.set(recording_duration);
                return true;
            } else {
                return false;
            },
            QueryView::Duration(ref mut q) => if q.get_format() == gst::Format::Time {
                let state = stream.state.lock().unwrap();
                let rec_state = self.state.lock().unwrap();
                let mut recording_duration = rec_state.recording_duration;
                if rec_state.recording_state == RecordingState::Recording
                    || rec_state.recording_state == RecordingState::Stopping
                {
                    recording_duration +=
                        state.current_running_time - rec_state.last_recording_start;
                }
                q.set(recording_duration);
                return true;
            } else {
                return false;
            },
            _ => (),
        };

        gst_log!(self.cat, obj: pad, "Forwarding query {:?}", query);
        stream.sinkpad.peer_query(query)
    }

    fn iterate_internal_links(&self, pad: &gst::Pad, element: &Element) -> gst::Iterator<gst::Pad> {
        let stream = match self.pads.lock().unwrap().get(pad) {
            None => {
                gst_element_error!(
                    element,
                    gst::CoreError::Pad,
                    ["Unknown pad {:?}", pad.get_name()]
                );
                return gst::Iterator::from_vec(vec![]);
            }
            Some(stream) => stream.clone(),
        };

        if pad == &stream.srcpad {
            gst::Iterator::from_vec(vec![stream.sinkpad.clone()])
        } else {
            gst::Iterator::from_vec(vec![stream.srcpad.clone()])
        }
    }
}

impl ObjectImpl<Element> for ToggleRecord {
    fn set_property(&self, obj: &glib::Object, id: u32, value: &glib::Value) {
        let prop = &PROPERTIES[id as usize];
        let element = obj.clone().downcast::<Element>().unwrap();

        match *prop {
            Property::Boolean("record", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let record = value.get().unwrap();
                gst_debug!(
                    self.cat,
                    obj: &element,
                    "Setting record from {:?} to {:?}",
                    settings.record,
                    record
                );
                settings.record = record;
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::Boolean("record", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.record.to_value())
            }
            Property::Boolean("recording", ..) => {
                let rec_state = self.state.lock().unwrap();
                Ok((rec_state.recording_state == RecordingState::Recording).to_value())
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl<Element> for ToggleRecord {
    fn change_state(
        &self,
        element: &Element,
        transition: gst::StateChange,
    ) -> gst::StateChangeReturn {
        gst_trace!(self.cat, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                for s in self.other_streams
                    .lock()
                    .unwrap()
                    .0
                    .iter()
                    .chain(iter::once(&self.main_stream))
                {
                    let mut state = s.state.lock().unwrap();
                    *state = StreamState::default();
                }

                let mut rec_state = self.state.lock().unwrap();
                *rec_state = State::default();
            }
            gst::StateChange::PausedToReady => {
                for s in &self.other_streams.lock().unwrap().0 {
                    let mut state = s.state.lock().unwrap();
                    state.flushing = true;
                }

                let mut state = self.main_stream.state.lock().unwrap();
                state.flushing = true;
                self.main_stream_cond.notify_all();
            }
            _ => (),
        }

        let ret = element.parent_change_state(transition);
        if ret == gst::StateChangeReturn::Failure {
            return ret;
        }

        match transition {
            gst::StateChange::PausedToReady => {
                for s in self.other_streams
                    .lock()
                    .unwrap()
                    .0
                    .iter()
                    .chain(iter::once(&self.main_stream))
                {
                    let mut state = s.state.lock().unwrap();

                    state.pending_events.clear();
                }

                let mut rec_state = self.state.lock().unwrap();
                *rec_state = State::default();
                drop(rec_state);
                self.notify(&element.clone().upcast(), "recording");
            }
            _ => (),
        }

        ret
    }

    fn request_new_pad(
        &self,
        element: &Element,
        _templ: &gst::PadTemplate,
        _name: Option<String>,
        _caps: Option<&gst::CapsRef>,
    ) -> Option<gst::Pad> {
        let mut other_streams = self.other_streams.lock().unwrap();
        let (ref mut other_streams, ref mut pad_count) = *other_streams;
        let mut pads = self.pads.lock().unwrap();

        let id = *pad_count;
        *pad_count += 1;

        let templ = element.get_pad_template("sink_%u").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, format!("sink_{}", id).as_str());

        let templ = element.get_pad_template("src_%u").unwrap();
        let srcpad = gst::Pad::new_from_template(&templ, format!("src_{}", id).as_str());

        ToggleRecord::set_pad_functions(&sinkpad, &srcpad);

        sinkpad.set_active(true).unwrap();
        srcpad.set_active(true).unwrap();

        element.add_pad(&sinkpad).unwrap();
        element.add_pad(&srcpad).unwrap();

        let stream = Stream::new(sinkpad.clone(), srcpad);

        pads.insert(stream.sinkpad.clone(), stream.clone());
        pads.insert(stream.srcpad.clone(), stream.clone());

        other_streams.push(stream);

        Some(sinkpad)
    }

    fn release_pad(&self, element: &Element, pad: &gst::Pad) {
        let mut other_streams = self.other_streams.lock().unwrap();
        let (ref mut other_streams, _) = *other_streams;
        let mut pads = self.pads.lock().unwrap();

        let stream = match pads.get(pad) {
            None => return,
            Some(stream) => stream.clone(),
        };

        stream.srcpad.set_active(false).unwrap();
        stream.sinkpad.set_active(false).unwrap();

        element.remove_pad(&stream.sinkpad).unwrap();
        element.remove_pad(&stream.srcpad).unwrap();

        pads.remove(&stream.sinkpad).unwrap();
        pads.remove(&stream.srcpad).unwrap();

        // TODO: Replace with Vec::remove_item() once stable
        let pos = other_streams.iter().position(|x| *x == stream);
        pos.map(|pos| other_streams.swap_remove(pos));
    }
}

struct ToggleRecordStatic;

impl ImplTypeStatic<Element> for ToggleRecordStatic {
    fn get_name(&self) -> &str {
        "ToggleRecord"
    }

    fn new(&self, element: &Element) -> Box<ElementImpl<Element>> {
        ToggleRecord::init(element)
    }

    fn class_init(&self, klass: &mut ElementClass) {
        ToggleRecord::class_init(klass);
    }
}

pub fn register(plugin: &gst::Plugin) {
    let togglerecord_static = ToggleRecordStatic;
    let type_ = register_type(togglerecord_static);
    gst::Element::register(plugin, "togglerecord", 0, type_);
}
