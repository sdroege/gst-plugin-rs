// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use once_cell::sync::Lazy;

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex};

fn utc_time_to_pts(
    segment: &gst::FormattedSegment<gst::ClockTime>,
    utc_time_running_time_mapping: (gst::ClockTime, gst::Signed<gst::ClockTime>),
    utc_time: gst::ClockTime,
) -> Option<gst::ClockTime> {
    let running_time = match utc_time_to_running_time(utc_time_running_time_mapping, utc_time)? {
        gst::Signed::Positive(running_time) => running_time,
        _ => return None,
    };
    segment.position_from_running_time(running_time)
}

fn utc_time_to_running_time(
    utc_time_running_time_mapping: (gst::ClockTime, gst::Signed<gst::ClockTime>),
    utc_time: gst::ClockTime,
) -> Option<gst::Signed<gst::ClockTime>> {
    if utc_time < utc_time_running_time_mapping.0 {
        let diff = utc_time_running_time_mapping.0 - utc_time;
        utc_time_running_time_mapping.1.checked_sub_unsigned(diff)
    } else {
        let diff = utc_time - utc_time_running_time_mapping.0;
        utc_time_running_time_mapping.1.checked_add_unsigned(diff)
    }
}

fn running_time_to_utc_time(
    utc_time_running_time_mapping: (gst::ClockTime, gst::Signed<gst::ClockTime>),
    running_time: gst::Signed<gst::ClockTime>,
) -> Option<gst::ClockTime> {
    let diff = running_time.checked_sub(utc_time_running_time_mapping.1)?;

    match diff {
        gst::Signed::Positive(diff) => utc_time_running_time_mapping.0.checked_add(diff),
        gst::Signed::Negative(diff) => utc_time_running_time_mapping.0.checked_sub(diff),
    }
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "onvifmetadataparse",
        gst::DebugColorFlags::empty(),
        Some("ONVIF Metadata Parser Element"),
    )
});

#[derive(Clone, Debug)]
struct Settings {
    latency: Option<gst::ClockTime>,
    max_lateness: Option<gst::ClockTime>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            latency: None,
            max_lateness: Some(gst::ClockTime::from_mseconds(200)),
        }
    }
}

#[derive(Debug)]
struct Frame {
    video_analytics: minidom::Element,
    other_elements: Vec<minidom::Element>,
    events: Vec<gst::Event>,
}

impl Default for Frame {
    fn default() -> Self {
        Frame {
            video_analytics: minidom::Element::bare(
                "VideoAnalytics",
                "http://www.onvif.org/ver10/schema",
            ),
            other_elements: Vec::new(),
            events: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
enum TimedBufferOrEvent {
    Buffer(gst::Signed<gst::ClockTime>, gst::Buffer),
    Event(gst::Signed<gst::ClockTime>, gst::Event),
}

#[derive(Debug, Clone)]
enum BufferOrEvent {
    Buffer(gst::Buffer),
    Event(gst::Event),
}

#[derive(Debug)]
struct State {
    /// Initially queued buffers and serialized events until we have a UTC time / running time mapping.
    pre_queued_buffers: Vec<TimedBufferOrEvent>,
    /// Mapping of UTC time to running time.
    utc_time_running_time_mapping: Option<(gst::ClockTime, gst::Signed<gst::ClockTime>)>,
    /// UTC time -> XML data and serialized events.
    queued_frames: BTreeMap<gst::ClockTime, Frame>,

    /// Currently configured input segment.
    in_segment: gst::FormattedSegment<gst::ClockTime>,
    /// Currently configured output segment.
    out_segment: gst::FormattedSegment<gst::ClockTime>,

    /// Upstream latency and live'ness.
    upstream_latency: Option<(bool, gst::ClockTime)>,
    /// Configured latency on this element.
    configured_latency: gst::ClockTime,
    /// Last flow return of the source pad.
    last_flow_ret: Result<gst::FlowSuccess, gst::FlowError>,
    /// Clock wait of the source pad task.
    /// Otherwise the source pad task waits on the condition variable.
    clock_wait: Option<gst::SingleShotClockId>,
}

impl Default for State {
    fn default() -> Self {
        let mut segment = gst::FormattedSegment::default();
        segment.set_position(gst::ClockTime::NONE);

        State {
            pre_queued_buffers: Vec::new(),
            utc_time_running_time_mapping: None,
            queued_frames: BTreeMap::new(),

            in_segment: segment.clone(),
            out_segment: segment.clone(),

            upstream_latency: None,
            configured_latency: gst::ClockTime::ZERO,
            last_flow_ret: Err(gst::FlowError::Flushing),
            clock_wait: None,
        }
    }
}

pub struct OnvifMetadataParse {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    cond: Condvar,
}

impl OnvifMetadataParse {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::OnvifMetadataParse,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(
            CAT,
            obj: pad,
            "Handling buffer {:?} with UTC time {}",
            buffer,
            crate::lookup_reference_timestamp(&buffer).display()
        );

        let mut state = self.state.lock().unwrap();

        let pts = match buffer.pts() {
            Some(pts) => pts,
            None => {
                gst::error!(CAT, obj: pad, "Need buffers with PTS");
                return Err(gst::FlowError::Error);
            }
        };

        let running_time = state.in_segment.to_running_time_full(pts).unwrap();

        if state
            .in_segment
            .position()
            .map_or(true, |position| position < pts)
        {
            gst::trace!(CAT, obj: element, "Input position updated to {}", pts);
            state.in_segment.set_position(pts);
        }

        // First we need to get an UTC/running time mapping. We wait up to the latency
        // for that and otherwise error out.
        if state.utc_time_running_time_mapping.is_none() {
            let utc_time = crate::lookup_reference_timestamp(&buffer);
            if let Some(utc_time) = utc_time {
                let (initial_utc_time, initial_running_time) = loop {
                    let (idx, initial_running_time) = state
                        .pre_queued_buffers
                        .iter()
                        .enumerate()
                        .find_map(|(idx, o)| {
                            if let TimedBufferOrEvent::Buffer(running_time, _) = o {
                                Some((Some(idx), *running_time))
                            } else {
                                None
                            }
                        })
                        .unwrap_or((None, running_time));

                    let diff = match running_time.checked_sub(initial_running_time) {
                        Some(diff) => diff,
                        None => {
                            gst::error!(
                            CAT,
                            obj: pad,
                            "Too big running time difference between initial running time {:?} and current running time {:?}",
                            initial_running_time,
                            running_time,
                        );
                            return Err(gst::FlowError::Error);
                        }
                    };

                    let initial_utc_time = match gst::Signed::Positive(utc_time).checked_sub(diff) {
                        Some(gst::Signed::Positive(initial_utc_time)) => initial_utc_time,
                        Some(gst::Signed::Negative(initial_utc_time)) => {
                            gst::warning!(
                                CAT,
                                obj: pad,
                                "Initial UTC time is negative: -{}, dropping buffer",
                                initial_utc_time
                            );

                            state.pre_queued_buffers.remove(idx.unwrap());
                            continue;
                        }
                        None => {
                            gst::warning!(
                                CAT,
                                obj: pad,
                                "Can't calculate initial UTC time, dropping buffer"
                            );
                            state.pre_queued_buffers.remove(idx.unwrap());
                            continue;
                        }
                    };

                    break (initial_utc_time, initial_running_time);
                };

                gst::info!(
                    CAT,
                    obj: pad,
                    "Calculated initial UTC/running time mapping: {}/{:?}",
                    initial_utc_time,
                    initial_running_time
                );
                state.utc_time_running_time_mapping =
                    Some((initial_utc_time, initial_running_time));
            } else {
                state
                    .pre_queued_buffers
                    .push(TimedBufferOrEvent::Buffer(running_time, buffer));

                if let Some((idx, front_running_time)) = state
                    .pre_queued_buffers
                    .iter()
                    .enumerate()
                    .find_map(|(idx, o)| {
                        if let TimedBufferOrEvent::Buffer(running_time, _) = o {
                            Some((idx, *running_time))
                        } else {
                            None
                        }
                    })
                {
                    if running_time.saturating_sub(front_running_time)
                        >= gst::Signed::Positive(state.configured_latency)
                    {
                        gst::warning!(
                            CAT,
                            obj: pad,
                            "Received no UTC time in the first {}",
                            state.configured_latency
                        );
                        state.pre_queued_buffers.remove(idx);
                    }
                }

                return Ok(gst::FlowSuccess::Ok);
            }
        }

        assert!(state.utc_time_running_time_mapping.is_some());
        self.queue(element, &mut state, buffer, running_time)?;
        let res = self.wake_up_output(element, state);

        gst::trace!(CAT, obj: pad, "Returning {:?}", res);

        res
    }

    fn queue(
        &self,
        element: &super::OnvifMetadataParse,
        state: &mut State,
        buffer: gst::Buffer,
        running_time: gst::Signed<gst::ClockTime>,
    ) -> Result<(), gst::FlowError> {
        let State {
            ref mut pre_queued_buffers,
            ref mut queued_frames,
            ref utc_time_running_time_mapping,
            ..
        } = &mut *state;

        let utc_time_running_time_mapping = utc_time_running_time_mapping.unwrap();

        for buffer_or_event in
            pre_queued_buffers
                .drain(..)
                .chain(std::iter::once(TimedBufferOrEvent::Buffer(
                    running_time,
                    buffer,
                )))
        {
            let (running_time, buffer) = match buffer_or_event {
                TimedBufferOrEvent::Event(running_time, event) => {
                    let current_utc_time =
                        running_time_to_utc_time(utc_time_running_time_mapping, running_time)
                            .unwrap_or(gst::ClockTime::ZERO);

                    let frame = queued_frames
                        .entry(current_utc_time)
                        .or_insert_with(Frame::default);
                    frame.events.push(event);

                    continue;
                }
                TimedBufferOrEvent::Buffer(running_time, buffer) => (running_time, buffer),
            };

            let root = crate::xml_from_buffer(&buffer).map_err(|err| {
                element.post_error_message(err);

                gst::FlowError::Error
            })?;

            for res in crate::iterate_video_analytics_frames(&root) {
                let (dt, el) = res.map_err(|err| {
                    element.post_error_message(err);

                    gst::FlowError::Error
                })?;

                let dt_unix_ns = gst::ClockTime::from_nseconds(dt.timestamp_nanos() as u64)
                    + crate::PRIME_EPOCH_OFFSET;

                gst::trace!(
                    CAT,
                    obj: element,
                    "Queueing frame with UTC time {}",
                    dt_unix_ns
                );

                let frame = queued_frames
                    .entry(dt_unix_ns)
                    .or_insert_with(Frame::default);

                frame.video_analytics.append_child(el.clone());
            }

            let utc_time = running_time_to_utc_time(utc_time_running_time_mapping, running_time)
                .unwrap_or(gst::ClockTime::ZERO);

            for child in root.children() {
                let frame = queued_frames.entry(utc_time).or_insert_with(Frame::default);

                if child.is("VideoAnalytics", "http://www.onvif.org/ver10/schema") {
                    for subchild in child.children() {
                        if subchild.is("Frame", "http://www.onvif.org/ver10/schema") {
                            continue;
                        }

                        frame.video_analytics.append_child(subchild.clone());
                    }
                } else {
                    frame.other_elements.push(child.clone());
                }
            }
        }

        Ok(())
    }

    fn wake_up_output<'a>(
        &'a self,
        element: &super::OnvifMetadataParse,
        mut state: std::sync::MutexGuard<'a, State>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if state.upstream_latency.is_none() {
            drop(state);

            gst::debug!(CAT, obj: element, "Have no upstream latency yet, querying");
            let mut q = gst::query::Latency::new();
            let res = self.sinkpad.peer_query(&mut q);

            state = self.state.lock().unwrap();

            if res {
                let (live, min, max) = q.result();

                gst::debug!(
                    CAT,
                    obj: element,
                    "Latency query response: live {} min {} max {}",
                    live,
                    min,
                    max.display()
                );

                state.upstream_latency = Some((live, min));
            } else {
                gst::warning!(
                    CAT,
                    obj: element,
                    "Can't query upstream latency -- assuming non-live upstream for now"
                );
            }
        }

        // Consider waking up the source element thread
        if self.sinkpad.pad_flags().contains(gst::PadFlags::EOS) {
            gst::trace!(CAT, obj: element, "Scheduling immediate wakeup at EOS",);

            if let Some(clock_wait) = state.clock_wait.take() {
                clock_wait.unschedule();
            }
            self.cond.notify_all();
        } else if self.reschedule_clock_wait(element, &mut state) {
            self.cond.notify_all();
        } else {
            // Not live or have no clock

            // Wake up if between now and the earliest frame's running time more than the
            // configured latency has passed.
            let queued_time = self.calculate_queued_time(element, &state);

            if queued_time.map_or(false, |queued_time| queued_time >= state.configured_latency) {
                gst::trace!(
                    CAT,
                    obj: element,
                    "Scheduling immediate wakeup -- queued time {}",
                    queued_time.display()
                );

                if let Some(clock_wait) = state.clock_wait.take() {
                    clock_wait.unschedule();
                }
                self.cond.notify_all();
            }
        }

        state.last_flow_ret
    }

    fn calculate_queued_time(
        &self,
        element: &super::OnvifMetadataParse,
        state: &State,
    ) -> Option<gst::ClockTime> {
        let earliest_utc_time = match state.queued_frames.iter().next() {
            Some((&earliest_utc_time, _earliest_frame)) => earliest_utc_time,
            None => return None,
        };

        let earliest_running_time = utc_time_to_running_time(
            state.utc_time_running_time_mapping.unwrap(),
            earliest_utc_time,
        );

        let current_running_time = state
            .in_segment
            .to_running_time_full(state.in_segment.position());

        let queued_time = Option::zip(current_running_time, earliest_running_time)
            .and_then(|(current_running_time, earliest_running_time)| {
                current_running_time.checked_sub(earliest_running_time)
            })
            .and_then(|queued_time| queued_time.positive_or(()).ok())
            .unwrap_or(gst::ClockTime::ZERO);

        gst::trace!(
            CAT,
            obj: element,
            "Currently queued {}",
            queued_time.display()
        );

        Some(queued_time)
    }

    fn reschedule_clock_wait(
        &self,
        element: &super::OnvifMetadataParse,
        state: &mut State,
    ) -> bool {
        let earliest_utc_time = match state.queued_frames.iter().next() {
            Some((&earliest_utc_time, _earliest_frame)) => earliest_utc_time,
            None => return false,
        };

        let min_latency = match state.upstream_latency {
            Some((true, min_latency)) => min_latency,
            _ => return false,
        };

        let earliest_running_time = utc_time_to_running_time(
            state.utc_time_running_time_mapping.unwrap(),
            earliest_utc_time,
        );

        let (clock, base_time) = match (element.clock(), element.base_time()) {
            (Some(clock), Some(base_time)) => (clock, base_time),
            _ => {
                gst::warning!(
                    CAT,
                    obj: element,
                    "Upstream is live but have no clock -- assuming non-live for now"
                );
                return false;
            }
        };

        // Update clock wait to the clock time of the earliest metadata to output plus
        // the configured latency
        if let Some(earliest_clock_time) = earliest_running_time
            .and_then(|earliest_running_time| {
                earliest_running_time
                    .checked_add_unsigned(base_time + min_latency + state.configured_latency)
            })
            .and_then(|earliest_clock_time| earliest_clock_time.positive_or(()).ok())
        {
            if state
                .clock_wait
                .as_ref()
                .map_or(true, |clock_wait| clock_wait.time() != earliest_clock_time)
            {
                if let Some(clock_wait) = state.clock_wait.take() {
                    clock_wait.unschedule();
                }
                gst::trace!(
                    CAT,
                    obj: element,
                    "Scheduling timer for {} / running time {}, now {}",
                    earliest_clock_time,
                    earliest_running_time
                        .unwrap()
                        .positive_or(())
                        .unwrap_or(gst::ClockTime::ZERO)
                        .display(),
                    clock.time().display(),
                );
                state.clock_wait = Some(clock.new_single_shot_id(earliest_clock_time));
            }
        } else {
            // Wake up immediately if the metadata is before the segment
            if let Some(clock_wait) = state.clock_wait.take() {
                clock_wait.unschedule();
            }
            gst::trace!(CAT, obj: element, "Scheduling immediate wakeup");
        }

        true
    }

    fn drain(
        &self,
        element: &super::OnvifMetadataParse,
        state: &mut State,
        drain_utc_time: Option<gst::ClockTime>,
    ) -> Result<Vec<BufferOrEvent>, gst::FlowError> {
        let State {
            ref mut queued_frames,
            ref mut out_segment,
            utc_time_running_time_mapping,
            ..
        } = &mut *state;

        let utc_time_running_time_mapping = match utc_time_running_time_mapping {
            Some(utc_time_running_time_mapping) => *utc_time_running_time_mapping,
            None => return Ok(vec![]),
        };

        gst::log!(
            CAT,
            obj: element,
            "Draining up to UTC time {} / running time {} from current position {} / running time {}",
            drain_utc_time.display(),
            drain_utc_time
                .and_then(|drain_utc_time| utc_time_to_running_time(
                    utc_time_running_time_mapping,
                    drain_utc_time
                ))
                .and_then(|running_time| running_time.positive_or(()).ok())
                .display(),
            out_segment.position().display(),
            out_segment
                .to_running_time(out_segment.position())
                .display(),
        );

        let mut data = Vec::new();

        while !queued_frames.is_empty() {
            let utc_time = *queued_frames.iter().next().unwrap().0;

            // Check if this frame should still be drained
            if drain_utc_time.map_or(false, |drain_utc_time| drain_utc_time < utc_time) {
                break;
            }

            // FIXME: Use pop_first() once stabilized
            let mut frame = queued_frames.remove(&utc_time).unwrap();

            let had_events = !frame.events.is_empty();
            for event in frame.events.drain(..) {
                match event.view() {
                    gst::EventView::Segment(ev) => {
                        let mut segment = ev
                            .segment()
                            .downcast_ref::<gst::ClockTime>()
                            .unwrap()
                            .clone();
                        let current_position = out_segment
                            .position()
                            .and_then(|position| out_segment.to_running_time(position))
                            .and_then(|running_time| {
                                segment.position_from_running_time(running_time)
                            });
                        segment.set_position(current_position);

                        gst::debug!(
                            CAT,
                            obj: element,
                            "Configuring output segment {:?}",
                            segment
                        );

                        *out_segment = segment;

                        data.push(BufferOrEvent::Event(event));
                    }
                    gst::EventView::Caps(ev) => {
                        data.push(BufferOrEvent::Event(
                            gst::event::Caps::builder(&self.srcpad.pad_template_caps())
                                .seqnum(ev.seqnum())
                                .build(),
                        ));
                    }
                    gst::EventView::Gap(ev) => {
                        let (current_position, _duration) = ev.get();

                        if out_segment
                            .position()
                            .map_or(true, |position| position < current_position)
                        {
                            gst::trace!(
                                CAT,
                                obj: element,
                                "Output position updated to {}",
                                current_position
                            );
                            out_segment.set_position(current_position);
                        }

                        data.push(BufferOrEvent::Event(event));
                    }
                    _ => {
                        data.push(BufferOrEvent::Event(event));
                    }
                }
            }

            let mut frame_pts =
                match utc_time_to_pts(out_segment, utc_time_running_time_mapping, utc_time) {
                    Some(frame_pts) => frame_pts,
                    None => {
                        gst::warning!(CAT, obj: element, "UTC time {} outside segment", utc_time);
                        gst::ClockTime::ZERO
                    }
                };

            if frame.video_analytics.children().next().is_none() && frame.other_elements.is_empty()
            {
                // Generate a gap event if there's no actual data for this time
                if !had_events {
                    data.push(BufferOrEvent::Event(
                        gst::event::Gap::builder(frame_pts).build(),
                    ));
                }

                continue;
            }

            if let Some(position) = out_segment.position() {
                let settings = self.settings.lock().unwrap().clone();
                let diff = position.saturating_sub(frame_pts);
                if settings
                    .max_lateness
                    .map_or(false, |max_lateness| diff > max_lateness)
                {
                    gst::warning!(
                        CAT,
                        obj: element,
                        "Dropping frame with UTC time {} / PTS {} that is too late by {} at current position {}",
                        utc_time,
                        frame_pts,
                        diff,
                        position,
                    );
                    continue;
                } else if diff > gst::ClockTime::ZERO {
                    gst::warning!(
                        CAT,
                        obj: element,
                        "Frame in the past by {} with UTC time {} / PTS {} at current position {}",
                        diff,
                        utc_time,
                        frame_pts,
                        position,
                    );

                    frame_pts = position;
                }
            }

            if out_segment
                .position()
                .map_or(true, |position| position < frame_pts)
            {
                gst::trace!(
                    CAT,
                    obj: element,
                    "Output position updated to {}",
                    frame_pts
                );
                out_segment.set_position(frame_pts);
            }

            let Frame {
                video_analytics,
                other_elements,
                ..
            } = frame;

            gst::trace!(
                CAT,
                obj: element,
                "Producing frame with UTC time {} / PTS {}",
                utc_time,
                frame_pts
            );

            let mut xml =
                minidom::Element::builder("MetadataStream", "http://www.onvif.org/ver10/schema")
                    .prefix(Some("tt".into()), "http://www.onvif.org/ver10/schema")
                    .unwrap()
                    .build();

            if video_analytics.children().next().is_some() {
                xml.append_child(video_analytics);
            }
            for child in other_elements {
                xml.append_child(child);
            }

            let mut vec = Vec::new();
            if let Err(err) = xml.write_to_decl(&mut vec) {
                gst::error!(CAT, obj: element, "Can't serialize XML element: {}", err);
                continue;
            }

            let mut buffer = gst::Buffer::from_mut_slice(vec);
            let buffer_ref = buffer.get_mut().unwrap();
            buffer_ref.set_pts(frame_pts);

            gst::ReferenceTimestampMeta::add(
                buffer_ref,
                &crate::NTP_CAPS,
                utc_time,
                gst::ClockTime::NONE,
            );

            data.push(BufferOrEvent::Buffer(buffer));
        }

        gst::trace!(
            CAT,
            obj: element,
            "Position after draining {} / running time {} -- queued now {} / {} items",
            out_segment.position().display(),
            out_segment
                .to_running_time(out_segment.position())
                .display(),
            self.calculate_queued_time(element, state).display(),
            state.queued_frames.len(),
        );

        Ok(data)
    }

    fn sink_event(
        &self,
        pad: &gst::Pad,
        element: &super::OnvifMetadataParse,
        event: gst::Event,
    ) -> bool {
        gst::log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            gst::EventView::FlushStart(_) => {
                let mut state = self.state.lock().unwrap();
                state.last_flow_ret = Err(gst::FlowError::Flushing);
                if let Some(ref clock_wait) = state.clock_wait.take() {
                    clock_wait.unschedule();
                }
                drop(state);
                self.cond.notify_all();

                pad.event_default(Some(element), event)
            }
            gst::EventView::FlushStop(_) => {
                let _ = self.srcpad.stop_task();
                let mut state = self.state.lock().unwrap();
                state.pre_queued_buffers.clear();
                state.queued_frames.clear();
                state.utc_time_running_time_mapping = None;
                state.in_segment.reset();
                state.in_segment.set_position(gst::ClockTime::NONE);
                state.out_segment.reset();
                state.out_segment.set_position(gst::ClockTime::NONE);
                state.last_flow_ret = Ok(gst::FlowSuccess::Ok);
                drop(state);
                let mut res = pad.event_default(Some(element), event);
                if res {
                    res = Self::src_start_task(element, &self.srcpad).is_ok();
                }
                res
            }
            ev if event.is_serialized() => {
                let mut state = self.state.lock().unwrap();

                match ev {
                    gst::EventView::Segment(ev) => {
                        match ev.segment().downcast_ref::<gst::ClockTime>().cloned() {
                            Some(mut segment) => {
                                let current_position = state
                                    .in_segment
                                    .position()
                                    .and_then(|position| state.in_segment.to_running_time(position))
                                    .and_then(|running_time| {
                                        segment.position_from_running_time(running_time)
                                    });
                                segment.set_position(current_position);

                                gst::debug!(
                                    CAT,
                                    obj: pad,
                                    "Configuring input segment {:?}",
                                    segment
                                );
                                state.in_segment = segment;
                            }
                            None => {
                                gst::error!(CAT, obj: pad, "Non-TIME segment");
                                return false;
                            }
                        }
                    }
                    gst::EventView::Caps(ev) => {
                        let settings = self.settings.lock().unwrap().clone();

                        let previous_latency = state.configured_latency;
                        let latency = if let Some(latency) = settings.latency {
                            latency
                        } else {
                            let caps = ev.caps();
                            let s = caps.structure(0).unwrap();
                            let parsed = Some(true) == s.get("parsed").ok();

                            if parsed {
                                gst::ClockTime::ZERO
                            } else {
                                gst::ClockTime::from_seconds(6)
                            }
                        };
                        state.configured_latency = latency;
                        drop(state);

                        gst::debug!(CAT, obj: pad, "Configuring latency of {}", latency);
                        if previous_latency != latency {
                            let _ = element.post_message(
                                gst::message::Latency::builder().src(element).build(),
                            );
                        }

                        state = self.state.lock().unwrap();
                    }
                    gst::EventView::Gap(ev) => {
                        let (mut current_position, duration) = ev.get();
                        if let Some(duration) = duration {
                            current_position += duration;
                        }

                        if state
                            .in_segment
                            .position()
                            .map_or(true, |position| position < current_position)
                        {
                            gst::trace!(
                                CAT,
                                obj: element,
                                "Input position updated to {}",
                                current_position
                            );
                            state.in_segment.set_position(current_position);
                        }
                    }
                    _ => (),
                }

                let State {
                    utc_time_running_time_mapping,
                    ref in_segment,
                    ref mut queued_frames,
                    ref mut pre_queued_buffers,
                    ..
                } = &mut *state;

                if let Some(utc_time_running_time_mapping) = utc_time_running_time_mapping {
                    let current_running_time = in_segment
                        .to_running_time_full(in_segment.position())
                        .unwrap_or(gst::Signed::Negative(gst::ClockTime::from_nseconds(
                            u64::MAX,
                        )));
                    let current_utc_time = running_time_to_utc_time(
                        *utc_time_running_time_mapping,
                        current_running_time,
                    )
                    .unwrap_or(gst::ClockTime::ZERO);

                    gst::trace!(
                        CAT,
                        obj: element,
                        "Queueing event with UTC time {} / running time {}",
                        current_utc_time,
                        current_running_time.positive_or(()).ok().display(),
                    );

                    let frame = queued_frames
                        .entry(current_utc_time)
                        .or_insert_with(Frame::default);
                    frame.events.push(event);

                    self.wake_up_output(element, state).is_ok()
                } else {
                    if let gst::EventView::Eos(_) = ev {
                        gst::error!(
                            CAT,
                            obj: element,
                            "Got EOS event before creating UTC/running time mapping"
                        );
                        gst::element_error!(
                            element,
                            gst::StreamError::Failed,
                            ["Got EOS event before creating UTC/running time mapping"]
                        );
                        return pad.event_default(Some(element), event);
                    }

                    let current_running_time = in_segment
                        .to_running_time_full(in_segment.position())
                        .unwrap_or(gst::Signed::Negative(gst::ClockTime::from_nseconds(
                            u64::MAX,
                        )));

                    gst::trace!(
                        CAT,
                        obj: element,
                        "Pre-queueing event with running time {}",
                        current_running_time.positive_or(()).ok().display()
                    );

                    pre_queued_buffers.push(TimedBufferOrEvent::Event(current_running_time, event));
                    true
                }
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn sink_query(
        &self,
        pad: &gst::Pad,
        element: &super::OnvifMetadataParse,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst::log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            gst::QueryViewMut::Caps(q) => {
                let caps = pad.pad_template_caps();
                let res = if let Some(filter) = q.filter() {
                    filter.intersect_with_mode(&caps, gst::CapsIntersectMode::First)
                } else {
                    caps
                };

                q.set_result(&res);

                true
            }
            gst::QueryViewMut::AcceptCaps(q) => {
                let caps = q.caps();
                let res = caps.can_intersect(&pad.pad_template_caps());
                q.set_result(res);

                true
            }
            gst::QueryViewMut::Allocation(_) => {
                gst::fixme!(CAT, obj: pad, "Dropping allocation query");
                false
            }
            _ => pad.query_default(Some(element), query),
        }
    }

    fn src_event(
        &self,
        pad: &gst::Pad,
        element: &super::OnvifMetadataParse,
        event: gst::Event,
    ) -> bool {
        gst::log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            gst::EventView::FlushStart(_) => {
                let mut state = self.state.lock().unwrap();
                state.last_flow_ret = Err(gst::FlowError::Flushing);
                if let Some(ref clock_wait) = state.clock_wait.take() {
                    clock_wait.unschedule();
                }
                drop(state);
                self.cond.notify_all();

                pad.event_default(Some(element), event)
            }
            gst::EventView::FlushStop(_) => {
                let _ = self.srcpad.stop_task();
                let mut state = self.state.lock().unwrap();
                state.pre_queued_buffers.clear();
                state.queued_frames.clear();
                state.utc_time_running_time_mapping = None;
                state.in_segment.reset();
                state.in_segment.set_position(gst::ClockTime::NONE);
                state.out_segment.reset();
                state.out_segment.set_position(gst::ClockTime::NONE);
                state.last_flow_ret = Ok(gst::FlowSuccess::Ok);
                drop(state);
                let mut res = pad.event_default(Some(element), event);
                if res {
                    res = Self::src_start_task(element, &self.srcpad).is_ok();
                }
                res
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        element: &super::OnvifMetadataParse,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst::log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            gst::QueryViewMut::Caps(q) => {
                let caps = pad.pad_template_caps();
                let res = if let Some(filter) = q.filter() {
                    filter.intersect_with_mode(&caps, gst::CapsIntersectMode::First)
                } else {
                    caps
                };

                q.set_result(&res);

                true
            }
            gst::QueryViewMut::AcceptCaps(q) => {
                let caps = q.caps();
                let res = caps.can_intersect(&pad.pad_template_caps());
                q.set_result(res);

                true
            }
            gst::QueryViewMut::Latency(q) => {
                let mut upstream_query = gst::query::Latency::new();
                let ret = self.sinkpad.peer_query(&mut upstream_query);

                if ret {
                    let (live, mut min, mut max) = upstream_query.result();

                    let mut state = self.state.lock().unwrap();
                    state.upstream_latency = Some((live, min));
                    min += state.configured_latency;
                    max = max.map(|max| max + state.configured_latency);

                    q.set(live, min, max);

                    gst::debug!(
                        CAT,
                        obj: pad,
                        "Latency query response: live {} min {} max {}",
                        live,
                        min,
                        max.display()
                    );

                    let _ = self.wake_up_output(element, state);
                }

                ret
            }
            _ => pad.query_default(Some(element), query),
        }
    }

    fn src_start_task(
        element: &super::OnvifMetadataParse,
        pad: &gst::Pad,
    ) -> Result<(), gst::LoggableError> {
        let element = element.clone();
        pad.start_task(move || {
            let self_ = element.imp();
            if let Err(err) = self_.src_loop(&element) {
                match err {
                    gst::FlowError::Flushing => {
                        gst::debug!(CAT, obj: &element, "Pausing after flow {:?}", err);
                    }
                    gst::FlowError::Eos => {
                        let _ = self_.srcpad.push_event(gst::event::Eos::builder().build());

                        gst::debug!(CAT, obj: &element, "Pausing after flow {:?}", err);
                    }
                    _ => {
                        let _ = self_.srcpad.push_event(gst::event::Eos::builder().build());

                        gst::error!(CAT, obj: &element, "Pausing after flow {:?}", err);

                        gst::element_error!(
                            &element,
                            gst::StreamError::Failed,
                            ["Streaming stopped, reason: {:?}", err]
                        );
                    }
                }

                let _ = self_.srcpad.pause_task();
            }
        })
        .map_err(|err| gst::loggable_error!(CAT, "Failed to start pad task: {}", err))
    }

    fn src_activatemode(
        pad: &gst::Pad,
        mode: gst::PadMode,
        activate: bool,
    ) -> Result<(), gst::LoggableError> {
        if mode == gst::PadMode::Pull || activate && mode == gst::PadMode::None {
            return Err(gst::loggable_error!(CAT, "Invalid activation mode"));
        }

        if activate {
            let element = pad
                .parent()
                .map(|p| p.downcast::<<Self as ObjectSubclass>::Type>().unwrap())
                .ok_or_else(|| {
                    gst::loggable_error!(CAT, "Failed to start pad task: pad has no parent")
                })?;

            let self_ = element.imp();
            let mut state = self_.state.lock().unwrap();
            state.last_flow_ret = Ok(gst::FlowSuccess::Ok);
            drop(state);

            Self::src_start_task(&element, pad)?;
        } else {
            let element = pad
                .parent()
                .map(|p| p.downcast::<super::OnvifMetadataParse>().unwrap())
                .ok_or_else(|| {
                    gst::loggable_error!(CAT, "Failed to stop pad task: pad has no parent")
                })?;

            let self_ = element.imp();
            let mut state = self_.state.lock().unwrap();
            state.last_flow_ret = Err(gst::FlowError::Flushing);
            if let Some(ref clock_wait) = state.clock_wait.take() {
                clock_wait.unschedule();
            }
            drop(state);
            self_.cond.notify_all();

            pad.stop_task()
                .map_err(|err| gst::loggable_error!(CAT, "Failed to stop pad task: {}", err))?;
        }

        Ok(())
    }

    fn src_loop(&self, element: &super::OnvifMetadataParse) -> Result<(), gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        let mut clock_wait_time = None;

        loop {
            // If flushing or any other error then just return here
            state.last_flow_ret?;

            if !self.sinkpad.pad_flags().contains(gst::PadFlags::EOS) {
                if let Some(clock_wait) = state.clock_wait.clone() {
                    gst::trace!(
                        CAT,
                        obj: element,
                        "Waiting on timer with time {}, now {}",
                        clock_wait.time(),
                        clock_wait.clock().and_then(|clock| clock.time()).display(),
                    );
                    clock_wait_time = Some(clock_wait.time());

                    drop(state);
                    let res = clock_wait.wait();
                    state = self.state.lock().unwrap();

                    match res {
                        (Ok(_), jitter) => {
                            gst::trace!(CAT, obj: element, "Woke up after waiting for {}", jitter);
                        }
                        (Err(err), jitter) => {
                            gst::trace!(
                                CAT,
                                obj: element,
                                "Woke up with error {:?} and jitter {}",
                                err,
                                jitter
                            );

                            // If unscheduled wait again or return immediately above if flushing
                            if err == gst::ClockError::Unscheduled {
                                continue;
                            }
                        }
                    }
                } else {
                    gst::debug!(CAT, obj: element, "Waiting on cond");
                    state = self.cond.wait(state).unwrap();

                    if state.clock_wait.is_some() {
                        gst::trace!(CAT, obj: element, "Got timer now, waiting again");
                        continue;
                    }
                    gst::trace!(CAT, obj: element, "Woke up and checking for data to drain");
                }
            }

            break;
        }

        // If flushing or any other error then just return here
        state.last_flow_ret?;

        let res = loop {
            // Calculate running time until which to drain now
            let mut drain_running_time = None;
            if self.sinkpad.pad_flags().contains(gst::PadFlags::EOS) {
                // Drain completely
                gst::debug!(CAT, obj: element, "Sink pad is EOS, draining");
            } else if let Some((true, min_latency)) = state.upstream_latency {
                if let Some((now, base_time)) = Option::zip(
                    element.clock().and_then(|clock| clock.time()),
                    element.base_time(),
                ) {
                    gst::trace!(
                        CAT,
                        obj: element,
                        "Clock time now {}, timer at {}",
                        now,
                        clock_wait_time.display()
                    );

                    let now =
                        (now - base_time).saturating_sub(min_latency + state.configured_latency);
                    let now = if let Some(clock_wait_time) = clock_wait_time {
                        let clock_wait_time = (clock_wait_time - base_time)
                            .saturating_sub(min_latency + state.configured_latency);
                        std::cmp::max(now, clock_wait_time)
                    } else {
                        now
                    };

                    drain_running_time = Some(gst::Signed::Positive(now));
                }
            } else {
                let current_running_time = state
                    .in_segment
                    .to_running_time_full(state.in_segment.position());
                drain_running_time = current_running_time.and_then(|current_running_time| {
                    current_running_time.checked_sub_unsigned(state.configured_latency)
                });
            }

            // And drain up to that running time now
            let data = if self.sinkpad.pad_flags().contains(gst::PadFlags::EOS) {
                self.drain(element, &mut state, None)?
            } else if let Some((drain_running_time, utc_time_running_time_mapping)) =
                Option::zip(drain_running_time, state.utc_time_running_time_mapping)
            {
                if let Some(drain_utc_time) =
                    running_time_to_utc_time(utc_time_running_time_mapping, drain_running_time)
                {
                    self.drain(element, &mut state, Some(drain_utc_time))?
                } else {
                    vec![]
                }
            } else {
                vec![]
            };

            if data.is_empty() {
                break state.last_flow_ret.map(|_| ());
            }

            drop(state);

            let mut res = Ok(());

            gst::trace!(CAT, obj: element, "Pushing {} items downstream", data.len());
            for data in data {
                match data {
                    BufferOrEvent::Event(event) => {
                        gst::trace!(CAT, obj: element, "Pushing event {:?}", event);
                        self.srcpad.push_event(event);
                    }
                    BufferOrEvent::Buffer(buffer) => {
                        gst::trace!(CAT, obj: element, "Pushing buffer {:?}", buffer);
                        if let Err(err) = self.srcpad.push(buffer) {
                            res = Err(err);
                            break;
                        }
                    }
                }
            }
            gst::trace!(CAT, obj: element, "Pushing returned {:?}", res);

            state = self.state.lock().unwrap();

            // If flushing or any other error then just return here
            state.last_flow_ret?;
            state.last_flow_ret = res.map(|_| gst::FlowSuccess::Ok);

            self.reschedule_clock_wait(element, &mut state);
        };

        res
    }
}

#[glib::object_subclass]
impl ObjectSubclass for OnvifMetadataParse {
    const NAME: &'static str = "OnvifMetadataParse";
    type Type = super::OnvifMetadataParse;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                OnvifMetadataParse::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |parse, element| parse.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                OnvifMetadataParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.sink_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                OnvifMetadataParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.sink_query(pad, element, query),
                )
            })
            .flags(gst::PadFlags::PROXY_ALLOCATION)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .event_function(|pad, parent, event| {
                OnvifMetadataParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.src_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                OnvifMetadataParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.src_query(pad, element, query),
                )
            })
            .activatemode_function(|pad, _parent, mode, activate| {
                Self::src_activatemode(pad, mode, activate)
            })
            .flags(gst::PadFlags::PROXY_ALLOCATION)
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Mutex::default(),
            state: Mutex::default(),
            cond: Condvar::new(),
        }
    }
}

impl ObjectImpl for OnvifMetadataParse {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt64::builder("latency")
                    .nick("Latency")
                    .blurb(
                        "Maximum latency to introduce for reordering metadata \
                     (max=auto: 6s if unparsed input, 0s if parsed input)",
                    )
                    .default_value(
                        Settings::default()
                            .latency
                            .map(|l| l.nseconds())
                            .unwrap_or(u64::MAX),
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("max-lateness")
                    .nick("Maximum Lateness")
                    .blurb("Drop metadata that delayed by more than this")
                    .default_value(
                        Settings::default()
                            .max_lateness
                            .map(|l| l.nseconds())
                            .unwrap_or(u64::MAX),
                    )
                    .mutable_ready()
                    .build(),
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
            "latency" => {
                self.settings.lock().unwrap().latency = value.get().expect("type checked upstream");

                let _ = obj.post_message(gst::message::Latency::builder().src(obj).build());
            }
            "max-lateness" => {
                self.settings.lock().unwrap().max_lateness =
                    value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "latency" => self.settings.lock().unwrap().latency.to_value(),
            "max-lateness" => self.settings.lock().unwrap().max_lateness.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for OnvifMetadataParse {}

impl ElementImpl for OnvifMetadataParse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "ONVIF Metadata Parser",
                "Metadata/Parser",
                "Parses ONVIF Timed XML Metadata",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("application/x-onvif-metadata")
                .field("parsed", true)
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst::Caps::builder("application/x-onvif-metadata").build();
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
        gst::trace!(CAT, obj: element, "Changing state {:?}", transition);

        if matches!(
            transition,
            gst::StateChange::PausedToReady | gst::StateChange::ReadyToPaused
        ) {
            let mut state = self.state.lock().unwrap();
            *state = State::default();
        }

        self.parent_change_state(element, transition)
    }
}
