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

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_log, gst_trace, gst_warning};

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
    current_running_time: Option<gst::ClockTime>,
    current_running_time_end: Option<gst::ClockTime>,
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
            current_running_time: None,
            current_running_time_end: None,
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
    last_recording_start: Option<gst::ClockTime>,
    last_recording_stop: Option<gst::ClockTime>,
    // Accumulated duration of previous recording segments,
    // updated whenever going to Stopped
    recording_duration: gst::ClockTime,
    // Updated whenever going to Recording
    running_time_offset: i64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            recording_state: RecordingState::Stopped,
            last_recording_start: None,
            last_recording_stop: None,
            recording_duration: gst::ClockTime::ZERO,
            running_time_offset: 0,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HandleResult<T> {
    Pass(T),
    Drop,
    Eos(bool),
    Flushing,
}

trait HandleData: Sized {
    fn pts(&self) -> Option<gst::ClockTime>;
    fn dts(&self) -> Option<gst::ClockTime>;
    fn dts_or_pts(&self) -> Option<gst::ClockTime> {
        let dts = self.dts();
        if dts.is_some() {
            dts
        } else {
            self.pts()
        }
    }
    fn duration(&self, state: &StreamState) -> Option<gst::ClockTime>;
    fn is_keyframe(&self) -> bool;
    fn can_clip(&self, state: &StreamState) -> bool;
    fn clip(
        self,
        state: &StreamState,
        segment: &gst::FormattedSegment<gst::ClockTime>,
    ) -> Option<Self>;
}

impl HandleData for (gst::ClockTime, Option<gst::ClockTime>) {
    fn pts(&self) -> Option<gst::ClockTime> {
        Some(self.0)
    }

    fn dts(&self) -> Option<gst::ClockTime> {
        Some(self.0)
    }

    fn duration(&self, _state: &StreamState) -> Option<gst::ClockTime> {
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
        let stop = self.0 + self.1.unwrap_or(gst::ClockTime::ZERO);

        segment.clip(self.0, stop).map(|(start, stop)| {
            let start = start.expect("provided a defined value");
            (start, stop.map(|stop| stop - start))
        })
    }
}

impl HandleData for gst::Buffer {
    fn pts(&self) -> Option<gst::ClockTime> {
        gst::BufferRef::pts(self)
    }

    fn dts(&self) -> Option<gst::ClockTime> {
        gst::BufferRef::dts(self)
    }

    fn duration(&self, state: &StreamState) -> Option<gst::ClockTime> {
        let duration = gst::BufferRef::duration(self);

        if duration.is_some() {
            duration
        } else if let Some(ref video_info) = state.video_info {
            if video_info.fps() != 0.into() {
                gst::ClockTime::SECOND.mul_div_floor(
                    *video_info.fps().denom() as u64,
                    *video_info.fps().numer() as u64,
                )
            } else {
                gst::ClockTime::NONE
            }
        } else if let Some(ref audio_info) = state.audio_info {
            if audio_info.bpf() == 0 || audio_info.rate() == 0 {
                return gst::ClockTime::NONE;
            }

            let size = self.size() as u64;
            let num_samples = size / audio_info.bpf() as u64;
            gst::ClockTime::SECOND.mul_div_floor(num_samples, audio_info.rate() as u64)
        } else {
            gst::ClockTime::NONE
        }
    }

    fn is_keyframe(&self) -> bool {
        !gst::BufferRef::flags(self).contains(gst::BufferFlags::DELTA_UNIT)
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
                || self.dts_or_pts() != self.pts()
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

        let pts = HandleData::pts(&self);
        let duration = HandleData::duration(&self, state);
        let stop = pts.map(|pts| pts + duration.unwrap_or(gst::ClockTime::ZERO));

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
                    buffer.set_duration(
                        stop.zip(start)
                            .and_then(|(stop, start)| stop.checked_sub(start)),
                        // FIXME we could expect here
                    );
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

        let mut dts_or_pts = data.dts_or_pts().ok_or_else(|| {
            gst::element_error!(
                element,
                gst::StreamError::Format,
                ["Buffer without DTS or PTS"]
            );
            gst::FlowError::Error
        })?;

        let mut dts_or_pts_end = dts_or_pts + data.duration(&state).unwrap_or(gst::ClockTime::ZERO);

        let data = match data.clip(&state, &state.in_segment) {
            Some(data) => data,
            None => {
                gst_log!(CAT, obj: pad, "Dropping raw data outside segment");
                return Ok(HandleResult::Drop);
            }
        };

        // This will only do anything for non-raw data
        // FIXME comment why we can unwrap
        dts_or_pts = state.in_segment.start().unwrap().max(dts_or_pts);
        dts_or_pts_end = state.in_segment.start().unwrap().max(dts_or_pts_end);
        if let Some(stop) = state.in_segment.stop() {
            dts_or_pts = stop.min(dts_or_pts);
            dts_or_pts_end = stop.min(dts_or_pts_end);
        }

        let current_running_time = state.in_segment.to_running_time(dts_or_pts);
        let current_running_time_end = state.in_segment.to_running_time(dts_or_pts_end);
        state.current_running_time = current_running_time
            .zip(state.current_running_time)
            .map(|(cur_rt, state_rt)| cur_rt.max(state_rt))
            .or(current_running_time);
        state.current_running_time_end = current_running_time_end
            .zip(state.current_running_time_end)
            .map(|(cur_rt_end, state_rt_end)| cur_rt_end.max(state_rt_end))
            .or(current_running_time_end);

        // FIXME we should probably return if either current_running_time or current_running_time_end
        // are None at this point

        // Wake up everybody, we advanced a bit
        // Important: They will only be able to advance once we're done with this
        // function or waiting for them to catch up below, otherwise they might
        // get the wrong state
        self.main_stream_cond.notify_all();

        gst_log!(
            CAT,
            obj: pad,
            "Main stream current running time {}-{} (position: {}-{})",
            current_running_time.display(),
            current_running_time_end.display(),
            dts_or_pts,
            dts_or_pts_end,
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
                let last_recording_duration = rec_state
                    .last_recording_stop
                    .zip(rec_state.last_recording_start)
                    .and_then(|(stop, start)| stop.checked_sub(start));
                gst_debug!(
                    CAT,
                    obj: pad,
                    "Stopping at {}, started at {}, current duration {}, previous accumulated recording duration {}",
                    rec_state.last_recording_stop.display(),
                    rec_state.last_recording_start.display(),
                    last_recording_duration.display(),
                    rec_state.recording_duration
                );

                // Then unlock and wait for all other streams to reach a buffer that is completely
                // after/at the recording stop position (i.e. can be dropped completely) or go EOS
                // instead.
                drop(rec_state);

                while !state.flushing
                    && !self.other_streams.lock().0.iter().all(|s| {
                        let s = s.state.lock();
                        s.eos
                            || s.current_running_time
                                .zip(current_running_time)
                                .map_or(false, |(s_cur_rt, cur_rt)| s_cur_rt >= cur_rt)
                    })
                {
                    gst_log!(CAT, obj: pad, "Waiting for other streams to stop");
                    self.main_stream_cond.wait(&mut state);
                }

                if state.flushing {
                    gst_debug!(CAT, obj: pad, "Flushing");
                    return Ok(HandleResult::Flushing);
                }

                let mut rec_state = self.state.lock();
                rec_state.recording_state = RecordingState::Stopped;
                rec_state.recording_duration +=
                    last_recording_duration.unwrap_or(gst::ClockTime::ZERO);
                rec_state.last_recording_start = None;
                rec_state.last_recording_stop = None;

                gst_debug!(
                    CAT,
                    obj: pad,
                    "Stopped at {}, recording duration {}",
                    current_running_time.display(),
                    rec_state.recording_duration.display(),
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
                rec_state.running_time_offset =
                    current_running_time.map_or(0, |current_running_time| {
                        current_running_time
                            .saturating_sub(rec_state.recording_duration)
                            .nseconds()
                    }) as i64;
                gst_debug!(
                    CAT,
                    obj: pad,
                    "Starting at {}, previous accumulated recording duration {}",
                    current_running_time.display(),
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

                while !state.flushing
                    && !self.other_streams.lock().0.iter().all(|s| {
                        let s = s.state.lock();
                        s.eos
                            || s.current_running_time
                                .zip(current_running_time)
                                .map_or(false, |(s_cur_rt, cur_rt)| s_cur_rt >= cur_rt)
                    })
                {
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
                    current_running_time.display(),
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

        let mut pts = data.pts().ok_or_else(|| {
            gst::element_error!(element, gst::StreamError::Format, ["Buffer without PTS"]);
            gst::FlowError::Error
        })?;

        if data.dts().map_or(false, |dts| dts != pts) {
            gst::element_error!(
                element,
                gst::StreamError::Format,
                ["DTS != PTS not supported for secondary streams"]
            );
            return Err(gst::FlowError::Error);
        }

        if !data.is_keyframe() {
            gst::element_error!(
                element,
                gst::StreamError::Format,
                ["Delta-units not supported for secondary streams"]
            );
            return Err(gst::FlowError::Error);
        }

        let mut pts_end = pts + data.duration(&state).unwrap_or(gst::ClockTime::ZERO);

        let data = match data.clip(&state, &state.in_segment) {
            None => {
                gst_log!(CAT, obj: pad, "Dropping raw data outside segment");
                return Ok(HandleResult::Drop);
            }
            Some(data) => data,
        };

        // This will only do anything for non-raw data
        // FIXME comment why we can unwrap
        pts = state.in_segment.start().unwrap().max(pts);
        pts_end = state.in_segment.start().unwrap().max(pts_end);
        if let Some(stop) = state.in_segment.stop() {
            pts = stop.min(pts);
            pts_end = stop.min(pts_end);
        }

        let current_running_time = state.in_segment.to_running_time(pts);
        let current_running_time_end = state.in_segment.to_running_time(pts_end);
        state.current_running_time = current_running_time
            .zip(state.current_running_time)
            .map(|(cur_rt, state_rt)| cur_rt.max(state_rt))
            .or(current_running_time);
        state.current_running_time_end = current_running_time_end
            .zip(state.current_running_time_end)
            .map(|(cur_rt_end, state_rt_end)| cur_rt_end.max(state_rt_end))
            .or(current_running_time_end);

        gst_log!(
            CAT,
            obj: pad,
            "Secondary stream current running time {}-{} (position: {}-{}",
            current_running_time.display(),
            current_running_time_end.display(),
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
                && main_state
                    .current_running_time_end
                    .zip(current_running_time_end)
                    .map_or(false, |(main_rt_end, cur_rt_end)| main_rt_end < cur_rt_end)
            || rec_state.recording_state == RecordingState::Starting
                && rec_state
                    .last_recording_start
                    .map_or(true, |last_rec_start| {
                        current_running_time.map_or(false, |cur_rt| last_rec_start <= cur_rt)
                    })
            || rec_state.recording_state == RecordingState::Stopping
                && rec_state.last_recording_stop.map_or(true, |last_rec_stop| {
                    current_running_time.map_or(false, |cur_rt| last_rec_stop <= cur_rt)
                }))
            && !main_state.eos
            && !stream.state.lock().flushing
        {
            gst_log!(
                CAT,
                obj: pad,
                "Waiting at {}-{} in {:?} state, main stream at {}-{}",
                current_running_time.display(),
                current_running_time_end.display(),
                rec_state.recording_state,
                main_state.current_running_time.display(),
                main_state.current_running_time_end.display(),
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
                return Ok(HandleResult::Eos(self.check_and_update_eos(
                    pad,
                    stream,
                    &mut state,
                    &mut rec_state,
                )));
            }

            let last_recording_start = rec_state.last_recording_start.expect("recording started");

            // FIXME it would help a lot if we could expect current_running_time
            // and possibly current_running_time_end at some point.

            if data.can_clip(&*state)
                && current_running_time.map_or(false, |cur_rt| cur_rt < last_recording_start)
                && current_running_time_end
                    .map_or(false, |cur_rt_end| cur_rt_end > last_recording_start)
            {
                // Otherwise if we're before the recording start but the end of the buffer is after
                // the start and we can clip, clip the buffer and pass it onwards.
                gst_debug!(
                        CAT,
                        obj: pad,
                        "Main stream EOS and we're not EOS yet (overlapping recording start, {} < {} < {})",
                        current_running_time.display(),
                        last_recording_start,
                        current_running_time_end.display(),
                    );

                let mut clip_start = state
                    .in_segment
                    .position_from_running_time(rec_state.last_recording_start);
                if clip_start.is_none() {
                    clip_start = state.in_segment.start();
                }
                let mut clip_stop = state
                    .in_segment
                    .position_from_running_time(rec_state.last_recording_stop);
                if clip_stop.is_none() {
                    clip_stop = state.in_segment.stop();
                }
                let mut segment = state.in_segment.clone();
                segment.set_start(clip_start);
                segment.set_stop(clip_stop);

                gst_log!(CAT, obj: pad, "Clipping to segment {:?}", segment);

                if let Some(data) = data.clip(&*state, &segment) {
                    return Ok(HandleResult::Pass(data));
                } else {
                    gst_warning!(CAT, obj: pad, "Complete buffer clipped!");
                    return Ok(HandleResult::Drop);
                }
            } else if current_running_time.map_or(false, |cur_rt| cur_rt < last_recording_start) {
                // Otherwise if the buffer starts before the recording start, drop it. This
                // means that we either can't clip, or that the end is also before the
                // recording start
                gst_debug!(
                    CAT,
                    obj: pad,
                    "Main stream EOS and we're not EOS yet (before recording start, {} < {})",
                    current_running_time.display(),
                    last_recording_start,
                );
                return Ok(HandleResult::Drop);
            } else if data.can_clip(&*state)
                && current_running_time
                    .zip(rec_state.last_recording_stop)
                    .map_or(false, |(cur_rt, last_rec_stop)| cur_rt < last_rec_stop)
                && current_running_time_end
                    .zip(rec_state.last_recording_stop)
                    .map_or(false, |(cur_rt_end, last_rec_stop)| {
                        cur_rt_end > last_rec_stop
                    })
            {
                // Similarly if the end is after the recording stop but the start is before and we
                // can clip, clip the buffer and pass it through.
                gst_debug!(
                        CAT,
                        obj: pad,
                        "Main stream EOS and we're not EOS yet (overlapping recording end, {} < {} < {})",
                        current_running_time.display(),
                        rec_state.last_recording_stop.display(),
                        current_running_time_end.display(),
                    );

                let mut clip_start = state
                    .in_segment
                    .position_from_running_time(rec_state.last_recording_start);
                if clip_start.is_none() {
                    clip_start = state.in_segment.start();
                }
                let mut clip_stop = state
                    .in_segment
                    .position_from_running_time(rec_state.last_recording_stop);
                if clip_stop.is_none() {
                    clip_stop = state.in_segment.stop();
                }
                let mut segment = state.in_segment.clone();
                segment.set_start(clip_start);
                segment.set_stop(clip_stop);

                gst_log!(CAT, obj: pad, "Clipping to segment {:?}", segment,);

                if let Some(data) = data.clip(&*state, &segment) {
                    return Ok(HandleResult::Pass(data));
                } else {
                    gst_warning!(CAT, obj: pad, "Complete buffer clipped!");
                    return Ok(HandleResult::Eos(self.check_and_update_eos(
                        pad,
                        stream,
                        &mut state,
                        &mut rec_state,
                    )));
                }
            } else if current_running_time_end
                .zip(rec_state.last_recording_stop)
                .map_or(false, |(cur_rt_end, last_rec_stop)| {
                    cur_rt_end > last_rec_stop
                })
            {
                // Otherwise if the end of the buffer is after the recording stop, we're EOS
                // now. This means that we either couldn't clip or that the start is also after
                // the recording stop
                gst_debug!(
                    CAT,
                    obj: pad,
                    "Main stream EOS and we're EOS too (after recording end, {} > {})",
                    current_running_time_end.display(),
                    rec_state.last_recording_stop.display(),
                );
                return Ok(HandleResult::Eos(self.check_and_update_eos(
                    pad,
                    stream,
                    &mut state,
                    &mut rec_state,
                )));
            } else {
                // In all other cases the buffer is fully between recording start and end and
                // can be passed through as is
                assert!(current_running_time.map_or(false, |cur_rt| cur_rt >= last_recording_start));
                assert!(current_running_time_end
                    .zip(rec_state.last_recording_stop)
                    .map_or(false, |(cur_rt_end, last_rec_stop)| cur_rt_end
                        <= last_rec_stop));

                gst_debug!(
                    CAT,
                    obj: pad,
                    "Main stream EOS and we're not EOS yet (before recording end, {} <= {} <= {})",
                    last_recording_start,
                    current_running_time.display(),
                    rec_state.last_recording_stop.display(),
                );
                return Ok(HandleResult::Pass(data));
            }
        }

        match rec_state.recording_state {
            RecordingState::Recording => {
                // The end of our buffer must be before/at the end of the previous buffer of the main
                // stream
                assert!(current_running_time_end
                    .zip(main_state.current_running_time_end)
                    .map_or(false, |(cur_rt_end, main_cur_rt_end)| cur_rt_end
                        <= main_cur_rt_end));

                // We're properly started, must have a start position and
                // be actually after that start position
                assert!(current_running_time
                    .zip(rec_state.last_recording_start)
                    .map_or(false, |(cur_rt, last_rec_start)| cur_rt >= last_rec_start));
                gst_log!(CAT, obj: pad, "Passing buffer (recording)");
                Ok(HandleResult::Pass(data))
            }
            RecordingState::Stopping => {
                // If we have no start position yet, the main stream is waiting for a key-frame
                let last_recording_stop = match rec_state.last_recording_stop {
                    Some(last_recording_stop) => last_recording_stop,
                    None => {
                        gst_log!(
                            CAT,
                            obj: pad,
                            "Passing buffer (stopping: waiting for keyframe)",
                        );
                        return Ok(HandleResult::Pass(data));
                    }
                };

                // The start of our buffer must be before the last recording stop as
                // otherwise we would be in Stopped state already
                assert!(current_running_time.map_or(false, |cur_rt| cur_rt < last_recording_stop));
                let current_running_time = current_running_time.expect("checked above");

                if current_running_time_end
                    .map_or(false, |cur_rt_end| cur_rt_end <= last_recording_stop)
                {
                    gst_log!(
                        CAT,
                        obj: pad,
                        "Passing buffer (stopping: {} <= {})",
                        current_running_time_end.display(),
                        last_recording_stop,
                    );
                    Ok(HandleResult::Pass(data))
                } else if data.can_clip(&*state)
                    && current_running_time < last_recording_stop
                    && current_running_time_end
                        .map_or(false, |cur_rt_end| cur_rt_end > last_recording_stop)
                {
                    gst_log!(
                        CAT,
                        obj: pad,
                        "Passing buffer (stopping: {} < {} < {})",
                        current_running_time,
                        last_recording_stop,
                        current_running_time_end.display(),
                    );

                    let mut clip_stop = state
                        .in_segment
                        .position_from_running_time(rec_state.last_recording_stop);
                    if clip_stop.is_none() {
                        clip_stop = state.in_segment.stop();
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
                        current_running_time_end.display(),
                        rec_state.last_recording_stop.display(),
                    );
                    Ok(HandleResult::Drop)
                }
            }
            RecordingState::Stopped => {
                // The end of our buffer must be before/at the end of the previous buffer of the main
                // stream
                assert!(current_running_time_end
                    .zip(main_state.current_running_time_end)
                    .map_or(false, |(cur_rt_end, state_rt_end)| cur_rt_end
                        <= state_rt_end));

                // We're properly stopped
                gst_log!(CAT, obj: pad, "Dropping buffer (stopped)");
                Ok(HandleResult::Drop)
            }
            RecordingState::Starting => {
                // If we have no start position yet, the main stream is waiting for a key-frame
                let last_recording_start = match rec_state.last_recording_start {
                    Some(last_recording_start) => last_recording_start,
                    None => {
                        gst_log!(
                            CAT,
                            obj: pad,
                            "Dropping buffer (starting: waiting for keyframe)",
                        );
                        return Ok(HandleResult::Drop);
                    }
                };

                // The start of our buffer must be before the last recording start as
                // otherwise we would be in Recording state already
                assert!(current_running_time.map_or(false, |cur_rt| cur_rt < last_recording_start));
                let current_running_time = current_running_time.expect("checked_above");

                if current_running_time >= last_recording_start {
                    gst_log!(
                        CAT,
                        obj: pad,
                        "Passing buffer (starting: {} >= {})",
                        current_running_time,
                        last_recording_start,
                    );
                    Ok(HandleResult::Pass(data))
                } else if data.can_clip(&*state)
                    && current_running_time < last_recording_start
                    && current_running_time_end
                        .map_or(false, |cur_rt_end| cur_rt_end > last_recording_start)
                {
                    gst_log!(
                        CAT,
                        obj: pad,
                        "Passing buffer (starting: {} < {} < {})",
                        current_running_time,
                        last_recording_start,
                        current_running_time_end.display(),
                    );

                    let mut clip_start = state
                        .in_segment
                        .position_from_running_time(rec_state.last_recording_start);
                    if clip_start.is_none() {
                        clip_start = state.in_segment.start();
                    }
                    let mut segment = state.in_segment.clone();
                    segment.set_start(clip_start);

                    gst_log!(CAT, obj: pad, "Clipping to segment {:?}", segment);

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
                        last_recording_start,
                    );
                    Ok(HandleResult::Drop)
                }
            }
        }
    }

    // should be called only if main stream is in eos state
    fn check_and_update_eos(
        &self,
        pad: &gst::Pad,
        stream: &Stream,
        stream_state: &mut StreamState,
        rec_state: &mut State,
    ) -> bool {
        stream_state.eos = true;

        // Check whether all secondary streams are in eos. If so, update recording
        // state to Stopped
        if rec_state.recording_state != RecordingState::Stopped {
            let mut others_eos = true;

            // Check eos state of all secondary streams
            self.other_streams.lock().0.iter().all(|s| {
                if s == stream {
                    return true;
                }

                let s = s.state.lock();
                if !s.eos {
                    others_eos = false;
                }
                others_eos
            });

            if others_eos {
                gst_debug!(
                    CAT,
                    obj: pad,
                    "All streams are in EOS state, change state to Stopped"
                );

                rec_state.recording_state = RecordingState::Stopped;
                return true;
            }
        }

        false
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::ToggleRecord,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let stream = self.pads.lock().get(pad).cloned().ok_or_else(|| {
            gst::element_error!(
                element,
                gst::CoreError::Pad,
                ["Unknown pad {:?}", pad.name()]
            );
            gst::FlowError::Error
        })?;

        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        {
            let state = stream.state.lock();
            if state.eos {
                return Err(gst::FlowError::Eos);
            }
            if state.flushing {
                return Err(gst::FlowError::Flushing);
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
            HandleResult::Eos(recording_state_updated) => {
                stream.srcpad.push_event(
                    gst::event::Eos::builder()
                        .seqnum(stream.state.lock().segment_seqnum)
                        .build(),
                );

                if recording_state_updated {
                    element.notify("recording");
                }

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
                state
                    .out_segment
                    .offset_running_time(-rec_state.running_time_offset)
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

            let out_running_time = state.out_segment.to_running_time(buffer.pts());

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
            out_running_time.display(),
            buffer,
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
                gst::element_error!(
                    element,
                    gst::CoreError::Pad,
                    ["Unknown pad {:?}", pad.name()]
                );
                return false;
            }
            Some(stream) => stream.clone(),
        };

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        let mut forward = true;
        let mut send_pending = false;
        let mut recording_state_changed = false;

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
                state.current_running_time = None;
                state.current_running_time_end = None;
            }
            EventView::Caps(c) => {
                let mut state = stream.state.lock();
                let caps = c.caps();
                let s = caps.structure(0).unwrap();
                if s.name().starts_with("audio/") {
                    state.audio_info = gst_audio::AudioInfo::from_caps(caps).ok();
                    gst_log!(CAT, obj: pad, "Got audio caps {:?}", state.audio_info);
                    state.video_info = None;
                } else if s.name().starts_with("video/") {
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

                let segment = match e.segment().clone().downcast::<gst::ClockTime>() {
                    Err(segment) => {
                        gst::element_error!(
                            element,
                            gst::StreamError::Format,
                            ["Only Time segments supported, got {:?}", segment.format(),]
                        );
                        return false;
                    }
                    Ok(segment) => segment,
                };

                if (segment.rate() - 1.0).abs() > f64::EPSILON {
                    gst::element_error!(
                        element,
                        gst::StreamError::Format,
                        [
                            "Only rate==1.0 segments supported, got {:?}",
                            segment.rate(),
                        ]
                    );
                    return false;
                }

                state.in_segment = segment;
                state.segment_seqnum = event.seqnum();
                state.segment_pending = true;
                state.current_running_time = None;
                state.current_running_time_end = None;

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
                    Ok(HandleResult::Pass((new_pts, new_duration))) => {
                        if new_pts != pts
                            || new_duration
                                .zip(duration)
                                .map_or(false, |(new_duration, duration)| new_duration != duration)
                        {
                            event = gst::event::Gap::builder(new_pts)
                                .duration(new_duration)
                                .build();
                        }
                        true
                    }
                    _ => false,
                };
            }
            EventView::Eos(..) => {
                let main_state = if stream != self.main_stream {
                    Some(self.main_stream.state.lock())
                } else {
                    None
                };
                let mut state = stream.state.lock();
                state.eos = true;

                let main_is_eos = if let Some(main_state) = main_state {
                    main_state.eos
                } else {
                    true
                };

                if main_is_eos {
                    let mut rec_state = self.state.lock();
                    recording_state_changed =
                        self.check_and_update_eos(pad, &stream, &mut state, &mut rec_state);
                }

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

        if recording_state_changed {
            element.notify("recording");
        }

        // If a serialized event and coming after Segment and a new Segment is pending,
        // queue up and send at a later time (buffer/gap) after we sent the Segment
        let type_ = event.type_();
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
                gst::element_error!(
                    element,
                    gst::CoreError::Pad,
                    ["Unknown pad {:?}", pad.name()]
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
                gst::element_error!(
                    element,
                    gst::CoreError::Pad,
                    ["Unknown pad {:?}", pad.name()]
                );
                return false;
            }
            Some(stream) => stream.clone(),
        };

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        let forward = !matches!(event.view(), EventView::Seek(..));

        let rec_state = self.state.lock();
        let offset = event.running_time_offset();
        event
            .make_mut()
            .set_running_time_offset(offset + rec_state.running_time_offset);
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
                gst::element_error!(
                    element,
                    gst::CoreError::Pad,
                    ["Unknown pad {:?}", pad.name()]
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

                let (flags, min, max, align) = new_query.result();
                q.set(flags, min, max, align);
                q.add_scheduling_modes(
                    &new_query
                        .scheduling_modes()
                        .iter()
                        .cloned()
                        .filter(|m| m != &gst::PadMode::Pull)
                        .collect::<Vec<_>>(),
                );
                gst_log!(CAT, obj: pad, "Returning {:?}", q.query_mut());
                true
            }
            QueryView::Seeking(ref mut q) => {
                // Seeking is not possible here
                let format = q.format();
                q.set(
                    false,
                    gst::GenericFormattedValue::new(format, -1),
                    gst::GenericFormattedValue::new(format, -1),
                );

                gst_log!(CAT, obj: pad, "Returning {:?}", q.query_mut());
                true
            }
            // Position and duration is always the current recording position
            QueryView::Position(ref mut q) => {
                if q.format() == gst::Format::Time {
                    let state = stream.state.lock();
                    let rec_state = self.state.lock();
                    let mut recording_duration = rec_state.recording_duration;
                    if rec_state.recording_state == RecordingState::Recording
                        || rec_state.recording_state == RecordingState::Stopping
                    {
                        if let Some(delta) = state
                            .current_running_time_end
                            .zip(rec_state.last_recording_start)
                            .and_then(|(cur_rt_end, last_rec_start)| {
                                cur_rt_end.checked_sub(last_rec_start)
                            })
                        {
                            gst_debug!(
                                CAT,
                                obj: pad,
                                "Returning position {} = {} - ({} + {})",
                                recording_duration + delta,
                                recording_duration,
                                state.current_running_time_end.display(),
                                rec_state.last_recording_start.display(),
                            );
                            recording_duration += delta;
                        }
                    } else {
                        gst_debug!(CAT, obj: pad, "Returning position {}", recording_duration);
                    }
                    q.set(recording_duration);
                    true
                } else {
                    false
                }
            }
            QueryView::Duration(ref mut q) => {
                if q.format() == gst::Format::Time {
                    let state = stream.state.lock();
                    let rec_state = self.state.lock();
                    let mut recording_duration = rec_state.recording_duration;
                    if rec_state.recording_state == RecordingState::Recording
                        || rec_state.recording_state == RecordingState::Stopping
                    {
                        if let Some(delta) = state
                            .current_running_time_end
                            .zip(rec_state.last_recording_start)
                            .and_then(|(cur_rt_end, last_rec_start)| {
                                cur_rt_end.checked_sub(last_rec_start)
                            })
                        {
                            gst_debug!(
                                CAT,
                                obj: pad,
                                "Returning duration {} = {} - ({} + {})",
                                recording_duration + delta,
                                recording_duration,
                                state.current_running_time_end.display(),
                                rec_state.last_recording_start.display(),
                            );
                            recording_duration += delta;
                        }
                    } else {
                        gst_debug!(CAT, obj: pad, "Returning duration {}", recording_duration);
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
                gst::element_error!(
                    element,
                    gst::CoreError::Pad,
                    ["Unknown pad {:?}", pad.name()]
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

#[glib::object_subclass]
impl ObjectSubclass for ToggleRecord {
    const NAME: &'static str = "RsToggleRecord";
    type Type = super::ToggleRecord;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
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

        let templ = klass.pad_template("src").unwrap();
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
}

impl ObjectImpl for ToggleRecord {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_boolean(
                    "record",
                    "Record",
                    "Enable/disable recording",
                    DEFAULT_RECORD,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING,
                ),
                glib::ParamSpec::new_boolean(
                    "recording",
                    "Recording",
                    "Whether recording is currently taking place",
                    DEFAULT_RECORD,
                    glib::ParamFlags::READABLE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "record" => {
                let mut settings = self.settings.lock();
                let record = value.get().expect("type checked upstream");
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

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "record" => {
                let settings = self.settings.lock();
                settings.record.to_value()
            }
            "recording" => {
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
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Toggle Record",
                "Generic",
                "Valve that ensures multiple streams start/end at the same time",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
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

            let secondary_src_pad_template = gst::PadTemplate::new(
                "src_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &caps,
            )
            .unwrap();

            let secondary_sink_pad_template = gst::PadTemplate::new(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
            )
            .unwrap();

            vec![
                src_pad_template,
                sink_pad_template,
                secondary_src_pad_template,
                secondary_sink_pad_template,
            ]
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

        let templ = element.pad_template("sink_%u").unwrap();
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

        let templ = element.pad_template("src_%u").unwrap();
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
