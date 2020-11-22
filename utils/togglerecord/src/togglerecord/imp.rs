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

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_element_error, gst_log, gst_trace, gst_warning};

use more_asserts::{assert_ge, assert_le, assert_lt};

use once_cell::sync::Lazy;
use parking_lot::{Condvar, Mutex};
use std::cmp;
use std::collections::HashMap;
use std::f64;
use std::iter;
use std::sync::Arc;

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

static PROPERTIES: [subclass::Property; 2] = [
    subclass::Property("record", |name| {
        glib::ParamSpec::boolean(
            name,
            "Record",
            "Enable/disable recording",
            DEFAULT_RECORD,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("recording", |name| {
        glib::ParamSpec::boolean(
            name,
            "Recording",
            "Whether recording is currently taking place",
            DEFAULT_RECORD,
            glib::ParamFlags::READABLE,
        )
    }),
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
            sinkpad,
            srcpad,
            state: Arc::new(Mutex::new(StreamState::default())),
        }
    }
}

struct StreamState {
    in_segment: gst::FormattedSegment<gst::ClockTime>,
    out_segment: gst::FormattedSegment<gst::ClockTime>,
    segment_seqnum: gst::Seqnum,
    // Start/end running time of the current/last buffer
    current_running_time: gst::ClockTime,
    current_running_time_end: gst::ClockTime,
    eos: bool,
    flushing: bool,
    segment_pending: bool,
    discont_pending: bool,
    pending_events: Vec<gst::Event>,
    audio_info: Option<gst_audio::AudioInfo>,
    video_info: Option<gst_video::VideoInfo>,
}

impl Default for StreamState {
    fn default() -> Self {
        Self {
            in_segment: gst::FormattedSegment::new(),
            out_segment: gst::FormattedSegment::new(),
            segment_seqnum: gst::Seqnum::next(),
            current_running_time: gst::CLOCK_TIME_NONE,
            current_running_time_end: gst::CLOCK_TIME_NONE,
            eos: false,
            flushing: false,
            segment_pending: false,
            discont_pending: true,
            pending_events: Vec::new(),
            audio_info: None,
            video_info: None,
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
enum HandleResult<T> {
    Pass(T),
    Drop,
    Eos,
    Flushing,
}

trait HandleData: Sized {
    fn get_pts(&self) -> gst::ClockTime;
    fn get_dts(&self) -> gst::ClockTime;
    fn get_dts_or_pts(&self) -> gst::ClockTime {
        let dts = self.get_dts();
        if dts.is_some() {
            dts
        } else {
            self.get_pts()
        }
    }
    fn get_duration(&self, state: &StreamState) -> gst::ClockTime;
    fn is_keyframe(&self) -> bool;
    fn can_clip(&self, state: &StreamState) -> bool;
    fn clip(
        self,
        state: &StreamState,
        segment: &gst::FormattedSegment<gst::ClockTime>,
    ) -> Option<Self>;
}

impl HandleData for (gst::ClockTime, gst::ClockTime) {
    fn get_pts(&self) -> gst::ClockTime {
        self.0
    }

    fn get_dts(&self) -> gst::ClockTime {
        self.0
    }

    fn get_duration(&self, _state: &StreamState) -> gst::ClockTime {
        self.1
    }

    fn is_keyframe(&self) -> bool {
        true
    }

    fn can_clip(&self, _state: &StreamState) -> bool {
        true
    }

    fn clip(
        self,
        _state: &StreamState,
        segment: &gst::FormattedSegment<gst::ClockTime>,
    ) -> Option<Self> {
        let stop = if self.1.is_some() {
            self.0 + self.1
        } else {
            self.0
        };

        segment
            .clip(self.0, stop)
            .map(|(start, stop)| (start, stop - start))
    }
}

impl HandleData for gst::Buffer {
    fn get_pts(&self) -> gst::ClockTime {
        gst::BufferRef::get_pts(self)
    }

    fn get_dts(&self) -> gst::ClockTime {
        gst::BufferRef::get_dts(self)
    }

    fn get_duration(&self, state: &StreamState) -> gst::ClockTime {
        let duration = gst::BufferRef::get_duration(self);

        if duration.is_some() {
            duration
        } else if let Some(ref video_info) = state.video_info {
            if video_info.fps() != 0.into() {
                gst::SECOND
                    .mul_div_floor(
                        *video_info.fps().denom() as u64,
                        *video_info.fps().numer() as u64,
                    )
                    .unwrap_or(gst::CLOCK_TIME_NONE)
            } else {
                gst::CLOCK_TIME_NONE
            }
        } else if let Some(ref audio_info) = state.audio_info {
            if audio_info.bpf() == 0 || audio_info.rate() == 0 {
                return gst::CLOCK_TIME_NONE;
            }

            let size = self.get_size() as u64;
            let num_samples = size / audio_info.bpf() as u64;
            gst::SECOND
                .mul_div_floor(num_samples, audio_info.rate() as u64)
                .unwrap_or(gst::CLOCK_TIME_NONE)
        } else {
            gst::CLOCK_TIME_NONE
        }
    }

    fn is_keyframe(&self) -> bool {
        !gst::BufferRef::get_flags(self).contains(gst::BufferFlags::DELTA_UNIT)
    }

    fn can_clip(&self, state: &StreamState) -> bool {
        // Only do actual clipping for raw audio/video
        if let Some(ref audio_info) = state.audio_info {
            if audio_info.format() == gst_audio::AudioFormat::Unknown
                || audio_info.format() == gst_audio::AudioFormat::Encoded
                || audio_info.rate() == 0
                || audio_info.bpf() == 0
            {
                return false;
            }
        } else if let Some(ref video_info) = state.video_info {
            if video_info.format() == gst_video::VideoFormat::Unknown
                || video_info.format() == gst_video::VideoFormat::Encoded
                || self.get_dts_or_pts() != self.get_pts()
            {
                return false;
            }
        } else {
            return false;
        }

        true
    }

    fn clip(
        mut self,
        state: &StreamState,
        segment: &gst::FormattedSegment<gst::ClockTime>,
    ) -> Option<Self> {
        // Only do actual clipping for raw audio/video
        if !self.can_clip(state) {
            return Some(self);
        }

        let pts = HandleData::get_pts(&self);
        let duration = HandleData::get_duration(&self, state);
        let stop = if duration.is_some() {
            pts + duration
        } else {
            pts
        };

        if let Some(ref audio_info) = state.audio_info {
            gst_audio::audio_buffer_clip(
                self,
                segment.upcast_ref(),
                audio_info.rate(),
                audio_info.bpf(),
            )
        } else if state.video_info.is_some() {
            segment.clip(pts, stop).map(move |(start, stop)| {
                {
                    let buffer = self.make_mut();
                    buffer.set_pts(start);
                    buffer.set_dts(start);
                    buffer.set_duration(stop - start);
                }

                self
            })
        } else {
            unreachable!();
        }
    }
}

pub struct ToggleRecord {
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

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "togglerecord",
        gst::DebugColorFlags::empty(),
        Some("Toggle Record Element"),
    )
});

impl ToggleRecord {
    fn handle_main_stream<T: HandleData>(
        &self,
        element: &super::ToggleRecord,
        pad: &gst::Pad,
        stream: &Stream,
        data: T,
    ) -> Result<HandleResult<T>, gst::FlowError> {
        let mut state = stream.state.lock();

        let mut dts_or_pts = data.get_dts_or_pts();
        let duration = data.get_duration(&state);

        if !dts_or_pts.is_some() {
            gst_element_error!(
                element,
                gst::StreamError::Format,
                ["Buffer without DTS or PTS"]
            );
            return Err(gst::FlowError::Error);
        }

        let mut dts_or_pts_end = if duration.is_some() {
            dts_or_pts + duration
        } else {
            dts_or_pts
        };

        let data = match data.clip(&state, &state.in_segment) {
            None => {
                gst_log!(CAT, obj: pad, "Dropping raw data outside segment");
                return Ok(HandleResult::Drop);
            }
            Some(data) => data,
        };

        // This will only do anything for non-raw data
        dts_or_pts = state.in_segment.get_start().max(dts_or_pts).unwrap();
        dts_or_pts_end = state.in_segment.get_start().max(dts_or_pts_end).unwrap();
        if state.in_segment.get_stop().is_some() {
            dts_or_pts = state.in_segment.get_stop().min(dts_or_pts).unwrap();
            dts_or_pts_end = state.in_segment.get_stop().min(dts_or_pts_end).unwrap();
        }

        let current_running_time = state.in_segment.to_running_time(dts_or_pts);
        let current_running_time_end = state.in_segment.to_running_time(dts_or_pts_end);
        state.current_running_time = current_running_time
            .max(state.current_running_time)
            .unwrap_or(current_running_time);
        state.current_running_time_end = current_running_time_end
            .max(state.current_running_time_end)
            .unwrap_or(current_running_time_end);

        // Wake up everybody, we advanced a bit
        // Important: They will only be able to advance once we're done with this
        // function or waiting for them to catch up below, otherwise they might
        // get the wrong state
        self.main_stream_cond.notify_all();

        gst_log!(
            CAT,
            obj: pad,
            "Main stream current running time {}-{} (position: {}-{})",
            current_running_time,
            current_running_time_end,
            dts_or_pts,
            dts_or_pts_end
        );

        let settings = *self.settings.lock();

        // First check if we have to update our recording state
        let mut rec_state = self.state.lock();
        let settings_changed = match rec_state.recording_state {
            RecordingState::Recording if !settings.record => {
                gst_debug!(CAT, obj: pad, "Stopping recording");
                rec_state.recording_state = RecordingState::Stopping;
                true
            }
            RecordingState::Stopped if settings.record => {
                gst_debug!(CAT, obj: pad, "Starting recording");
                rec_state.recording_state = RecordingState::Starting;
                true
            }
            _ => false,
        };

        match rec_state.recording_state {
            RecordingState::Recording => {
                // Remember where we stopped last, in case of EOS
                rec_state.last_recording_stop = current_running_time_end;
                gst_log!(CAT, obj: pad, "Passing buffer (recording)");
                Ok(HandleResult::Pass(data))
            }
            RecordingState::Stopping => {
                if !data.is_keyframe() {
                    // Remember where we stopped last, in case of EOS
                    rec_state.last_recording_stop = current_running_time_end;
                    gst_log!(CAT, obj: pad, "Passing non-keyframe buffer (stopping)");

                    drop(rec_state);
                    drop(state);
                    if settings_changed {
                        gst_debug!(CAT, obj: pad, "Requesting a new keyframe");
                        stream
                            .sinkpad
                            .push_event(gst_video::UpstreamForceKeyUnitEvent::builder().build());
                    }

                    return Ok(HandleResult::Pass(data));
                }

                // Remember the time when we stopped: now, i.e. right before the current buffer!
                rec_state.last_recording_stop = current_running_time;
                gst_debug!(
                    CAT,
                    obj: pad,
                    "Stopping at {}, started at {}, current duration {}, previous accumulated recording duration {}",
                    rec_state.last_recording_stop,
                    rec_state.last_recording_start,
                    rec_state.last_recording_stop - rec_state.last_recording_start,
                    rec_state.recording_duration
                );

                // Then unlock and wait for all other streams to reach a buffer that is completely
                // after/at the recording stop position (i.e. can be dropped completely) or go EOS
                // instead.
                drop(rec_state);

                while !self.other_streams.lock().0.iter().all(|s| {
                    let s = s.state.lock();
                    s.eos
                        || (s.current_running_time.is_some()
                            && s.current_running_time >= current_running_time)
                }) {
                    gst_log!(CAT, obj: pad, "Waiting for other streams to stop");
                    self.main_stream_cond.wait(&mut state);
                }

                if state.flushing {
                    gst_debug!(CAT, obj: pad, "Flushing");
                    return Ok(HandleResult::Flushing);
                }

                let mut rec_state = self.state.lock();
                rec_state.recording_state = RecordingState::Stopped;
                let advance_by = rec_state.last_recording_stop - rec_state.last_recording_start;
                rec_state.recording_duration += advance_by;
                rec_state.last_recording_start = gst::CLOCK_TIME_NONE;
                rec_state.last_recording_stop = gst::CLOCK_TIME_NONE;

                gst_debug!(
                    CAT,
                    obj: pad,
                    "Stopped at {}, recording duration {}",
                    current_running_time,
                    rec_state.recording_duration
                );

                // Then become Stopped and drop this buffer. We always stop right before
                // a keyframe
                gst_log!(CAT, obj: pad, "Dropping buffer (stopped)");

                drop(rec_state);
                drop(state);
                element.notify("recording");

                Ok(HandleResult::Drop)
            }
            RecordingState::Stopped => {
                gst_log!(CAT, obj: pad, "Dropping buffer (stopped)");
                Ok(HandleResult::Drop)
            }
            RecordingState::Starting => {
                // If this is no keyframe, we can directly go out again here and drop the frame
                if !data.is_keyframe() {
                    gst_log!(CAT, obj: pad, "Dropping non-keyframe buffer (starting)");

                    drop(rec_state);
                    drop(state);
                    if settings_changed {
                        gst_debug!(CAT, obj: pad, "Requesting a new keyframe");
                        stream
                            .sinkpad
                            .push_event(gst_video::UpstreamForceKeyUnitEvent::builder().build());
                    }

                    return Ok(HandleResult::Drop);
                }

                // Remember the time when we started: now!
                rec_state.last_recording_start = current_running_time;
                rec_state.running_time_offset = current_running_time - rec_state.recording_duration;
                gst_debug!(
                    CAT,
                    obj: pad,
                    "Starting at {}, previous accumulated recording duration {}",
                    current_running_time,
                    rec_state.recording_duration,
                );

                state.segment_pending = true;
                state.discont_pending = true;
                for other_stream in &self.other_streams.lock().0 {
                    let mut other_state = other_stream.state.lock();
                    other_state.segment_pending = true;
                    other_state.discont_pending = true;
                }

                // Then unlock and wait for all other streams to reach a buffer that is completely
                // after/at the recording start position (i.e. can be passed through completely) or
                // go EOS instead.
                drop(rec_state);

                while !self.other_streams.lock().0.iter().all(|s| {
                    let s = s.state.lock();
                    s.eos
                        || (s.current_running_time.is_some()
                            && s.current_running_time >= current_running_time)
                }) {
                    gst_log!(CAT, obj: pad, "Waiting for other streams to start");
                    self.main_stream_cond.wait(&mut state);
                }

                if state.flushing {
                    gst_debug!(CAT, obj: pad, "Flushing");
                    return Ok(HandleResult::Flushing);
                }

                let mut rec_state = self.state.lock();
                rec_state.recording_state = RecordingState::Recording;
                gst_debug!(
                    CAT,
                    obj: pad,
                    "Started at {}, recording duration {}",
                    current_running_time,
                    rec_state.recording_duration
                );

                gst_log!(CAT, obj: pad, "Passing buffer (recording)");

                drop(rec_state);
                drop(state);
                element.notify("recording");

                Ok(HandleResult::Pass(data))
            }
        }
    }

    fn handle_secondary_stream<T: HandleData>(
        &self,
        element: &super::ToggleRecord,
        pad: &gst::Pad,
        stream: &Stream,
        data: T,
    ) -> Result<HandleResult<T>, gst::FlowError> {
        // Calculate end pts & current running time and make sure we stay in the segment
        let mut state = stream.state.lock();

        let mut pts = data.get_pts();
        let duration = data.get_duration(&state);

        if pts.is_none() {
            gst_element_error!(element, gst::StreamError::Format, ["Buffer without PTS"]);
            return Err(gst::FlowError::Error);
        }

        let dts = data.get_dts();
        if dts.is_some() && pts.is_some() && dts != pts {
            gst_element_error!(
                element,
                gst::StreamError::Format,
                ["DTS != PTS not supported for secondary streams"]
            );
            return Err(gst::FlowError::Error);
        }

        if !data.is_keyframe() {
            gst_element_error!(
                element,
                gst::StreamError::Format,
                ["Delta-units not supported for secondary streams"]
            );
            return Err(gst::FlowError::Error);
        }

        let mut pts_end = if duration.is_some() {
            pts + duration
        } else {
            pts
        };

        let data = match data.clip(&state, &state.in_segment) {
            None => {
                gst_log!(CAT, obj: pad, "Dropping raw data outside segment");
                return Ok(HandleResult::Drop);
            }
            Some(data) => data,
        };

        // This will only do anything for non-raw data
        pts = state.in_segment.get_start().max(pts).unwrap();
        pts_end = state.in_segment.get_start().max(pts_end).unwrap();
        if state.in_segment.get_stop().is_some() {
            pts = state.in_segment.get_stop().min(pts).unwrap();
            pts_end = state.in_segment.get_stop().min(pts_end).unwrap();
        }

        let current_running_time = state.in_segment.to_running_time(pts);
        let current_running_time_end = state.in_segment.to_running_time(pts_end);
        state.current_running_time = current_running_time
            .max(state.current_running_time)
            .unwrap_or(current_running_time);
        state.current_running_time_end = current_running_time_end
            .max(state.current_running_time_end)
            .unwrap_or(current_running_time_end);

        gst_log!(
            CAT,
            obj: pad,
            "Secondary stream current running time {}-{} (position: {}-{}",
            current_running_time,
            current_running_time_end,
            pts,
            pts_end
        );

        drop(state);

        let mut main_state = self.main_stream.state.lock();

        // Wake up, in case the main stream is waiting for us to progress up to here. We progressed
        // above but all notifying must happen while the main_stream state is locked as per above.
        self.main_stream_cond.notify_all();

        let mut rec_state = self.state.lock();

        // Wait until the main stream advanced completely past our current running time in
        // Recording/Stopped modes to make sure we're not already outputting/dropping data that
        // should actually be dropped/output if recording is started/stopped now.
        //
        // In Starting/Stopping mode we wait if we the start of this buffer is after last recording
        // start/stop as in that case we should be in Recording/Stopped mode already. The main
        // stream is waiting for us to reach that position to switch to Recording/Stopped mode so
        // that in those modes we only have to pass through/drop the whole buffers.
        while (main_state.current_running_time.is_none()
            || rec_state.recording_state != RecordingState::Starting
                && rec_state.recording_state != RecordingState::Stopping
                && main_state.current_running_time_end < current_running_time_end
            || rec_state.recording_state == RecordingState::Starting
                && (rec_state.last_recording_start.is_none()
                    || rec_state.last_recording_start <= current_running_time)
            || rec_state.recording_state == RecordingState::Stopping
                && (rec_state.last_recording_stop.is_none()
                    || rec_state.last_recording_stop <= current_running_time))
            && !main_state.eos
            && !stream.state.lock().flushing
        {
            gst_log!(
                CAT,
                obj: pad,
                "Waiting at {}-{} in {:?} state, main stream at {}-{}",
                current_running_time,
                current_running_time_end,
                rec_state.recording_state,
                main_state.current_running_time,
                main_state.current_running_time_end
            );

            drop(rec_state);
            self.main_stream_cond.wait(&mut main_state);
            rec_state = self.state.lock();
        }

        state = stream.state.lock();

        if state.flushing {
            gst_debug!(CAT, obj: pad, "Flushing");
            return Ok(HandleResult::Flushing);
        }

        // If the main stream is EOS, we are also EOS unless we are
        // before the final last recording stop running time
        if main_state.eos {
            // If we have no start or stop position (we never recorded) then we're EOS too now
            if rec_state.last_recording_stop.is_none() || rec_state.last_recording_start.is_none() {
                gst_debug!(CAT, obj: pad, "Main stream EOS and recording never started",);
                return Ok(HandleResult::Eos);
            } else if data.can_clip(&*state)
                && current_running_time < rec_state.last_recording_start
                && current_running_time_end > rec_state.last_recording_start
            {
                // Otherwise if we're before the recording start but the end of the buffer is after
                // the start and we can clip, clip the buffer and pass it onwards.
                gst_debug!(
                        CAT,
                        obj: pad,
                        "Main stream EOS and we're not EOS yet (overlapping recording start, {} < {} < {})",
                        current_running_time,
                        rec_state.last_recording_start,
                        current_running_time_end
                    );

                let mut clip_start = state
                    .in_segment
                    .position_from_running_time(rec_state.last_recording_start);
                if clip_start.is_none() {
                    clip_start = state.in_segment.get_start();
                }
                let mut clip_stop = state
                    .in_segment
                    .position_from_running_time(rec_state.last_recording_stop);
                if clip_stop.is_none() {
                    clip_stop = state.in_segment.get_stop();
                }
                let mut segment = state.in_segment.clone();
                segment.set_start(clip_start);
                segment.set_stop(clip_stop);

                gst_log!(CAT, obj: pad, "Clipping to segment {:?}", segment,);

                if let Some(data) = data.clip(&*state, &segment) {
                    return Ok(HandleResult::Pass(data));
                } else {
                    gst_warning!(CAT, obj: pad, "Complete buffer clipped!");
                    return Ok(HandleResult::Drop);
                }
            } else if current_running_time < rec_state.last_recording_start {
                // Otherwise if the buffer starts before the recording start, drop it. This
                // means that we either can't clip, or that the end is also before the
                // recording start
                gst_debug!(
                    CAT,
                    obj: pad,
                    "Main stream EOS and we're not EOS yet (before recording start, {} < {})",
                    current_running_time,
                    rec_state.last_recording_start
                );
                return Ok(HandleResult::Drop);
            } else if data.can_clip(&*state)
                && current_running_time < rec_state.last_recording_stop
                && current_running_time_end > rec_state.last_recording_stop
            {
                // Similarly if the end is after the recording stop but the start is before and we
                // can clip, clip the buffer and pass it through.
                gst_debug!(
                        CAT,
                        obj: pad,
                        "Main stream EOS and we're not EOS yet (overlapping recording end, {} < {} < {})",
                        current_running_time,
                        rec_state.last_recording_stop,
                        current_running_time_end
                    );

                let mut clip_start = state
                    .in_segment
                    .position_from_running_time(rec_state.last_recording_start);
                if clip_start.is_none() {
                    clip_start = state.in_segment.get_start();
                }
                let mut clip_stop = state
                    .in_segment
                    .position_from_running_time(rec_state.last_recording_stop);
                if clip_stop.is_none() {
                    clip_stop = state.in_segment.get_stop();
                }
                let mut segment = state.in_segment.clone();
                segment.set_start(clip_start);
                segment.set_stop(clip_stop);

                gst_log!(CAT, obj: pad, "Clipping to segment {:?}", segment,);

                if let Some(data) = data.clip(&*state, &segment) {
                    return Ok(HandleResult::Pass(data));
                } else {
                    gst_warning!(CAT, obj: pad, "Complete buffer clipped!");
                    return Ok(HandleResult::Eos);
                }
            } else if current_running_time_end > rec_state.last_recording_stop {
                // Otherwise if the end of the buffer is after the recording stop, we're EOS
                // now. This means that we either couldn't clip or that the start is also after
                // the recording stop
                gst_debug!(
                    CAT,
                    obj: pad,
                    "Main stream EOS and we're EOS too (after recording end, {} > {})",
                    current_running_time_end,
                    rec_state.last_recording_stop
                );
                return Ok(HandleResult::Eos);
            } else {
                // In all other cases the buffer is fully between recording start and end and
                // can be passed through as is
                assert_ge!(current_running_time, rec_state.last_recording_start);
                assert_le!(current_running_time_end, rec_state.last_recording_stop);

                gst_debug!(
                    CAT,
                    obj: pad,
                    "Main stream EOS and we're not EOS yet (before recording end, {} <= {} <= {})",
                    rec_state.last_recording_start,
                    current_running_time,
                    rec_state.last_recording_stop
                );
                return Ok(HandleResult::Pass(data));
            }
        }

        match rec_state.recording_state {
            RecordingState::Recording => {
                // The end of our buffer must be before/at the end of the previous buffer of the main
                // stream
                assert_le!(
                    current_running_time_end,
                    main_state.current_running_time_end
                );

                // We're properly started, must have a start position and
                // be actually after that start position
                assert!(rec_state.last_recording_start.is_some());
                assert_ge!(current_running_time, rec_state.last_recording_start);
                gst_log!(CAT, obj: pad, "Passing buffer (recording)");
                Ok(HandleResult::Pass(data))
            }
            RecordingState::Stopping => {
                // The start of our buffer must be before the last recording stop as
                // otherwise we would be in Stopped state already
                assert_lt!(current_running_time, rec_state.last_recording_stop);

                // If we have no start position yet, the main stream is waiting for a key-frame
                if rec_state.last_recording_stop.is_none() {
                    gst_log!(
                        CAT,
                        obj: pad,
                        "Passing buffer (stopping: waiting for keyframe)",
                    );
                    Ok(HandleResult::Pass(data))
                } else if current_running_time_end <= rec_state.last_recording_stop {
                    gst_log!(
                        CAT,
                        obj: pad,
                        "Passing buffer (stopping: {} <= {})",
                        current_running_time_end,
                        rec_state.last_recording_stop
                    );
                    Ok(HandleResult::Pass(data))
                } else if data.can_clip(&*state)
                    && current_running_time < rec_state.last_recording_stop
                    && current_running_time_end > rec_state.last_recording_stop
                {
                    gst_log!(
                        CAT,
                        obj: pad,
                        "Passing buffer (stopping: {} < {} < {})",
                        current_running_time,
                        rec_state.last_recording_stop,
                        current_running_time_end,
                    );

                    let mut clip_stop = state
                        .in_segment
                        .position_from_running_time(rec_state.last_recording_stop);
                    if clip_stop.is_none() {
                        clip_stop = state.in_segment.get_stop();
                    }
                    let mut segment = state.in_segment.clone();
                    segment.set_stop(clip_stop);

                    gst_log!(CAT, obj: pad, "Clipping to segment {:?}", segment,);

                    if let Some(data) = data.clip(&*state, &segment) {
                        Ok(HandleResult::Pass(data))
                    } else {
                        gst_warning!(CAT, obj: pad, "Complete buffer clipped!");
                        Ok(HandleResult::Drop)
                    }
                } else {
                    gst_log!(
                        CAT,
                        obj: pad,
                        "Dropping buffer (stopping: {} > {})",
                        current_running_time_end,
                        rec_state.last_recording_stop
                    );
                    Ok(HandleResult::Drop)
                }
            }
            RecordingState::Stopped => {
                // The end of our buffer must be before/at the end of the previous buffer of the main
                // stream
                assert_le!(
                    current_running_time_end,
                    main_state.current_running_time_end
                );

                // We're properly stopped
                gst_log!(CAT, obj: pad, "Dropping buffer (stopped)");
                Ok(HandleResult::Drop)
            }
            RecordingState::Starting => {
                // If we have no start position yet, the main stream is waiting for a key-frame
                if rec_state.last_recording_start.is_none() {
                    gst_log!(
                        CAT,
                        obj: pad,
                        "Dropping buffer (starting: waiting for keyframe)",
                    );
                    return Ok(HandleResult::Drop);
                }

                // The start of our buffer must be before the last recording start as
                // otherwise we would be in Recording state already
                assert_lt!(current_running_time, rec_state.last_recording_start);
                if current_running_time >= rec_state.last_recording_start {
                    gst_log!(
                        CAT,
                        obj: pad,
                        "Passing buffer (starting: {} >= {})",
                        current_running_time,
                        rec_state.last_recording_start
                    );
                    Ok(HandleResult::Pass(data))
                } else if data.can_clip(&*state)
                    && current_running_time < rec_state.last_recording_start
                    && current_running_time_end > rec_state.last_recording_start
                {
                    gst_log!(
                        CAT,
                        obj: pad,
                        "Passing buffer (starting: {} < {} < {})",
                        current_running_time,
                        rec_state.last_recording_start,
                        current_running_time_end,
                    );

                    let mut clip_start = state
                        .in_segment
                        .position_from_running_time(rec_state.last_recording_start);
                    if clip_start.is_none() {
                        clip_start = state.in_segment.get_start();
                    }
                    let mut segment = state.in_segment.clone();
                    segment.set_start(clip_start);

                    gst_log!(CAT, obj: pad, "Clipping to segment {:?}", segment,);

                    if let Some(data) = data.clip(&*state, &segment) {
                        Ok(HandleResult::Pass(data))
                    } else {
                        gst_warning!(CAT, obj: pad, "Complete buffer clipped!");
                        Ok(HandleResult::Drop)
                    }
                } else {
                    gst_log!(
                        CAT,
                        obj: pad,
                        "Dropping buffer (starting: {} < {})",
                        current_running_time,
                        rec_state.last_recording_start
                    );
                    Ok(HandleResult::Drop)
                }
            }
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::ToggleRecord,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let stream = self.pads.lock().get(pad).cloned().ok_or_else(|| {
            gst_element_error!(
                element,
                gst::CoreError::Pad,
                ["Unknown pad {:?}", pad.get_name()]
            );
            gst::FlowError::Error
        })?;

        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        {
            let state = stream.state.lock();
            if state.eos {
                return Err(gst::FlowError::Eos);
            }
        }

        let handle_result = if stream != self.main_stream {
            self.handle_secondary_stream(element, pad, &stream, buffer)
        } else {
            self.handle_main_stream(element, pad, &stream, buffer)
        }?;

        let mut buffer = match handle_result {
            HandleResult::Drop => {
                return Ok(gst::FlowSuccess::Ok);
            }
            HandleResult::Flushing => {
                return Err(gst::FlowError::Flushing);
            }
            HandleResult::Eos => {
                stream.srcpad.push_event(
                    gst::event::Eos::builder()
                        .seqnum(stream.state.lock().segment_seqnum)
                        .build(),
                );
                return Err(gst::FlowError::Eos);
            }
            HandleResult::Pass(buffer) => {
                // Pass through and actually push the buffer
                buffer
            }
        };

        let out_running_time = {
            let mut state = stream.state.lock();

            if state.discont_pending {
                gst_debug!(CAT, obj: pad, "Pending discont");
                let buffer = buffer.make_mut();
                buffer.set_flags(gst::BufferFlags::DISCONT);
                state.discont_pending = false;
            }

            let mut events = Vec::with_capacity(state.pending_events.len() + 1);

            if state.segment_pending {
                let rec_state = self.state.lock();

                // Adjust so that last_recording_start has running time of
                // recording_duration

                state.out_segment = state.in_segment.clone();
                let offset = rec_state.running_time_offset.unwrap_or(0);
                state
                    .out_segment
                    .offset_running_time(-(offset as i64))
                    .expect("Adjusting record duration");
                events.push(
                    gst::event::Segment::builder(&state.out_segment)
                        .seqnum(state.segment_seqnum)
                        .build(),
                );
                state.segment_pending = false;
                gst_debug!(CAT, obj: pad, "Pending Segment {:?}", &state.out_segment);
            }

            if !state.pending_events.is_empty() {
                gst_debug!(CAT, obj: pad, "Pushing pending events");
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
            CAT,
            obj: pad,
            "Pushing buffer with running time {}: {:?}",
            out_running_time,
            buffer
        );
        stream.srcpad.push(buffer)
    }

    fn sink_event(
        &self,
        pad: &gst::Pad,
        element: &super::ToggleRecord,
        mut event: gst::Event,
    ) -> bool {
        use gst::EventView;

        let stream = match self.pads.lock().get(pad) {
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

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        let mut forward = true;
        let mut send_pending = false;

        match event.view() {
            EventView::FlushStart(..) => {
                let _main_state = if stream != self.main_stream {
                    Some(self.main_stream.state.lock())
                } else {
                    None
                };
                let mut state = stream.state.lock();

                state.flushing = true;
                self.main_stream_cond.notify_all();
            }
            EventView::FlushStop(..) => {
                let mut state = stream.state.lock();

                state.eos = false;
                state.flushing = false;
                state.segment_pending = true;
                state.discont_pending = true;
                state.current_running_time = gst::CLOCK_TIME_NONE;
                state.current_running_time_end = gst::CLOCK_TIME_NONE;
            }
            EventView::Caps(c) => {
                let mut state = stream.state.lock();
                let caps = c.get_caps();
                let s = caps.get_structure(0).unwrap();
                if s.get_name().starts_with("audio/") {
                    state.audio_info = gst_audio::AudioInfo::from_caps(caps).ok();
                    gst_log!(CAT, obj: pad, "Got audio caps {:?}", state.audio_info);
                    state.video_info = None;
                } else if s.get_name().starts_with("video/") {
                    state.audio_info = None;
                    state.video_info = gst_video::VideoInfo::from_caps(caps).ok();
                    gst_log!(CAT, obj: pad, "Got video caps {:?}", state.video_info);
                } else {
                    state.audio_info = None;
                    state.video_info = None;
                }
            }
            EventView::Segment(e) => {
                let mut state = stream.state.lock();

                let segment = match e.get_segment().clone().downcast::<gst::ClockTime>() {
                    Err(segment) => {
                        gst_element_error!(
                            element,
                            gst::StreamError::Format,
                            [
                                "Only Time segments supported, got {:?}",
                                segment.get_format(),
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
                            segment.get_rate(),
                        ]
                    );
                    return false;
                }

                state.in_segment = segment;
                state.segment_seqnum = event.get_seqnum();
                state.segment_pending = true;
                state.current_running_time = gst::CLOCK_TIME_NONE;
                state.current_running_time_end = gst::CLOCK_TIME_NONE;

                gst_debug!(CAT, obj: pad, "Got new Segment {:?}", state.in_segment);

                forward = false;
            }
            EventView::Gap(e) => {
                gst_debug!(CAT, obj: pad, "Handling Gap event {:?}", event);
                let (pts, duration) = e.get();
                let handle_result = if stream == self.main_stream {
                    self.handle_main_stream(element, pad, &stream, (pts, duration))
                } else {
                    self.handle_secondary_stream(element, pad, &stream, (pts, duration))
                };

                forward = match handle_result {
                    Ok(HandleResult::Pass((new_pts, new_duration))) if new_pts.is_some() => {
                        if new_pts != pts || new_duration != duration {
                            event = gst::event::Gap::new(new_pts, new_duration);
                        }
                        true
                    }
                    Ok(_) => false,
                    Err(_) => false,
                };
            }
            EventView::Eos(..) => {
                let _main_state = if stream != self.main_stream {
                    Some(self.main_stream.state.lock())
                } else {
                    None
                };
                let mut state = stream.state.lock();

                state.eos = true;
                self.main_stream_cond.notify_all();
                gst_debug!(
                    CAT,
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
        if forward
            && type_ != gst::EventType::Eos
            && type_.is_serialized()
            && type_.partial_cmp(&gst::EventType::Segment) == Some(cmp::Ordering::Greater)
        {
            let mut state = stream.state.lock();
            if state.segment_pending {
                gst_log!(CAT, obj: pad, "Storing event for later pushing");
                state.pending_events.push(event);
                return true;
            }
        }

        if send_pending {
            let mut state = stream.state.lock();
            let mut events = Vec::with_capacity(state.pending_events.len() + 1);

            // Got not a single buffer on this stream before EOS, forward
            // the input segment
            if state.segment_pending {
                events.push(
                    gst::event::Segment::builder(&state.in_segment)
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
            gst_log!(CAT, obj: pad, "Forwarding event {:?}", event);
            stream.srcpad.push_event(event)
        } else {
            gst_log!(CAT, obj: pad, "Dropping event {:?}", event);
            true
        }
    }

    fn sink_query(
        &self,
        pad: &gst::Pad,
        element: &super::ToggleRecord,
        query: &mut gst::QueryRef,
    ) -> bool {
        let stream = match self.pads.lock().get(pad) {
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

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);

        stream.srcpad.peer_query(query)
    }

    // FIXME `matches!` was introduced in rustc 1.42.0, current MSRV is 1.41.0
    // FIXME uncomment when CI can upgrade to 1.47.1
    //#[allow(clippy::match_like_matches_macro)]
    fn src_event(
        &self,
        pad: &gst::Pad,
        element: &super::ToggleRecord,
        mut event: gst::Event,
    ) -> bool {
        use gst::EventView;

        let stream = match self.pads.lock().get(pad) {
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

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        let forward = !matches!(event.view(), EventView::Seek(..));

        let rec_state = self.state.lock();
        let running_time_offset = rec_state.running_time_offset.unwrap_or(0) as i64;
        let offset = event.get_running_time_offset();
        event
            .make_mut()
            .set_running_time_offset(offset + running_time_offset);
        drop(rec_state);

        if forward {
            gst_log!(CAT, obj: pad, "Forwarding event {:?}", event);
            stream.sinkpad.push_event(event)
        } else {
            gst_log!(CAT, obj: pad, "Dropping event {:?}", event);
            false
        }
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        element: &super::ToggleRecord,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        let stream = match self.pads.lock().get(pad) {
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

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);
        match query.view_mut() {
            QueryView::Scheduling(ref mut q) => {
                let mut new_query = gst::query::Scheduling::new();
                let res = stream.sinkpad.peer_query(&mut new_query);
                if !res {
                    return res;
                }

                gst_log!(CAT, obj: pad, "Downstream returned {:?}", new_query);

                let (flags, min, max, align) = new_query.get_result();
                q.set(flags, min, max, align);
                q.add_scheduling_modes(
                    &new_query
                        .get_scheduling_modes()
                        .iter()
                        .cloned()
                        .filter(|m| m != &gst::PadMode::Pull)
                        .collect::<Vec<_>>(),
                );
                gst_log!(CAT, obj: pad, "Returning {:?}", q.get_mut_query());
                true
            }
            QueryView::Seeking(ref mut q) => {
                // Seeking is not possible here
                let format = q.get_format();
                q.set(
                    false,
                    gst::GenericFormattedValue::new(format, -1),
                    gst::GenericFormattedValue::new(format, -1),
                );

                gst_log!(CAT, obj: pad, "Returning {:?}", q.get_mut_query());
                true
            }
            // Position and duration is always the current recording position
            QueryView::Position(ref mut q) => {
                if q.get_format() == gst::Format::Time {
                    let state = stream.state.lock();
                    let rec_state = self.state.lock();
                    let mut recording_duration = rec_state.recording_duration;
                    if rec_state.recording_state == RecordingState::Recording
                        || rec_state.recording_state == RecordingState::Stopping
                    {
                        gst_debug!(
                            CAT,
                            obj: pad,
                            "Returning position {} = {} - ({} + {})",
                            recording_duration
                                + (state.current_running_time_end - rec_state.last_recording_start),
                            recording_duration,
                            state.current_running_time_end,
                            rec_state.last_recording_start
                        );
                        recording_duration +=
                            state.current_running_time_end - rec_state.last_recording_start;
                    } else {
                        gst_debug!(CAT, obj: pad, "Returning position {}", recording_duration,);
                    }
                    q.set(recording_duration);
                    true
                } else {
                    false
                }
            }
            QueryView::Duration(ref mut q) => {
                if q.get_format() == gst::Format::Time {
                    let state = stream.state.lock();
                    let rec_state = self.state.lock();
                    let mut recording_duration = rec_state.recording_duration;
                    if rec_state.recording_state == RecordingState::Recording
                        || rec_state.recording_state == RecordingState::Stopping
                    {
                        gst_debug!(
                            CAT,
                            obj: pad,
                            "Returning duration {} = {} - ({} + {})",
                            recording_duration
                                + (state.current_running_time_end - rec_state.last_recording_start),
                            recording_duration,
                            state.current_running_time_end,
                            rec_state.last_recording_start
                        );
                        recording_duration +=
                            state.current_running_time_end - rec_state.last_recording_start;
                    } else {
                        gst_debug!(CAT, obj: pad, "Returning duration {}", recording_duration,);
                    }
                    q.set(recording_duration);
                    true
                } else {
                    false
                }
            }
            _ => {
                gst_log!(CAT, obj: pad, "Forwarding query {:?}", query);
                stream.sinkpad.peer_query(query)
            }
        }
    }

    fn iterate_internal_links(
        &self,
        pad: &gst::Pad,
        element: &super::ToggleRecord,
    ) -> gst::Iterator<gst::Pad> {
        let stream = match self.pads.lock().get(pad) {
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
            gst::Iterator::from_vec(vec![stream.sinkpad])
        } else {
            gst::Iterator::from_vec(vec![stream.srcpad])
        }
    }
}

impl ObjectSubclass for ToggleRecord {
    const NAME: &'static str = "RsToggleRecord";
    type Type = super::ToggleRecord;
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib::glib_object_subclass!();

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                ToggleRecord::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |togglerecord, element| togglerecord.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                ToggleRecord::catch_panic_pad_function(
                    parent,
                    || false,
                    |togglerecord, element| togglerecord.sink_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                ToggleRecord::catch_panic_pad_function(
                    parent,
                    || false,
                    |togglerecord, element| togglerecord.sink_query(pad, element, query),
                )
            })
            .iterate_internal_links_function(|pad, parent| {
                ToggleRecord::catch_panic_pad_function(
                    parent,
                    || gst::Iterator::from_vec(vec![]),
                    |togglerecord, element| togglerecord.iterate_internal_links(pad, element),
                )
            })
            .build();

        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .event_function(|pad, parent, event| {
                ToggleRecord::catch_panic_pad_function(
                    parent,
                    || false,
                    |togglerecord, element| togglerecord.src_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                ToggleRecord::catch_panic_pad_function(
                    parent,
                    || false,
                    |togglerecord, element| togglerecord.src_query(pad, element, query),
                )
            })
            .iterate_internal_links_function(|pad, parent| {
                ToggleRecord::catch_panic_pad_function(
                    parent,
                    || gst::Iterator::from_vec(vec![]),
                    |togglerecord, element| togglerecord.iterate_internal_links(pad, element),
                )
            })
            .build();

        let main_stream = Stream::new(sinkpad, srcpad);

        let mut pads = HashMap::new();
        pads.insert(main_stream.sinkpad.clone(), main_stream.clone());
        pads.insert(main_stream.srcpad.clone(), main_stream.clone());

        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            main_stream,
            main_stream_cond: Condvar::new(),
            other_streams: Mutex::new((Vec::new(), 0)),
            pads: Mutex::new(pads),
        }
    }

    fn class_init(klass: &mut Self::Class) {
        klass.install_properties(&PROPERTIES);

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

        let src_pad_template = gst::PadTemplate::new(
            "src_%u",
            gst::PadDirection::Src,
            gst::PadPresence::Sometimes,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        let sink_pad_template = gst::PadTemplate::new(
            "sink_%u",
            gst::PadDirection::Sink,
            gst::PadPresence::Request,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);
    }
}

impl ObjectImpl for ToggleRecord {
    fn set_property(&self, obj: &Self::Type, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("record", ..) => {
                let mut settings = self.settings.lock();
                let record = value.get_some().expect("type checked upstream");
                gst_debug!(
                    CAT,
                    obj: obj,
                    "Setting record from {:?} to {:?}",
                    settings.record,
                    record
                );

                settings.record = record;
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &Self::Type, id: usize) -> glib::Value {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("record", ..) => {
                let settings = self.settings.lock();
                settings.record.to_value()
            }
            subclass::Property("recording", ..) => {
                let rec_state = self.state.lock();
                (rec_state.recording_state == RecordingState::Recording).to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.main_stream.sinkpad).unwrap();
        obj.add_pad(&self.main_stream.srcpad).unwrap();
    }
}

impl ElementImpl for ToggleRecord {
    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                for s in self
                    .other_streams
                    .lock()
                    .0
                    .iter()
                    .chain(iter::once(&self.main_stream))
                {
                    let mut state = s.state.lock();
                    *state = StreamState::default();
                }

                let mut rec_state = self.state.lock();
                *rec_state = State::default();
            }
            gst::StateChange::PausedToReady => {
                for s in &self.other_streams.lock().0 {
                    let mut state = s.state.lock();
                    state.flushing = true;
                }

                let mut state = self.main_stream.state.lock();
                state.flushing = true;
                self.main_stream_cond.notify_all();
            }
            _ => (),
        }

        let success = self.parent_change_state(element, transition)?;

        if transition == gst::StateChange::PausedToReady {
            for s in self
                .other_streams
                .lock()
                .0
                .iter()
                .chain(iter::once(&self.main_stream))
            {
                let mut state = s.state.lock();

                state.pending_events.clear();
            }

            let mut rec_state = self.state.lock();
            *rec_state = State::default();
            drop(rec_state);
            element.notify("recording");
        }

        Ok(success)
    }

    fn request_new_pad(
        &self,
        element: &Self::Type,
        _templ: &gst::PadTemplate,
        _name: Option<String>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let mut other_streams_guard = self.other_streams.lock();
        let (ref mut other_streams, ref mut pad_count) = *other_streams_guard;
        let mut pads = self.pads.lock();

        let id = *pad_count;
        *pad_count += 1;

        let templ = element.get_pad_template("sink_%u").unwrap();
        let sinkpad =
            gst::Pad::builder_with_template(&templ, Some(format!("sink_{}", id).as_str()))
                .chain_function(|pad, parent, buffer| {
                    ToggleRecord::catch_panic_pad_function(
                        parent,
                        || Err(gst::FlowError::Error),
                        |togglerecord, element| togglerecord.sink_chain(pad, element, buffer),
                    )
                })
                .event_function(|pad, parent, event| {
                    ToggleRecord::catch_panic_pad_function(
                        parent,
                        || false,
                        |togglerecord, element| togglerecord.sink_event(pad, element, event),
                    )
                })
                .query_function(|pad, parent, query| {
                    ToggleRecord::catch_panic_pad_function(
                        parent,
                        || false,
                        |togglerecord, element| togglerecord.sink_query(pad, element, query),
                    )
                })
                .iterate_internal_links_function(|pad, parent| {
                    ToggleRecord::catch_panic_pad_function(
                        parent,
                        || gst::Iterator::from_vec(vec![]),
                        |togglerecord, element| togglerecord.iterate_internal_links(pad, element),
                    )
                })
                .build();

        let templ = element.get_pad_template("src_%u").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some(format!("src_{}", id).as_str()))
            .event_function(|pad, parent, event| {
                ToggleRecord::catch_panic_pad_function(
                    parent,
                    || false,
                    |togglerecord, element| togglerecord.src_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                ToggleRecord::catch_panic_pad_function(
                    parent,
                    || false,
                    |togglerecord, element| togglerecord.src_query(pad, element, query),
                )
            })
            .iterate_internal_links_function(|pad, parent| {
                ToggleRecord::catch_panic_pad_function(
                    parent,
                    || gst::Iterator::from_vec(vec![]),
                    |togglerecord, element| togglerecord.iterate_internal_links(pad, element),
                )
            })
            .build();

        sinkpad.set_active(true).unwrap();
        srcpad.set_active(true).unwrap();

        let stream = Stream::new(sinkpad.clone(), srcpad.clone());

        pads.insert(stream.sinkpad.clone(), stream.clone());
        pads.insert(stream.srcpad.clone(), stream.clone());

        other_streams.push(stream);

        drop(pads);
        drop(other_streams_guard);

        element.add_pad(&sinkpad).unwrap();
        element.add_pad(&srcpad).unwrap();

        Some(sinkpad)
    }

    fn release_pad(&self, element: &Self::Type, pad: &gst::Pad) {
        let mut other_streams_guard = self.other_streams.lock();
        let (ref mut other_streams, _) = *other_streams_guard;
        let mut pads = self.pads.lock();

        let stream = match pads.get(pad) {
            None => return,
            Some(stream) => stream.clone(),
        };

        stream.srcpad.set_active(false).unwrap();
        stream.sinkpad.set_active(false).unwrap();

        pads.remove(&stream.sinkpad).unwrap();
        pads.remove(&stream.srcpad).unwrap();

        // TODO: Replace with Vec::remove_item() once stable
        let pos = other_streams.iter().position(|x| *x == stream);
        pos.map(|pos| other_streams.swap_remove(pos));

        drop(pads);
        drop(other_streams_guard);

        element.remove_pad(&stream.sinkpad).unwrap();
        element.remove_pad(&stream.srcpad).unwrap();
    }
}
