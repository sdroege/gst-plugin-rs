// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info, gst_trace, gst_warning};

use std::collections::VecDeque;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use super::boxes;
use super::Buffer;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "fmp4mux",
        gst::DebugColorFlags::empty(),
        Some("FMP4Mux Element"),
    )
});

const DEFAULT_FRAGMENT_DURATION: gst::ClockTime = gst::ClockTime::from_seconds(10);
const DEFAULT_HEADER_UPDATE_MODE: super::HeaderUpdateMode = super::HeaderUpdateMode::None;
const DEFAULT_WRITE_MFRA: bool = false;
const DEFAULT_WRITE_MEHD: bool = false;

#[derive(Debug, Clone)]
struct Settings {
    fragment_duration: gst::ClockTime,
    header_update_mode: super::HeaderUpdateMode,
    write_mfra: bool,
    write_mehd: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            fragment_duration: DEFAULT_FRAGMENT_DURATION,
            header_update_mode: DEFAULT_HEADER_UPDATE_MODE,
            write_mfra: DEFAULT_WRITE_MFRA,
            write_mehd: DEFAULT_WRITE_MEHD,
        }
    }
}

struct Gop {
    // Running times
    start_pts: gst::ClockTime,
    start_dts: Option<gst::ClockTime>,
    earliest_pts: gst::ClockTime,
    // Once this is known to be the final earliest PTS/DTS
    final_earliest_pts: bool,
    // PTS plus duration of last buffer, or start of next GOP
    end_pts: gst::ClockTime,
    // DTS plus duration of last buffer, or start of next GOP
    end_dts: Option<gst::ClockTime>,

    // Buffer positions
    earliest_pts_position: gst::ClockTime,
    start_dts_position: Option<gst::ClockTime>,

    // Buffer, PTS running time, DTS running time
    buffers: Vec<Buffer>,
}

#[derive(Default)]
struct State {
    segment: Option<gst::FormattedSegment<gst::ClockTime>>,
    caps: Option<gst::Caps>,
    intra_only: bool,

    // Created once we received caps and kept up to date with the caps,
    // sent as part of the buffer list for the first fragment.
    stream_header: Option<gst::Buffer>,

    sequence_number: u32,
    queued_gops: VecDeque<Gop>,
    // Duration of all GOPs except for the newest one that is still being filled
    queued_duration: gst::ClockTime,

    // Difference between the first DTS and 0 in case of negative DTS
    dts_offset: Option<gst::ClockTime>,

    last_force_keyunit_time: Option<gst::ClockTime>,

    // Fragment tracking for mfra
    current_offset: u64,
    fragment_offsets: Vec<super::FragmentOffset>,

    // Start / end PTS of the whole stream
    earliest_pts: Option<gst::ClockTime>,
    end_pts: Option<gst::ClockTime>,
}

pub(crate) struct FMP4Mux {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl FMP4Mux {
    fn queue_input(
        &self,
        element: &super::FMP4Mux,
        state: &mut State,
        buffer: gst::Buffer,
    ) -> Result<(), gst::FlowError> {
        gst_trace!(CAT, obj: element, "Handling buffer {:?}", buffer);

        let segment = match state.segment {
            Some(ref segment) => segment,
            None => {
                gst_error!(CAT, obj: element, "Got buffer before segment");
                return Err(gst::FlowError::Error);
            }
        };

        if state.caps.is_none() {
            gst_error!(CAT, obj: element, "Got buffer before caps");
            return Err(gst::FlowError::NotNegotiated);
        }

        let intra_only = state.intra_only;

        if !intra_only && buffer.dts().is_none() {
            gst_error!(CAT, obj: element, "Require DTS for video streams");
            return Err(gst::FlowError::Error);
        }

        if intra_only && buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            gst_error!(CAT, obj: element, "Intra-only stream with delta units");
            return Err(gst::FlowError::Error);
        }

        let pts = buffer.pts().ok_or_else(|| {
            gst_error!(CAT, obj: element, "Require timestamped buffers");
            gst::FlowError::Error
        })?;
        let duration = buffer.duration();
        let end_pts = duration.opt_add(pts).unwrap_or(pts);

        let pts = match segment.to_running_time_full(pts) {
            (_, None) => {
                gst_error!(CAT, obj: element, "Couldn't convert PTS to running time");
                return Err(gst::FlowError::Error);
            }
            (pts_signum, _) if pts_signum < 0 => {
                gst_error!(CAT, obj: element, "Negative PTSs are not supported");
                return Err(gst::FlowError::Error);
            }
            (_, Some(pts)) => pts,
        };

        let end_pts = match segment.to_running_time_full(end_pts) {
            (_, None) => {
                gst_error!(
                    CAT,
                    obj: element,
                    "Couldn't convert end PTS to running time"
                );
                return Err(gst::FlowError::Error);
            }
            (pts_signum, _) if pts_signum < 0 => {
                gst_error!(CAT, obj: element, "Negative PTSs are not supported");
                return Err(gst::FlowError::Error);
            }
            (_, Some(pts)) => pts,
        };

        let (dts, end_dts) = if intra_only {
            (None, None)
        } else {
            // with the dts_offset by having negative composition time offsets in the `trun` box.
            let dts = buffer.dts().expect("not DTS");
            let end_dts = duration.opt_add(dts).unwrap_or(dts);

            let dts = match segment.to_running_time_full(dts) {
                (_, None) => {
                    gst_error!(CAT, obj: element, "Couldn't convert DTS to running time");
                    return Err(gst::FlowError::Error);
                }
                (pts_signum, Some(dts)) if pts_signum < 0 => {
                    if state.dts_offset.is_none() {
                        state.dts_offset = Some(dts);
                    }

                    let dts_offset = state.dts_offset.unwrap();
                    if dts > dts_offset {
                        gst_warning!(CAT, obj: element, "DTS before first DTS");
                        gst::ClockTime::ZERO
                    } else {
                        dts_offset - dts
                    }
                }
                (_, Some(dts)) => {
                    if let Some(dts_offset) = state.dts_offset {
                        dts + dts_offset
                    } else {
                        dts
                    }
                }
            };

            let end_dts = match segment.to_running_time_full(end_dts) {
                (_, None) => {
                    gst_error!(
                        CAT,
                        obj: element,
                        "Couldn't convert end DTS to running time"
                    );
                    return Err(gst::FlowError::Error);
                }
                (pts_signum, Some(dts)) if pts_signum < 0 => {
                    if state.dts_offset.is_none() {
                        state.dts_offset = Some(dts);
                    }

                    let dts_offset = state.dts_offset.unwrap();
                    if dts > dts_offset {
                        gst_warning!(CAT, obj: element, "End DTS before first DTS");
                        gst::ClockTime::ZERO
                    } else {
                        dts_offset - dts
                    }
                }
                (_, Some(dts)) => {
                    if let Some(dts_offset) = state.dts_offset {
                        dts + dts_offset
                    } else {
                        dts
                    }
                }
            };
            (Some(dts), Some(end_dts))
        };

        if !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            gst_debug!(
                CAT,
                obj: element,
                "Starting new GOP at PTS {} DTS {}",
                pts,
                dts.display()
            );

            let gop = Gop {
                start_pts: pts,
                start_dts: dts,
                start_dts_position: if intra_only { None } else { buffer.dts() },
                earliest_pts: pts,
                earliest_pts_position: buffer.pts().expect("no PTS"),
                final_earliest_pts: intra_only,
                end_pts,
                end_dts,
                buffers: vec![Buffer { buffer, pts, dts }],
            };
            state.queued_gops.push_front(gop);

            if let Some(prev_gop) = state.queued_gops.get_mut(1) {
                gst_debug!(
                    CAT,
                    obj: element,
                    "Updating previous GOP starting at PTS {} to end PTS {} DTS {}",
                    prev_gop.earliest_pts,
                    pts,
                    dts.display(),
                );
                prev_gop.end_pts = pts;
                prev_gop.end_dts = dts;

                if !prev_gop.final_earliest_pts {
                    // Don't bother logging this for intra-only streams as it would be for every
                    // single buffer.
                    if !intra_only {
                        gst_debug!(
                            CAT,
                            obj: element,
                            "Previous GOP has final earliest PTS at {}",
                            prev_gop.earliest_pts
                        );
                    }

                    prev_gop.final_earliest_pts = true;

                    state.queued_duration =
                        prev_gop.end_pts - state.queued_gops.back().unwrap().earliest_pts;
                    gst_debug!(
                        CAT,
                        obj: element,
                        "Queued duration updated to {}",
                        state.queued_duration
                    );
                } else if intra_only {
                    state.queued_duration =
                        prev_gop.end_pts - state.queued_gops.back().unwrap().earliest_pts;
                    gst_debug!(
                        CAT,
                        obj: element,
                        "Queued duration updated to {}",
                        state.queued_duration
                    );
                }
            }
        } else if let Some(gop) = state.queued_gops.front_mut() {
            assert!(!intra_only);

            // We require DTS for non-intra-only streams
            let dts = dts.unwrap();
            let end_dts = end_dts.unwrap();
            let pts_position = buffer.pts().expect("no PTS");

            gop.end_pts = std::cmp::max(gop.end_pts, end_pts);
            gop.end_dts = Some(std::cmp::max(gop.end_dts.expect("no end DTS"), end_dts));
            gop.buffers.push(Buffer {
                buffer,
                pts,
                dts: Some(dts),
            });

            if gop.earliest_pts > pts && !gop.final_earliest_pts {
                gst_debug!(
                    CAT,
                    obj: element,
                    "Updating current GOP earliest PTS from {} to {}",
                    gop.earliest_pts,
                    pts
                );
                gop.earliest_pts = pts;
                gop.earliest_pts_position = pts_position;

                if let Some(prev_gop) = state.queued_gops.get_mut(1) {
                    gst_debug!(
                        CAT,
                        obj: element,
                        "Updating previous GOP starting PTS {} end time from {} to {}",
                        pts,
                        prev_gop.end_pts,
                        pts
                    );
                    prev_gop.end_pts = pts;
                }
            }

            let gop = state.queued_gops.front_mut().unwrap();

            // The earliest PTS is known when the current DTS is bigger or equal to the first
            // PTS that was observed in this GOP. If there was another frame later that had a
            // lower PTS then it wouldn't be possible to display it in time anymore, i.e. the
            // stream would be invalid.
            if gop.start_pts <= dts && !gop.final_earliest_pts {
                gst_debug!(
                    CAT,
                    obj: element,
                    "GOP has final earliest PTS at {}",
                    gop.earliest_pts
                );
                gop.final_earliest_pts = true;

                if let Some(prev_gop) = state.queued_gops.get_mut(1) {
                    state.queued_duration =
                        prev_gop.end_pts - state.queued_gops.back().unwrap().earliest_pts;
                    gst_debug!(
                        CAT,
                        obj: element,
                        "Queued duration updated to {}",
                        state.queued_duration
                    );
                }
            }
        } else {
            gst_warning!(
                CAT,
                obj: element,
                "Waiting for keyframe at the beginning of the stream"
            );
        }

        Ok(())
    }

    fn create_force_keyunit_event(
        &self,
        element: &super::FMP4Mux,
        state: &mut State,
        settings: &Settings,
        pts: gst::ClockTime,
    ) -> Result<Option<gst::Event>, gst::FlowError> {
        let segment = state.segment.as_ref().expect("no segment");

        // If we never sent a force-keyunit event then wait until the earliest PTS of the first GOP
        // is known and send one now.
        //
        // Otherwise if the current PTS is a fragment duration in the future, send the next one
        // now.
        let oldest_gop = state.queued_gops.back().unwrap();
        let earliest_pts = oldest_gop.earliest_pts;
        let pts = segment.to_running_time(pts).expect("no running time");

        if state.last_force_keyunit_time.is_none() && oldest_gop.final_earliest_pts {
            let fku_running_time = earliest_pts + settings.fragment_duration;
            gst_debug!(
                CAT,
                obj: element,
                "Sending first force-keyunit event for running time {}",
                fku_running_time
            );
            state.last_force_keyunit_time = Some(fku_running_time);

            return Ok(Some(
                gst_video::UpstreamForceKeyUnitEvent::builder()
                    .running_time(fku_running_time)
                    .all_headers(true)
                    .build(),
            ));
        } else if state.last_force_keyunit_time.is_some()
            && state.last_force_keyunit_time <= Some(pts)
        {
            let fku_running_time =
                state.last_force_keyunit_time.unwrap() + settings.fragment_duration;
            gst_debug!(
                CAT,
                obj: element,
                "Sending force-keyunit event for running time {}",
                fku_running_time
            );
            state.last_force_keyunit_time = Some(fku_running_time);

            return Ok(Some(
                gst_video::UpstreamForceKeyUnitEvent::builder()
                    .running_time(fku_running_time)
                    .all_headers(true)
                    .build(),
            ));
        }

        Ok(None)
    }

    fn drain(
        &self,
        element: &super::FMP4Mux,
        state: &mut State,
        settings: &Settings,
        at_eos: bool,
    ) -> Result<Option<gst::BufferList>, gst::FlowError> {
        let class = element.class();

        if state.queued_duration < settings.fragment_duration && !at_eos {
            return Ok(None);
        }

        assert!(at_eos || state.queued_gops.get(1).map(|gop| gop.final_earliest_pts) == Some(true));

        // At EOS, finalize all GOPs and drain them out. Otherwise if the queued duration is
        // equal to the fragment duration then drain out all complete GOPs, otherwise all
        // except for the newest complete GOP.
        let drain_gops = if at_eos {
            gst_info!(CAT, obj: element, "Draining at EOS");
            state.queued_duration = gst::ClockTime::ZERO;
            state
                .queued_gops
                .drain(..)
                .map(|mut gop| {
                    gop.final_earliest_pts = true;
                    gop
                })
                .collect::<Vec<_>>()
        } else if state.queued_duration == settings.fragment_duration
            || state.queued_gops.len() == 2
        {
            state.queued_duration = gst::ClockTime::ZERO;
            state.queued_gops.drain(1..).collect::<Vec<_>>()
        } else {
            let gops = state.queued_gops.drain(2..).collect::<Vec<_>>();

            let gop = state.queued_gops.front().unwrap();
            if gop.final_earliest_pts {
                let prev_gop = state.queued_gops.get(1).unwrap();
                state.queued_duration =
                    prev_gop.end_pts - state.queued_gops.back().unwrap().earliest_pts;
            } else {
                state.queued_duration = gst::ClockTime::ZERO;
            }

            gops
        };

        let mut buffer_list = None;

        if !drain_gops.is_empty() {
            let earliest_pts = drain_gops.last().unwrap().earliest_pts;
            let earliest_pts_position = drain_gops.last().unwrap().earliest_pts_position;
            let start_dts = drain_gops.last().unwrap().start_dts;
            let start_dts_position = drain_gops.last().unwrap().start_dts_position;
            let end_pts = drain_gops[0].end_pts;
            let end_dts = drain_gops[0].end_dts;
            let dts_offset = state.dts_offset;

            gst_info!(
                CAT,
                obj: element,
                "Draining {} worth of buffers starting at PTS {} DTS {}, DTS offset {}",
                end_pts - earliest_pts,
                earliest_pts,
                start_dts.display(),
                dts_offset.display(),
            );

            let mut fmp4_header = None;
            if state.sequence_number == 0 {
                let mut buffer = state.stream_header.as_ref().unwrap().copy();
                {
                    let buffer = buffer.get_mut().unwrap();

                    buffer.set_pts(earliest_pts_position);
                    buffer.set_dts(start_dts_position);

                    // Header is DISCONT|HEADER
                    buffer.set_flags(gst::BufferFlags::DISCONT | gst::BufferFlags::HEADER);
                }

                fmp4_header = Some(buffer);

                state.earliest_pts = Some(earliest_pts);
                state.sequence_number = 1;
            }

            let mut buffers = drain_gops
                .into_iter()
                .rev()
                .flat_map(|gop| gop.buffers)
                .collect::<Vec<Buffer>>();

            // TODO: Write prft boxes before moof
            // TODO: Write sidx boxes before moof and rewrite once offsets are known

            let sequence_number = state.sequence_number;
            state.sequence_number += 1;
            let (mut fmp4_fragment_header, moof_offset) =
                boxes::create_fmp4_fragment_header(super::FragmentHeaderConfiguration {
                    variant: class.as_ref().variant,
                    sequence_number,
                    caps: state.caps.as_ref().unwrap(),
                    buffers: &buffers,
                    earliest_pts,
                    start_dts,
                    end_pts,
                    end_dts,
                    dts_offset,
                })
                .map_err(|err| {
                    gst_error!(
                        CAT,
                        obj: element,
                        "Failed to create FMP4 fragment header: {}",
                        err
                    );
                    gst::FlowError::Error
                })?;

            {
                let buffer = fmp4_fragment_header.get_mut().unwrap();
                buffer.set_pts(earliest_pts_position);
                buffer.set_dts(start_dts_position);
                buffer.set_duration(end_pts.checked_sub(earliest_pts));

                // Fragment header is HEADER
                buffer.set_flags(gst::BufferFlags::HEADER);

                // Copy metas from the first actual buffer to the fragment header. This allows
                // getting things like the reference timestamp meta or the timecode meta to identify
                // the fragment.
                let _ = buffers[0]
                    .buffer
                    .copy_into(buffer, gst::BufferCopyFlags::META, 0, None);
            }

            let moof_offset = state.current_offset
                + fmp4_header.as_ref().map(|h| h.size()).unwrap_or(0) as u64
                + moof_offset;

            let buffers_len = buffers.len();
            for (idx, buffer) in buffers.iter_mut().enumerate() {
                // Fix up buffer flags, all other buffers are DELTA_UNIT
                let buffer_ref = buffer.buffer.make_mut();
                buffer_ref.unset_flags(gst::BufferFlags::all());
                buffer_ref.set_flags(gst::BufferFlags::DELTA_UNIT);

                // Set the marker flag for the last buffer of the segment
                if idx == buffers_len - 1 {
                    buffer_ref.set_flags(gst::BufferFlags::MARKER);
                }
            }

            buffer_list = Some(
                fmp4_header
                    .into_iter()
                    .chain(Some(fmp4_fragment_header))
                    .chain(buffers.into_iter().map(|buffer| buffer.buffer))
                    .inspect(|b| {
                        state.current_offset += b.size() as u64;
                    })
                    .collect::<gst::BufferList>(),
            );

            state.fragment_offsets.push(super::FragmentOffset {
                time: earliest_pts,
                offset: moof_offset,
            });
            state.end_pts = Some(end_pts);

            gst_debug!(
                CAT,
                obj: element,
                "Queued duration updated to {} after draining",
                state.queued_duration
            );
        }

        if settings.write_mfra && at_eos {
            match boxes::create_mfra(state.caps.as_ref().unwrap(), &state.fragment_offsets) {
                Ok(mut mfra) => {
                    {
                        let mfra = mfra.get_mut().unwrap();
                        // mfra is HEADER|DELTA_UNIT like other boxes
                        mfra.set_flags(gst::BufferFlags::HEADER | gst::BufferFlags::DELTA_UNIT);
                    }

                    if buffer_list.is_none() {
                        buffer_list = Some(gst::BufferList::new_sized(1));
                    }
                    buffer_list.as_mut().unwrap().get_mut().unwrap().add(mfra);
                }
                Err(err) => {
                    gst_error!(CAT, obj: element, "Failed to create mfra box: {}", err);
                }
            }
        }

        // TODO: Write edit list at EOS
        // TODO: Rewrite bitrates at EOS

        Ok(buffer_list)
    }

    fn update_header(
        &self,
        element: &super::FMP4Mux,
        state: &mut State,
        settings: &Settings,
        at_eos: bool,
    ) -> Result<Option<(gst::BufferList, gst::Caps)>, gst::FlowError> {
        let class = element.class();
        let variant = class.as_ref().variant;

        if settings.header_update_mode == super::HeaderUpdateMode::None && at_eos {
            return Ok(None);
        }

        assert!(!at_eos || state.queued_gops.is_empty());

        let duration = state
            .end_pts
            .opt_checked_sub(state.earliest_pts)
            .ok()
            .flatten();

        let mut buffer = boxes::create_fmp4_header(super::HeaderConfiguration {
            variant,
            update: at_eos,
            caps: state.caps.as_ref().unwrap(),
            write_mehd: settings.write_mehd,
            duration: if at_eos { duration } else { None },
        })
        .map_err(|err| {
            gst_error!(CAT, obj: element, "Failed to create FMP4 header: {}", err);
            gst::FlowError::Error
        })?;

        {
            let buffer = buffer.get_mut().unwrap();

            // No timestamps

            // Header is DISCONT|HEADER
            buffer.set_flags(gst::BufferFlags::DISCONT | gst::BufferFlags::HEADER);
        }

        // Remember stream header for later
        state.stream_header = Some(buffer.clone());

        let variant = match variant {
            super::Variant::ISO | super::Variant::DASH => "iso-fragmented",
            super::Variant::CMAF => "cmaf",
        };
        let caps = gst::Caps::builder("video/quicktime")
            .field("variant", variant)
            .field("streamheader", gst::Array::new(&[&buffer]))
            .build();

        let mut list = gst::BufferList::new_sized(1);
        {
            let list = list.get_mut().unwrap();
            list.add(buffer);
        }

        Ok(Some((list, caps)))
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        element: &super::FMP4Mux,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();

        let mut upstream_events = vec![];

        let buffers = {
            let mut state = self.state.lock().unwrap();

            let pts = buffer.pts();

            // Queue up the buffer and update GOP tracking state
            self.queue_input(element, &mut state, buffer)?;

            // If we have a PTS with this buffer, check if a new force-keyunit event for the next
            // fragment start has to be created
            if let Some(pts) = pts {
                if let Some(event) =
                    self.create_force_keyunit_event(element, &mut state, &settings, pts)?
                {
                    upstream_events.push(event);
                }
            }

            // If enough GOPs were queued, drain and create the output fragment
            self.drain(element, &mut state, &settings, false)?
        };

        for event in upstream_events {
            self.sinkpad.push_event(event);
        }

        if let Some(buffers) = buffers {
            gst_trace!(CAT, obj: element, "Pushing buffer list {:?}", buffers);
            self.srcpad.push_list(buffers)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::FMP4Mux, mut event: gst::Event) -> bool {
        use gst::EventView;

        gst_trace!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Segment(ev) => {
                let segment = match ev.segment().downcast_ref::<gst::ClockTime>() {
                    Some(segment) => {
                        gst_info!(CAT, obj: pad, "Received segment {:?}", segment);
                        segment.clone()
                    }
                    None => {
                        gst_warning!(
                            CAT,
                            obj: pad,
                            "Received non-TIME segment, replacing with default TIME segment"
                        );
                        let segment = gst::FormattedSegment::new();
                        event = gst::event::Segment::builder(&segment)
                            .seqnum(event.seqnum())
                            .build();
                        segment
                    }
                };

                self.state.lock().unwrap().segment = Some(segment);

                self.srcpad.push_event(event)
            }
            EventView::Caps(ev) => {
                let caps = ev.caps_owned();

                gst_info!(CAT, obj: pad, "Received caps {:?}", caps);
                let caps = {
                    let settings = self.settings.lock().unwrap().clone();
                    let mut state = self.state.lock().unwrap();

                    let s = caps.structure(0).unwrap();

                    match s.name() {
                        "video/x-h264" | "video/x-h265" => {
                            if !s.has_field_with_type("codec_data", gst::Buffer::static_type()) {
                                gst_error!(CAT, obj: pad, "Received caps without codec_data");
                                return false;
                            }
                        }
                        "audio/mpeg" => {
                            if !s.has_field_with_type("codec_data", gst::Buffer::static_type()) {
                                gst_error!(CAT, obj: pad, "Received caps without codec_data");
                                return false;
                            }
                            state.intra_only = true;
                        }
                        _ => unreachable!(),
                    }

                    state.caps = Some(caps);

                    let (_, caps) = match self.update_header(element, &mut state, &settings, false)
                    {
                        Ok(Some(res)) => res,
                        _ => {
                            return false;
                        }
                    };

                    caps
                };

                self.srcpad.push_event(gst::event::Caps::new(&caps))
            }
            EventView::Tag(_ev) => {
                // TODO: Maybe store for putting into the headers of the next fragment?

                pad.event_default(Some(element), event)
            }
            EventView::Gap(_ev) => {
                // TODO: queue up and check if draining is needed now
                // i.e. make the last sample much longer
                true
            }
            EventView::Eos(_ev) => {
                let settings = self.settings.lock().unwrap().clone();

                let drained = self.drain(element, &mut self.state.lock().unwrap(), &settings, true);
                let update_header =
                    drained.is_ok() && settings.header_update_mode != super::HeaderUpdateMode::None;

                match drained {
                    Ok(Some(buffers)) => {
                        gst_trace!(CAT, obj: element, "Pushing buffer list {:?}", buffers);

                        if let Err(err) = self.srcpad.push_list(buffers) {
                            gst_error!(
                                CAT,
                                obj: element,
                                "Failed pushing EOS buffers downstream: {:?}",
                                err,
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        gst_error!(CAT, obj: element, "Failed draining at EOS: {:?}", err);
                    }
                }

                if update_header {
                    let updated_header = self.update_header(
                        element,
                        &mut self.state.lock().unwrap(),
                        &settings,
                        true,
                    );
                    match updated_header {
                        Ok(Some((buffer_list, caps))) => {
                            match settings.header_update_mode {
                                super::HeaderUpdateMode::None => unreachable!(),
                                super::HeaderUpdateMode::Rewrite => {
                                    let mut q = gst::query::Seeking::new(gst::Format::Bytes);
                                    if self.srcpad.peer_query(&mut q) && q.result().0 {
                                        // Seek to the beginning with a default bytes segment
                                        self.srcpad.push_event(gst::event::Segment::new(
                                            &gst::FormattedSegment::<gst::format::Bytes>::new(),
                                        ));

                                        self.srcpad.push_event(gst::event::Caps::new(&caps));
                                        if let Err(err) = self.srcpad.push_list(buffer_list) {
                                            gst_error!(
                                                CAT,
                                                obj: element,
                                                "Failed pushing updated header buffer downstream: {:?}",
                                                err,
                                            );
                                        }
                                    } else {
                                        gst_error!(CAT, obj: element, "Can't rewrite header because downstream is not seekable");
                                    }
                                }
                                super::HeaderUpdateMode::Update => {
                                    self.srcpad.push_event(gst::event::Caps::new(&caps));
                                    if let Err(err) = self.srcpad.push_list(buffer_list) {
                                        gst_error!(
                                            CAT,
                                            obj: element,
                                            "Failed pushing updated header buffer downstream: {:?}",
                                            err,
                                        );
                                    }
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            gst_error!(
                                CAT,
                                obj: element,
                                "Failed to generate updated header: {:?}",
                                err
                            );
                        }
                    }
                }

                pad.event_default(Some(element), event)
            }
            EventView::FlushStop(_ev) => {
                let mut state = self.state.lock().unwrap();

                state.segment = None;
                state.queued_gops.clear();
                state.queued_duration = gst::ClockTime::ZERO;
                state.dts_offset = None;
                state.last_force_keyunit_time = None;
                state.current_offset = 0;
                state.fragment_offsets.clear();

                pad.event_default(Some(element), event)
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn sink_query(
        &self,
        pad: &gst::Pad,
        element: &super::FMP4Mux,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_trace!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Caps(mut q) => {
                let state = self.state.lock().unwrap();

                let allowed_caps = if let Some(ref caps) = state.caps {
                    // TODO: Maybe allow codec_data changes and similar?
                    caps.clone()
                } else {
                    pad.pad_template_caps()
                };

                if let Some(filter_caps) = q.filter() {
                    let res = filter_caps
                        .intersect_with_mode(&allowed_caps, gst::CapsIntersectMode::First);
                    q.set_result(&res);
                } else {
                    q.set_result(&allowed_caps);
                }

                true
            }
            _ => pad.query_default(Some(element), query),
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &super::FMP4Mux, event: gst::Event) -> bool {
        use gst::EventView;

        gst_trace!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Seek(_ev) => false,
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        element: &super::FMP4Mux,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_trace!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Seeking(mut q) => {
                // We can't really handle seeking, it would break everything
                q.set(false, gst::ClockTime::ZERO.into(), gst::ClockTime::NONE);
                true
            }
            QueryView::Latency(mut q) => {
                if !self.sinkpad.peer_query(q.query_mut()) {
                    return false;
                }

                let settings = self.settings.lock().unwrap();
                let (live, min, max) = q.result();
                gst_info!(
                    CAT,
                    obj: pad,
                    "Upstream latency: live {}, min {}, max {}",
                    live,
                    min,
                    max.display()
                );
                let (min, max) = (
                    min + settings.fragment_duration,
                    max.opt_add(settings.fragment_duration),
                );
                gst_info!(
                    CAT,
                    obj: pad,
                    "Returning latency: live {}, min {}, max {}",
                    live,
                    min,
                    max.display()
                );
                q.set(live, min, max);

                true
            }
            _ => pad.query_default(Some(element), query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for FMP4Mux {
    const NAME: &'static str = "GstFMP4Mux";
    type Type = super::FMP4Mux;
    type ParentType = gst::Element;
    type Class = Class;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                FMP4Mux::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |fmp4mux, element| fmp4mux.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                FMP4Mux::catch_panic_pad_function(
                    parent,
                    || false,
                    |fmp4mux, element| fmp4mux.sink_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                FMP4Mux::catch_panic_pad_function(
                    parent,
                    || false,
                    |fmp4mux, element| fmp4mux.sink_query(pad, element, query),
                )
            })
            .flags(gst::PadFlags::ACCEPT_INTERSECT)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .event_function(|pad, parent, event| {
                FMP4Mux::catch_panic_pad_function(
                    parent,
                    || false,
                    |fmp4mux, element| fmp4mux.src_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                FMP4Mux::catch_panic_pad_function(
                    parent,
                    || false,
                    |fmp4mux, element| fmp4mux.src_query(pad, element, query),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS | gst::PadFlags::ACCEPT_TEMPLATE)
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Mutex::default(),
            state: Mutex::default(),
        }
    }
}

impl ObjectImpl for FMP4Mux {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                // TODO: Add chunk-duration property separate from fragment-size
                glib::ParamSpecUInt64::new(
                    "fragment-duration",
                    "Fragment Duration",
                    "Duration for each FMP4 fragment",
                    0,
                    u64::MAX,
                    DEFAULT_FRAGMENT_DURATION.nseconds(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecEnum::new(
                    "header-update-mode",
                    "Header update mode",
                    "Mode for updating the header at the end of the stream",
                    super::HeaderUpdateMode::static_type(),
                    DEFAULT_HEADER_UPDATE_MODE as i32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecBoolean::new(
                    "write-mfra",
                    "Write mfra box",
                    "Write fragment random access box at the end of the stream",
                    DEFAULT_WRITE_MFRA,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecBoolean::new(
                    "write-mehd",
                    "Write mehd box",
                    "Write movie extends header box with the duration at the end of the stream (needs a header-update-mode enabled)",
                    DEFAULT_WRITE_MFRA,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
            ]
        });

        &*PROPERTIES
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "fragment-duration" => {
                let mut settings = self.settings.lock().unwrap();
                settings.fragment_duration = value.get().expect("type checked upstream");
            }

            "header-update-mode" => {
                let mut settings = self.settings.lock().unwrap();
                settings.header_update_mode = value.get().expect("type checked upstream");
            }

            "write-mfra" => {
                let mut settings = self.settings.lock().unwrap();
                settings.write_mfra = value.get().expect("type checked upstream");
            }

            "write-mehd" => {
                let mut settings = self.settings.lock().unwrap();
                settings.write_mehd = value.get().expect("type checked upstream");
            }

            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "fragment-duration" => {
                let settings = self.settings.lock().unwrap();
                settings.fragment_duration.to_value()
            }

            "header-update-mode" => {
                let settings = self.settings.lock().unwrap();
                settings.header_update_mode.to_value()
            }

            "write-mfra" => {
                let settings = self.settings.lock().unwrap();
                settings.write_mfra.to_value()
            }

            "write-mehd" => {
                let settings = self.settings.lock().unwrap();
                settings.write_mehd.to_value()
            }

            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for FMP4Mux {}

impl ElementImpl for FMP4Mux {
    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        let res = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                *self.state.lock().unwrap() = State::default();
            }
            _ => (),
        }

        Ok(res)
    }
}

#[repr(C)]
pub(crate) struct Class {
    parent: gst::ffi::GstElementClass,
    variant: super::Variant,
}

unsafe impl ClassStruct for Class {
    type Type = FMP4Mux;
}

impl std::ops::Deref for Class {
    type Target = glib::Class<gst::Element>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*(&self.parent as *const _ as *const _) }
    }
}

unsafe impl<T: FMP4MuxImpl> IsSubclassable<T> for super::FMP4Mux {
    fn class_init(class: &mut glib::Class<Self>) {
        Self::parent_class_init::<T>(class);

        let class = class.as_mut();
        class.variant = T::VARIANT;
    }
}

pub(crate) trait FMP4MuxImpl: ElementImpl {
    const VARIANT: super::Variant;
}

#[derive(Default)]
pub(crate) struct ISOFMP4Mux;

#[glib::object_subclass]
impl ObjectSubclass for ISOFMP4Mux {
    const NAME: &'static str = "GstISOFMP4Mux";
    type Type = super::ISOFMP4Mux;
    type ParentType = super::FMP4Mux;
}

impl ObjectImpl for ISOFMP4Mux {}

impl GstObjectImpl for ISOFMP4Mux {}

impl ElementImpl for ISOFMP4Mux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "ISOFMP4Mux",
                "Codec/Muxer",
                "ISO fragmented MP4 muxer",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/quicktime")
                    .field("variant", "iso-fragmented")
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &[
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", gst::List::new(["avc", "avc3"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", gst::List::new(["hvc1", "hev1"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "raw")
                        .field("channels", gst::IntRange::new(1, u16::MAX as i32))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                ]
                .into_iter()
                .collect::<gst::Caps>(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl FMP4MuxImpl for ISOFMP4Mux {
    const VARIANT: super::Variant = super::Variant::ISO;
}

#[derive(Default)]
pub(crate) struct CMAFMux;

#[glib::object_subclass]
impl ObjectSubclass for CMAFMux {
    const NAME: &'static str = "GstCMAFMux";
    type Type = super::CMAFMux;
    type ParentType = super::FMP4Mux;
}

impl ObjectImpl for CMAFMux {}

impl GstObjectImpl for CMAFMux {}

impl ElementImpl for CMAFMux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "CMAFMux",
                "Codec/Muxer",
                "CMAF fragmented MP4 muxer",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/quicktime")
                    .field("variant", "cmaf")
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &[
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", gst::List::new(["avc", "avc3"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", gst::List::new(["hvc1", "hev1"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "raw")
                        .field("channels", gst::IntRange::new(1, u16::MAX as i32))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                ]
                .into_iter()
                .collect::<gst::Caps>(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl FMP4MuxImpl for CMAFMux {
    const VARIANT: super::Variant = super::Variant::CMAF;
}

#[derive(Default)]
pub(crate) struct DASHMP4Mux;

#[glib::object_subclass]
impl ObjectSubclass for DASHMP4Mux {
    const NAME: &'static str = "GstDASHMP4Mux";
    type Type = super::DASHMP4Mux;
    type ParentType = super::FMP4Mux;
}

impl ObjectImpl for DASHMP4Mux {}

impl GstObjectImpl for DASHMP4Mux {}

impl ElementImpl for DASHMP4Mux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "DASHMP4Mux",
                "Codec/Muxer",
                "DASH fragmented MP4 muxer",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/quicktime")
                    .field("variant", "iso-fragmented")
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &[
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", gst::List::new(&[&"avc", &"avc3"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", gst::List::new(&[&"hvc1", &"hev1"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "raw")
                        .field("channels", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .field("rate", gst::IntRange::<i32>::new(1, i32::MAX))
                        .build(),
                ]
                .into_iter()
                .collect::<gst::Caps>(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl FMP4MuxImpl for DASHMP4Mux {
    const VARIANT: super::Variant = super::Variant::DASH;
}
