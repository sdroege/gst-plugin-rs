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
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use std::collections::VecDeque;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use super::boxes;
use super::Buffer;

/// Offset for the segment in non-single-stream variants.
const SEGMENT_OFFSET: gst::ClockTime = gst::ClockTime::from_seconds(60 * 60 * 1000);

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
const DEFAULT_INTERLEAVE_BYTES: Option<u64> = None;
const DEFAULT_INTERLEAVE_TIME: Option<gst::ClockTime> = Some(gst::ClockTime::from_mseconds(250));

#[derive(Debug, Clone)]
struct Settings {
    fragment_duration: gst::ClockTime,
    header_update_mode: super::HeaderUpdateMode,
    write_mfra: bool,
    write_mehd: bool,
    interleave_bytes: Option<u64>,
    interleave_time: Option<gst::ClockTime>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            fragment_duration: DEFAULT_FRAGMENT_DURATION,
            header_update_mode: DEFAULT_HEADER_UPDATE_MODE,
            write_mfra: DEFAULT_WRITE_MFRA,
            write_mehd: DEFAULT_WRITE_MEHD,
            interleave_bytes: DEFAULT_INTERLEAVE_BYTES,
            interleave_time: DEFAULT_INTERLEAVE_TIME,
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
    // Once this is known to be the final end PTS/DTS
    final_end_pts: bool,
    // DTS plus duration of last buffer, or start of next GOP
    end_dts: Option<gst::ClockTime>,

    // Buffer positions
    earliest_pts_position: gst::ClockTime,
    start_dts_position: Option<gst::ClockTime>,

    // Buffer, PTS running time, DTS running time
    buffers: Vec<Buffer>,
}

struct Stream {
    sinkpad: gst_base::AggregatorPad,

    caps: gst::Caps,
    intra_only: bool,

    queued_gops: VecDeque<Gop>,
    fragment_filled: bool,

    // Difference between the first DTS and 0 in case of negative DTS
    dts_offset: Option<gst::ClockTime>,

    last_force_keyunit_time: Option<gst::ClockTime>,
}

#[derive(Default)]
struct State {
    streams: Vec<Stream>,

    // Created once we received caps and kept up to date with the caps,
    // sent as part of the buffer list for the first fragment.
    stream_header: Option<gst::Buffer>,

    sequence_number: u32,

    // Fragment tracking for mfra
    current_offset: u64,
    fragment_offsets: Vec<super::FragmentOffset>,

    // Start / end PTS of the whole stream
    earliest_pts: Option<gst::ClockTime>,
    end_pts: Option<gst::ClockTime>,

    // Start PTS of the current fragment
    fragment_start_pts: Option<gst::ClockTime>,

    sent_headers: bool,
}

#[derive(Default)]
pub(crate) struct FMP4Mux {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl FMP4Mux {
    fn queue_input(
        &self,
        element: &super::FMP4Mux,
        idx: usize,
        stream: &mut Stream,
        segment: &gst::FormattedSegment<gst::ClockTime>,
        mut buffer: gst::Buffer,
    ) -> Result<(), gst::FlowError> {
        assert!(!stream.fragment_filled);

        gst::trace!(CAT, obj: &stream.sinkpad, "Handling buffer {:?}", buffer);

        let intra_only = stream.intra_only;

        if !intra_only && buffer.dts().is_none() {
            gst::error!(CAT, obj: &stream.sinkpad, "Require DTS for video streams");
            return Err(gst::FlowError::Error);
        }

        if intra_only && buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            gst::error!(CAT, obj: &stream.sinkpad, "Intra-only stream with delta units");
            return Err(gst::FlowError::Error);
        }

        let pts_position = buffer.pts().ok_or_else(|| {
            gst::error!(CAT, obj: &stream.sinkpad, "Require timestamped buffers");
            gst::FlowError::Error
        })?;
        let duration = buffer.duration();
        let end_pts_position = duration.map_or(pts_position, |duration| pts_position + duration);

        let pts = match segment.to_running_time_full(pts_position) {
            (_, None) => {
                gst::error!(CAT, obj: &stream.sinkpad, "Couldn't convert PTS to running time");
                return Err(gst::FlowError::Error);
            }
            (pts_signum, _) if pts_signum < 0 => {
                gst::error!(CAT, obj: &stream.sinkpad, "Negative PTSs are not supported");
                return Err(gst::FlowError::Error);
            }
            (_, Some(pts)) => pts,
        };

        let end_pts = match segment.to_running_time_full(end_pts_position) {
            (_, None) => {
                gst::error!(
                    CAT,
                    obj: &stream.sinkpad,
                    "Couldn't convert end PTS to running time"
                );
                return Err(gst::FlowError::Error);
            }
            (pts_signum, _) if pts_signum < 0 => {
                gst::error!(CAT, obj: &stream.sinkpad, "Negative PTSs are not supported");
                return Err(gst::FlowError::Error);
            }
            (_, Some(pts)) => pts,
        };

        let (dts_position, dts, end_dts) = if intra_only {
            (None, None, None)
        } else {
            // Negative DTS are handled via the dts_offset and by having negative composition time
            // offsets in the `trun` box. The smallest DTS here is shifted to zero.
            let dts_position = buffer.dts().expect("not DTS");
            let end_dts_position =
                duration.map_or(dts_position, |duration| dts_position + duration);

            let dts = match segment.to_running_time_full(dts_position) {
                (_, None) => {
                    gst::error!(CAT, obj: &stream.sinkpad, "Couldn't convert DTS to running time");
                    return Err(gst::FlowError::Error);
                }
                (pts_signum, Some(dts)) if pts_signum < 0 => {
                    if stream.dts_offset.is_none() {
                        stream.dts_offset = Some(dts);
                    }

                    let dts_offset = stream.dts_offset.unwrap();
                    if dts > dts_offset {
                        gst::warning!(CAT, obj: &stream.sinkpad, "DTS before first DTS");
                        gst::ClockTime::ZERO
                    } else {
                        dts_offset - dts
                    }
                }
                (_, Some(dts)) => {
                    if let Some(dts_offset) = stream.dts_offset {
                        dts + dts_offset
                    } else {
                        dts
                    }
                }
            };

            let end_dts = match segment.to_running_time_full(end_dts_position) {
                (_, None) => {
                    gst::error!(
                        CAT,
                        obj: &stream.sinkpad,
                        "Couldn't convert end DTS to running time"
                    );
                    return Err(gst::FlowError::Error);
                }
                (pts_signum, Some(dts)) if pts_signum < 0 => {
                    if stream.dts_offset.is_none() {
                        stream.dts_offset = Some(dts);
                    }

                    let dts_offset = stream.dts_offset.unwrap();
                    if dts > dts_offset {
                        gst::warning!(CAT, obj: &stream.sinkpad, "End DTS before first DTS");
                        gst::ClockTime::ZERO
                    } else {
                        dts_offset - dts
                    }
                }
                (_, Some(dts)) => {
                    if let Some(dts_offset) = stream.dts_offset {
                        dts + dts_offset
                    } else {
                        dts
                    }
                }
            };
            (Some(dts_position), Some(dts), Some(end_dts))
        };

        // If this is a multi-stream element then we need to update the PTS/DTS positions according
        // to the output segment, specifically to re-timestamp them with the running time and
        // adjust for the segment shift to compensate for negative DTS.
        let class = element.class();
        let (pts_position, dts_position) = if class.as_ref().variant.is_single_stream() {
            (pts_position, dts_position)
        } else {
            let pts_position = pts + SEGMENT_OFFSET;
            let dts_position = dts.map(|dts| {
                dts + SEGMENT_OFFSET - stream.dts_offset.unwrap_or(gst::ClockTime::ZERO)
            });

            let buffer = buffer.make_mut();
            buffer.set_pts(pts_position);
            buffer.set_dts(dts_position);

            (pts_position, dts_position)
        };

        if !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            gst::debug!(
                CAT,
                obj: &stream.sinkpad,
                "Starting new GOP at PTS {} DTS {} (DTS offset {})",
                pts,
                dts.display(),
                stream.dts_offset.display(),
            );

            let gop = Gop {
                start_pts: pts,
                start_dts: dts,
                start_dts_position: if intra_only { None } else { dts_position },
                earliest_pts: pts,
                earliest_pts_position: pts_position,
                final_earliest_pts: intra_only,
                end_pts,
                end_dts,
                final_end_pts: false,
                buffers: vec![Buffer {
                    idx,
                    buffer,
                    pts,
                    dts,
                }],
            };
            stream.queued_gops.push_front(gop);

            if let Some(prev_gop) = stream.queued_gops.get_mut(1) {
                gst::debug!(
                    CAT,
                    obj: &stream.sinkpad,
                    "Updating previous GOP starting at PTS {} to end PTS {} DTS {}",
                    prev_gop.earliest_pts,
                    pts,
                    dts.display(),
                );
                prev_gop.end_pts = pts;
                prev_gop.end_dts = dts;

                if intra_only {
                    prev_gop.final_end_pts = true;
                }

                if !prev_gop.final_earliest_pts {
                    // Don't bother logging this for intra-only streams as it would be for every
                    // single buffer.
                    if !intra_only {
                        gst::debug!(
                            CAT,
                            obj: &stream.sinkpad,
                            "Previous GOP has final earliest PTS at {}",
                            prev_gop.earliest_pts
                        );
                    }

                    prev_gop.final_earliest_pts = true;
                    if let Some(prev_prev_gop) = stream.queued_gops.get_mut(2) {
                        prev_prev_gop.final_end_pts = true;
                    }
                }
            }
        } else if let Some(gop) = stream.queued_gops.front_mut() {
            assert!(!intra_only);

            // We require DTS for non-intra-only streams
            let dts = dts.unwrap();
            let end_dts = end_dts.unwrap();

            gop.end_pts = std::cmp::max(gop.end_pts, end_pts);
            gop.end_dts = Some(std::cmp::max(gop.end_dts.expect("no end DTS"), end_dts));
            gop.buffers.push(Buffer {
                idx,
                buffer,
                pts,
                dts: Some(dts),
            });

            if gop.earliest_pts > pts && !gop.final_earliest_pts {
                gst::debug!(
                    CAT,
                    obj: &stream.sinkpad,
                    "Updating current GOP earliest PTS from {} to {}",
                    gop.earliest_pts,
                    pts
                );
                gop.earliest_pts = pts;
                gop.earliest_pts_position = pts_position;

                if let Some(prev_gop) = stream.queued_gops.get_mut(1) {
                    gst::debug!(
                        CAT,
                        obj: &stream.sinkpad,
                        "Updating previous GOP starting PTS {} end time from {} to {}",
                        pts,
                        prev_gop.end_pts,
                        pts
                    );
                    prev_gop.end_pts = pts;
                }
            }

            let gop = stream.queued_gops.front_mut().unwrap();

            // The earliest PTS is known when the current DTS is bigger or equal to the first
            // PTS that was observed in this GOP. If there was another frame later that had a
            // lower PTS then it wouldn't be possible to display it in time anymore, i.e. the
            // stream would be invalid.
            if gop.start_pts <= dts && !gop.final_earliest_pts {
                gst::debug!(
                    CAT,
                    obj: &stream.sinkpad,
                    "GOP has final earliest PTS at {}",
                    gop.earliest_pts
                );
                gop.final_earliest_pts = true;

                if let Some(prev_gop) = stream.queued_gops.get_mut(1) {
                    prev_gop.final_end_pts = true;
                }
            }
        } else {
            gst::warning!(
                CAT,
                obj: &stream.sinkpad,
                "Waiting for keyframe at the beginning of the stream"
            );
        }

        if let Some((prev_gop, first_gop)) = Option::zip(
            stream.queued_gops.iter().find(|gop| gop.final_end_pts),
            stream.queued_gops.back(),
        ) {
            gst::debug!(
                CAT,
                obj: &stream.sinkpad,
                "Queued full GOPs duration updated to {}",
                prev_gop.end_pts.saturating_sub(first_gop.earliest_pts),
            );
        }

        gst::debug!(
            CAT,
            obj: &stream.sinkpad,
            "Queued duration updated to {}",
            Option::zip(stream.queued_gops.front(), stream.queued_gops.back())
                .map(|(end, start)| end.end_pts.saturating_sub(start.start_pts))
                .unwrap_or(gst::ClockTime::ZERO)
        );

        Ok(())
    }

    fn create_initial_force_keyunit_event(
        &self,
        _element: &super::FMP4Mux,
        stream: &mut Stream,
        settings: &Settings,
        earliest_pts: gst::ClockTime,
    ) -> Result<Option<gst::Event>, gst::FlowError> {
        assert!(stream.last_force_keyunit_time.is_none());

        // If we never sent a force-keyunit event then send one now.
        let fku_running_time = earliest_pts + settings.fragment_duration;
        gst::debug!(
            CAT,
            obj: &stream.sinkpad,
            "Sending first force-keyunit event for running time {}",
            fku_running_time
        );
        stream.last_force_keyunit_time = Some(fku_running_time);

        return Ok(Some(
            gst_video::UpstreamForceKeyUnitEvent::builder()
                .running_time(fku_running_time)
                .all_headers(true)
                .build(),
        ));
    }

    fn create_force_keyunit_event(
        &self,
        _element: &super::FMP4Mux,
        stream: &mut Stream,
        settings: &Settings,
        segment: &gst::FormattedSegment<gst::ClockTime>,
        pts: gst::ClockTime,
    ) -> Result<Option<gst::Event>, gst::FlowError> {
        // If we never sent a force-keyunit event then wait until the earliest PTS of the first GOP
        // is known and send it then.
        //
        // Otherwise if the current PTS is a fragment duration in the future, send the next one
        // now.
        let pts = segment.to_running_time(pts).expect("no running time");

        let last_force_keyunit_time = match stream.last_force_keyunit_time {
            None => return Ok(None),
            Some(last_force_keyunit_time) if last_force_keyunit_time > pts => return Ok(None),
            Some(last_force_keyunit_time) => last_force_keyunit_time,
        };

        let fku_running_time = last_force_keyunit_time + settings.fragment_duration;
        gst::debug!(
            CAT,
            obj: &stream.sinkpad,
            "Sending force-keyunit event for running time {}",
            fku_running_time
        );
        stream.last_force_keyunit_time = Some(fku_running_time);

        Ok(Some(
            gst_video::UpstreamForceKeyUnitEvent::builder()
                .running_time(fku_running_time)
                .all_headers(true)
                .build(),
        ))
    }

    fn drain(
        &self,
        element: &super::FMP4Mux,
        state: &mut State,
        settings: &Settings,
        timeout: bool,
        at_eos: bool,
    ) -> Result<Option<gst::BufferList>, gst::FlowError> {
        let class = element.class();

        if at_eos {
            gst::info!(CAT, obj: element, "Draining at EOS");
        } else if timeout {
            gst::info!(CAT, obj: element, "Draining at timeout");
        } else {
            for stream in &state.streams {
                if !stream.fragment_filled && !stream.sinkpad.is_eos() {
                    return Ok(None);
                }
            }
        }

        let mut drain_buffers = Vec::with_capacity(state.streams.len());
        let mut timing_infos = Vec::with_capacity(state.streams.len());
        let mut caps = Vec::with_capacity(state.streams.len());

        let mut min_earliest_pts_position = None;
        let mut min_earliest_pts = None;
        let mut min_start_dts_position = None;
        let mut max_end_pts = None;

        for stream in &mut state.streams {
            assert!(
                timeout
                    || at_eos
                    || stream.sinkpad.is_eos()
                    || stream.queued_gops.get(1).map(|gop| gop.final_earliest_pts) == Some(true)
            );

            // At EOS, finalize all GOPs and drain them out. Otherwise if the queued duration is
            // equal to the fragment duration then drain out all complete GOPs, otherwise all
            // except for the newest complete GOP.
            let gops = if at_eos || stream.sinkpad.is_eos() {
                stream.queued_gops.drain(..).rev().collect::<Vec<_>>()
            } else {
                let mut gops = vec![];

                let fragment_start_pts = state.fragment_start_pts.unwrap();
                while let Some(gop) = stream.queued_gops.pop_back() {
                    assert!(timeout || gop.final_end_pts);

                    let end_pts = gop.end_pts;
                    gops.push(gop);
                    if end_pts.saturating_sub(fragment_start_pts) >= settings.fragment_duration {
                        break;
                    }
                }

                gops
            };
            stream.fragment_filled = false;

            if gops.is_empty() {
                timing_infos.push(None);
            } else {
                let first_gop = gops.first().unwrap();
                let last_gop = gops.last().unwrap();
                let earliest_pts = first_gop.earliest_pts;
                let earliest_pts_position = first_gop.earliest_pts_position;
                let start_dts = first_gop.start_dts;
                let start_dts_position = first_gop.start_dts_position;
                let end_pts = last_gop.end_pts;
                let end_dts = last_gop.end_dts;
                let dts_offset = stream.dts_offset;

                if min_earliest_pts.map_or(true, |min| min > earliest_pts) {
                    min_earliest_pts = Some(earliest_pts);
                }
                if min_earliest_pts_position.map_or(true, |min| min > earliest_pts_position) {
                    min_earliest_pts_position = Some(earliest_pts_position);
                }
                if let Some(start_dts_position) = start_dts_position {
                    if min_start_dts_position.map_or(true, |min| min > start_dts_position) {
                        min_start_dts_position = Some(start_dts_position);
                    }
                }
                if max_end_pts.map_or(true, |max| max < end_pts) {
                    max_end_pts = Some(end_pts);
                }

                gst::info!(
                    CAT,
                    obj: &stream.sinkpad,
                    "Draining {} worth of buffers starting at PTS {} DTS {}, DTS offset {}",
                    end_pts.saturating_sub(earliest_pts),
                    earliest_pts,
                    start_dts.display(),
                    dts_offset.display(),
                );

                if let Some((prev_gop, first_gop)) = Option::zip(
                    stream.queued_gops.iter().find(|gop| gop.final_end_pts),
                    stream.queued_gops.back(),
                ) {
                    gst::debug!(
                        CAT,
                        obj: &stream.sinkpad,
                        "Queued full GOPs duration updated to {}",
                        prev_gop.end_pts.saturating_sub(first_gop.earliest_pts),
                    );
                }

                gst::debug!(
                    CAT,
                    obj: &stream.sinkpad,
                    "Queued duration updated to {}",
                    Option::zip(stream.queued_gops.front(), stream.queued_gops.back())
                        .map(|(end, start)| end.end_pts.saturating_sub(start.start_pts))
                        .unwrap_or(gst::ClockTime::ZERO)
                );

                timing_infos.push(Some(super::FragmentTimingInfo {
                    earliest_pts,
                    start_dts,
                    end_pts,
                    end_dts,
                    dts_offset,
                }));
            }

            caps.push(&stream.caps);

            let mut buffers = VecDeque::with_capacity(gops.iter().map(|g| g.buffers.len()).sum());
            for gop in gops {
                for buffer in gop.buffers {
                    buffers.push_back(buffer);
                }
            }
            drain_buffers.push(buffers);
        }

        // Interleave buffers according to the settings into a single vec
        let mut interleaved_buffers =
            Vec::with_capacity(drain_buffers.iter().map(|bs| bs.len()).sum());
        while let Some((idx, bs)) =
            drain_buffers
                .iter_mut()
                .enumerate()
                .min_by(|(a_idx, a), (b_idx, b)| {
                    let (a, b) = match (a.front(), b.front()) {
                        (None, None) => return std::cmp::Ordering::Equal,
                        (None, _) => return std::cmp::Ordering::Greater,
                        (_, None) => return std::cmp::Ordering::Less,
                        (Some(a), Some(b)) => (a, b),
                    };

                    match a.dts.unwrap_or(a.pts).cmp(&b.dts.unwrap_or(b.pts)) {
                        std::cmp::Ordering::Equal => a_idx.cmp(b_idx),
                        cmp => cmp,
                    }
                })
        {
            let start_time = match bs.front() {
                None => {
                    // No more buffers now
                    break;
                }
                Some(buf) => buf.dts.unwrap_or(buf.pts),
            };
            let mut current_end_time = start_time;
            let mut dequeued_bytes = 0;

            while settings
                .interleave_bytes
                .map_or(true, |max_bytes| dequeued_bytes <= max_bytes)
                && settings.interleave_time.map_or(true, |max_time| {
                    current_end_time.saturating_sub(start_time) <= max_time
                })
            {
                if let Some(buffer) = bs.pop_front() {
                    current_end_time = match bs.front() {
                        Some(next_buffer) => next_buffer.dts.unwrap_or(next_buffer.pts),
                        None => {
                            let timing_info = timing_infos[idx].as_ref().unwrap();
                            timing_info.end_dts.unwrap_or(timing_info.end_pts)
                        }
                    };
                    dequeued_bytes += buffer.buffer.size() as u64;
                    interleaved_buffers.push(buffer);
                } else {
                    // No buffers left in this stream, go to next stream
                    break;
                }
            }
        }

        assert!(drain_buffers.iter().all(|bs| bs.is_empty()));

        let mut buffer_list = None;

        if interleaved_buffers.is_empty() {
            assert!(timeout || at_eos);
        } else {
            // Remove all GAP buffers before writing them out
            interleaved_buffers.retain(|buf| {
                !buf.buffer.flags().contains(gst::BufferFlags::GAP)
                    || !buf.buffer.flags().contains(gst::BufferFlags::DROPPABLE)
                    || buf.buffer.size() != 0
            });

            let min_earliest_pts_position = min_earliest_pts_position.unwrap();
            let min_earliest_pts = min_earliest_pts.unwrap();
            let max_end_pts = max_end_pts.unwrap();

            let mut fmp4_header = None;
            if !state.sent_headers {
                let mut buffer = state.stream_header.as_ref().unwrap().copy();
                {
                    let buffer = buffer.get_mut().unwrap();

                    buffer.set_pts(min_earliest_pts_position);
                    buffer.set_dts(min_start_dts_position);

                    // Header is DISCONT|HEADER
                    buffer.set_flags(gst::BufferFlags::DISCONT | gst::BufferFlags::HEADER);
                }

                fmp4_header = Some(buffer);

                state.sent_headers = true;
            }

            // TODO: Write prft boxes before moof
            // TODO: Write sidx boxes before moof and rewrite once offsets are known

            if state.sequence_number == 0 {
                state.sequence_number = 1;
            }
            let sequence_number = state.sequence_number;
            state.sequence_number += 1;
            let (mut fmp4_fragment_header, moof_offset) =
                boxes::create_fmp4_fragment_header(super::FragmentHeaderConfiguration {
                    variant: class.as_ref().variant,
                    sequence_number,
                    caps: caps.as_slice(),
                    timing_infos: timing_infos.as_slice(),
                    buffers: interleaved_buffers.as_slice(),
                })
                .map_err(|err| {
                    gst::error!(
                        CAT,
                        obj: element,
                        "Failed to create FMP4 fragment header: {}",
                        err
                    );
                    gst::FlowError::Error
                })?;

            {
                let buffer = fmp4_fragment_header.get_mut().unwrap();
                buffer.set_pts(min_earliest_pts_position);
                buffer.set_dts(min_start_dts_position);
                buffer.set_duration(max_end_pts.checked_sub(min_earliest_pts));

                // Fragment header is HEADER
                buffer.set_flags(gst::BufferFlags::HEADER);

                // Copy metas from the first actual buffer to the fragment header. This allows
                // getting things like the reference timestamp meta or the timecode meta to identify
                // the fragment.
                let _ = interleaved_buffers[0].buffer.copy_into(
                    buffer,
                    gst::BufferCopyFlags::META,
                    0,
                    None,
                );
            }

            let moof_offset = state.current_offset
                + fmp4_header.as_ref().map(|h| h.size()).unwrap_or(0) as u64
                + moof_offset;

            let buffers_len = interleaved_buffers.len();
            for (idx, buffer) in interleaved_buffers.iter_mut().enumerate() {
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
                    .chain(interleaved_buffers.into_iter().map(|buffer| buffer.buffer))
                    .inspect(|b| {
                        state.current_offset += b.size() as u64;
                    })
                    .collect::<gst::BufferList>(),
            );

            // Write mfra only for the main stream, and if there are no buffers for the main stream
            // in this segment then don't write anything.
            if let Some(Some(ref timing_info)) = timing_infos.get(0) {
                state.fragment_offsets.push(super::FragmentOffset {
                    time: timing_info.earliest_pts,
                    offset: moof_offset,
                });
            }
            state.end_pts = Some(max_end_pts);

            // Update for the start PTS of the next fragment
            state.fragment_start_pts = state
                .fragment_start_pts
                .map(|start| start + settings.fragment_duration);
        }

        if settings.write_mfra && at_eos {
            match boxes::create_mfra(caps[0], &state.fragment_offsets) {
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
                    gst::error!(CAT, obj: element, "Failed to create mfra box: {}", err);
                }
            }
        }

        // TODO: Write edit list at EOS
        // TODO: Rewrite bitrates at EOS

        Ok(buffer_list)
    }

    fn create_streams(
        &self,
        element: &super::FMP4Mux,
        state: &mut State,
    ) -> Result<(), gst::FlowError> {
        for pad in element
            .sink_pads()
            .into_iter()
            .map(|pad| pad.downcast::<gst_base::AggregatorPad>().unwrap())
        {
            let caps = match pad.current_caps() {
                Some(caps) => caps,
                None => {
                    gst::warning!(CAT, obj: &pad, "Skipping pad without caps");
                    continue;
                }
            };

            gst::info!(CAT, obj: &pad, "Configuring caps {:?}", caps);

            let s = caps.structure(0).unwrap();

            let mut intra_only = false;
            match s.name() {
                "video/x-h264" | "video/x-h265" => {
                    if !s.has_field_with_type("codec_data", gst::Buffer::static_type()) {
                        gst::error!(CAT, obj: &pad, "Received caps without codec_data");
                        return Err(gst::FlowError::NotNegotiated);
                    }
                }
                "image/jpeg" => {
                    intra_only = true;
                }
                "audio/mpeg" => {
                    if !s.has_field_with_type("codec_data", gst::Buffer::static_type()) {
                        gst::error!(CAT, obj: &pad, "Received caps without codec_data");
                        return Err(gst::FlowError::NotNegotiated);
                    }
                    intra_only = true;
                }
                "audio/x-alaw" | "audio/x-mulaw" => {
                    intra_only = true;
                }
                "audio/x-adpcm" => {
                    intra_only = true;
                }
                "application/x-onvif-metadata" => {
                    intra_only = true;
                }
                _ => unreachable!(),
            }

            state.streams.push(Stream {
                sinkpad: pad,
                caps,
                intra_only,
                queued_gops: VecDeque::new(),
                fragment_filled: false,
                dts_offset: None,
                last_force_keyunit_time: None,
            });
        }

        if state.streams.is_empty() {
            gst::error!(CAT, obj: element, "No streams available");
            return Err(gst::FlowError::Error);
        }

        // Sort video streams first and then audio streams and then metadata streams, and each group by pad name.
        state.streams.sort_by(|a, b| {
            let order_of_caps = |caps: &gst::CapsRef| {
                let s = caps.structure(0).unwrap();

                if s.name().starts_with("video/") {
                    0
                } else if s.name().starts_with("audio/") {
                    1
                } else if s.name().starts_with("application/x-onvif-metadata") {
                    2
                } else {
                    unimplemented!();
                }
            };

            let st_a = order_of_caps(&a.caps);
            let st_b = order_of_caps(&b.caps);

            if st_a == st_b {
                return a.sinkpad.name().cmp(&b.sinkpad.name());
            }

            st_a.cmp(&st_b)
        });

        Ok(())
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

        assert!(!at_eos || state.streams.iter().all(|s| s.queued_gops.is_empty()));

        let duration = state
            .end_pts
            .opt_checked_sub(state.earliest_pts)
            .ok()
            .flatten();

        let caps = state.streams.iter().map(|s| &s.caps).collect::<Vec<_>>();

        let mut buffer = boxes::create_fmp4_header(super::HeaderConfiguration {
            variant,
            update: at_eos,
            caps: caps.as_slice(),
            write_mehd: settings.write_mehd,
            duration: if at_eos { duration } else { None },
        })
        .map_err(|err| {
            gst::error!(CAT, obj: element, "Failed to create FMP4 header: {}", err);
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
            super::Variant::ISO | super::Variant::DASH | super::Variant::ONVIF => "iso-fragmented",
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
}

#[glib::object_subclass]
impl ObjectSubclass for FMP4Mux {
    const NAME: &'static str = "GstFMP4Mux";
    type Type = super::FMP4Mux;
    type ParentType = gst_base::Aggregator;
    type Class = Class;
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
                glib::ParamSpecUInt64::new(
                    "interleave-bytes",
                    "Interleave Bytes",
                    "Interleave between streams in bytes",
                    0,
                    u64::MAX,
                    DEFAULT_INTERLEAVE_BYTES.unwrap_or(0),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt64::new(
                    "interleave-time",
                    "Interleave Time",
                    "Interleave between streams in nanoseconds",
                    0,
                    u64::MAX,
                    DEFAULT_INTERLEAVE_TIME.map(gst::ClockTime::nseconds).unwrap_or(u64::MAX),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),

            ]
        });

        &*PROPERTIES
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "fragment-duration" => {
                let mut settings = self.settings.lock().unwrap();
                let fragment_duration = value.get().expect("type checked upstream");
                if settings.fragment_duration != fragment_duration {
                    settings.fragment_duration = fragment_duration;
                    drop(settings);
                    obj.set_latency(fragment_duration, None);
                }
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

            "interleave-bytes" => {
                let mut settings = self.settings.lock().unwrap();
                settings.interleave_bytes = match value.get().expect("type checked upstream") {
                    0 => None,
                    v => Some(v),
                };
            }

            "interleave-time" => {
                let mut settings = self.settings.lock().unwrap();
                settings.interleave_time = match value.get().expect("type checked upstream") {
                    Some(gst::ClockTime::ZERO) | None => None,
                    v => v,
                };
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

            "interleave-bytes" => {
                let settings = self.settings.lock().unwrap();
                settings.interleave_bytes.unwrap_or(0).to_value()
            }

            "interleave-time" => {
                let settings = self.settings.lock().unwrap();
                settings.interleave_time.to_value()
            }

            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        let class = obj.class();
        for templ in class.pad_template_list().filter(|templ| {
            templ.presence() == gst::PadPresence::Always
                && templ.direction() == gst::PadDirection::Sink
        }) {
            let sinkpad =
                gst::PadBuilder::<gst_base::AggregatorPad>::from_template(&templ, Some("sink"))
                    .flags(gst::PadFlags::ACCEPT_INTERSECT)
                    .build();

            obj.add_pad(&sinkpad).unwrap();
        }

        obj.set_latency(Settings::default().fragment_duration, None);
    }
}

impl GstObjectImpl for FMP4Mux {}

impl ElementImpl for FMP4Mux {
    fn request_new_pad(
        &self,
        element: &Self::Type,
        templ: &gst::PadTemplate,
        name: Option<String>,
        caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let state = self.state.lock().unwrap();
        if state.stream_header.is_some() {
            gst::error!(
                CAT,
                obj: element,
                "Can't request new pads after header was generated"
            );
            return None;
        }

        self.parent_request_new_pad(element, templ, name, caps)
    }
}

impl AggregatorImpl for FMP4Mux {
    fn next_time(&self, _aggregator: &Self::Type) -> Option<gst::ClockTime> {
        let state = self.state.lock().unwrap();
        state.fragment_start_pts
    }

    fn sink_query(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &gst_base::AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryViewMut;

        gst::trace!(CAT, obj: aggregator_pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryViewMut::Caps(q) => {
                let allowed_caps = aggregator_pad
                    .current_caps()
                    .unwrap_or_else(|| aggregator_pad.pad_template_caps());

                if let Some(filter_caps) = q.filter() {
                    let res = filter_caps
                        .intersect_with_mode(&allowed_caps, gst::CapsIntersectMode::First);
                    q.set_result(&res);
                } else {
                    q.set_result(&allowed_caps);
                }

                true
            }
            _ => self.parent_sink_query(aggregator, aggregator_pad, query),
        }
    }

    fn sink_event_pre_queue(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &gst_base::AggregatorPad,
        mut event: gst::Event,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        use gst::EventView;

        gst::trace!(CAT, obj: aggregator_pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Segment(ev) => {
                if ev.segment().format() != gst::Format::Time {
                    gst::warning!(
                        CAT,
                        obj: aggregator_pad,
                        "Received non-TIME segment, replacing with default TIME segment"
                    );
                    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
                    event = gst::event::Segment::builder(&segment)
                        .seqnum(event.seqnum())
                        .build();
                }
                self.parent_sink_event_pre_queue(aggregator, aggregator_pad, event)
            }
            _ => self.parent_sink_event_pre_queue(aggregator, aggregator_pad, event),
        }
    }

    fn sink_event(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &gst_base::AggregatorPad,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst::trace!(CAT, obj: aggregator_pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Segment(ev) => {
                // Already fixed-up above to always be a TIME segment
                let segment = ev
                    .segment()
                    .clone()
                    .downcast::<gst::ClockTime>()
                    .expect("non-TIME segment");
                gst::info!(CAT, obj: aggregator_pad, "Received segment {:?}", segment);

                // Only forward the segment event verbatim if this is a single stream variant.
                // Otherwise we have to produce a default segment and re-timestamp all buffers
                // with their running time.
                let class = aggregator.class();
                if class.as_ref().variant.is_single_stream() {
                    aggregator.update_segment(&segment);
                }

                self.parent_sink_event(aggregator, aggregator_pad, event)
            }
            EventView::Tag(_ev) => {
                // TODO: Maybe store for putting into the headers of the next fragment?

                self.parent_sink_event(aggregator, aggregator_pad, event)
            }
            _ => self.parent_sink_event(aggregator, aggregator_pad, event),
        }
    }

    fn src_query(&self, aggregator: &Self::Type, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        gst::trace!(CAT, obj: aggregator, "Handling query {:?}", query);

        match query.view_mut() {
            QueryViewMut::Seeking(q) => {
                // We can't really handle seeking, it would break everything
                q.set(false, gst::ClockTime::ZERO.into(), gst::ClockTime::NONE);
                true
            }
            _ => self.parent_src_query(aggregator, query),
        }
    }

    fn src_event(&self, aggregator: &Self::Type, event: gst::Event) -> bool {
        use gst::EventView;

        gst::trace!(CAT, obj: aggregator, "Handling event {:?}", event);

        match event.view() {
            EventView::Seek(_ev) => false,
            _ => self.parent_src_event(aggregator, event),
        }
    }

    fn flush(&self, aggregator: &Self::Type) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.parent_flush(aggregator)?;

        let mut state = self.state.lock().unwrap();

        for stream in &mut state.streams {
            stream.queued_gops.clear();
            stream.dts_offset = None;
            stream.last_force_keyunit_time = None;
            stream.fragment_filled = false;
        }

        state.current_offset = 0;
        state.fragment_offsets.clear();

        Ok(gst::FlowSuccess::Ok)
    }

    fn stop(&self, aggregator: &Self::Type) -> Result<(), gst::ErrorMessage> {
        gst::trace!(CAT, obj: aggregator, "Stopping");

        let _ = self.parent_stop(aggregator);

        *self.state.lock().unwrap() = State::default();

        Ok(())
    }

    fn start(&self, aggregator: &Self::Type) -> Result<(), gst::ErrorMessage> {
        gst::trace!(CAT, obj: aggregator, "Starting");

        self.parent_start(aggregator)?;

        // For non-single-stream variants configure a default segment that allows for negative
        // DTS so that we can correctly re-timestamp buffers with their running times.
        let class = aggregator.class();
        if !class.as_ref().variant.is_single_stream() {
            let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
            segment.set_start(SEGMENT_OFFSET);
            segment.set_position(SEGMENT_OFFSET);
            aggregator.update_segment(&segment);
        }

        *self.state.lock().unwrap() = State::default();

        Ok(())
    }

    fn negotiate(&self, _aggregator: &Self::Type) -> bool {
        true
    }

    fn aggregate(
        &self,
        aggregator: &Self::Type,
        timeout: bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();

        let mut all_eos = true;
        let mut upstream_events = vec![];

        let buffers = {
            let mut state = self.state.lock().unwrap();

            // Create streams, stream header in the beginning and set output caps.
            if state.stream_header.is_none() {
                self.create_streams(aggregator, &mut state)?;

                let (_, caps) = self
                    .update_header(aggregator, &mut state, &settings, false)?
                    .unwrap();

                drop(state);
                aggregator.set_src_caps(&caps);
                state = self.state.lock().unwrap();
            }

            // Queue buffers from all streams that are not filled for the current fragment yet
            let fragment_start_pts = state.fragment_start_pts;
            for (idx, stream) in state.streams.iter_mut().enumerate() {
                if stream.fragment_filled {
                    let buffer = stream.sinkpad.peek_buffer();
                    all_eos &= buffer.is_none() && stream.sinkpad.is_eos();

                    continue;
                }

                let buffer = stream.sinkpad.pop_buffer();
                all_eos &= buffer.is_none() && stream.sinkpad.is_eos();

                let buffer = match buffer {
                    None => continue,
                    Some(buffer) => buffer,
                };

                let segment = match stream
                    .sinkpad
                    .segment()
                    .clone()
                    .downcast::<gst::ClockTime>()
                    .ok()
                {
                    Some(segment) => segment,
                    None => {
                        gst::error!(CAT, obj: &stream.sinkpad, "Got buffer before segment");
                        return Err(gst::FlowError::Error);
                    }
                };

                let pts = buffer.pts();

                // Queue up the buffer and update GOP tracking state
                self.queue_input(aggregator, idx, stream, &segment, buffer)?;

                // If we have a PTS with this buffer, check if a new force-keyunit event for the next
                // fragment start has to be created
                if let Some(pts) = pts {
                    if let Some(event) = self
                        .create_force_keyunit_event(aggregator, stream, &settings, &segment, pts)?
                    {
                        upstream_events.push((stream.sinkpad.clone(), event));
                    }
                }

                // Check if this stream is filled enough now.
                if let Some((queued_end_pts, fragment_start_pts)) = Option::zip(
                    stream
                        .queued_gops
                        .iter()
                        .find(|gop| gop.final_end_pts)
                        .map(|gop| gop.end_pts),
                    fragment_start_pts,
                ) {
                    if queued_end_pts.saturating_sub(fragment_start_pts)
                        >= settings.fragment_duration
                    {
                        gst::debug!(CAT, obj: &stream.sinkpad, "Stream queued enough data for this fragment");
                        stream.fragment_filled = true;
                    }
                }
            }

            if all_eos {
                gst::debug!(CAT, obj: aggregator, "All streams are EOS now");
            }

            // Calculate the earliest PTS after queueing input if we can now.
            if state.earliest_pts.is_none() {
                let mut earliest_pts = None;

                for stream in &state.streams {
                    let stream_earliest_pts = match stream.queued_gops.back() {
                        None => {
                            earliest_pts = None;
                            break;
                        }
                        Some(oldest_gop) => {
                            if !timeout && !oldest_gop.final_earliest_pts {
                                earliest_pts = None;
                                break;
                            }

                            oldest_gop.earliest_pts
                        }
                    };

                    if earliest_pts.map_or(true, |earliest_pts| earliest_pts > stream_earliest_pts)
                    {
                        earliest_pts = Some(stream_earliest_pts);
                    }
                }

                if let Some(earliest_pts) = earliest_pts {
                    gst::info!(CAT, obj: aggregator, "Got earliest PTS {}", earliest_pts);
                    state.earliest_pts = Some(earliest_pts);
                    state.fragment_start_pts = Some(earliest_pts);

                    for stream in &mut state.streams {
                        if let Some(event) = self.create_initial_force_keyunit_event(
                            aggregator,
                            stream,
                            &settings,
                            earliest_pts,
                        )? {
                            upstream_events.push((stream.sinkpad.clone(), event));
                        }

                        // Check if this stream is filled enough now.
                        if let Some(queued_end_pts) = stream
                            .queued_gops
                            .iter()
                            .find(|gop| gop.final_end_pts)
                            .map(|gop| gop.end_pts)
                        {
                            if queued_end_pts.saturating_sub(earliest_pts)
                                >= settings.fragment_duration
                            {
                                gst::debug!(CAT, obj: &stream.sinkpad, "Stream queued enough data for this fragment");
                                stream.fragment_filled = true;
                            }
                        }
                    }
                }
            }

            // If enough GOPs were queued, drain and create the output fragment
            self.drain(aggregator, &mut state, &settings, timeout, all_eos)?
        };

        for (sinkpad, event) in upstream_events {
            sinkpad.push_event(event);
        }

        if let Some(buffers) = buffers {
            gst::trace!(CAT, obj: aggregator, "Pushing buffer list {:?}", buffers);
            aggregator.finish_buffer_list(buffers)?;
        }

        if all_eos {
            gst::debug!(CAT, obj: aggregator, "Doing EOS handling");

            if settings.header_update_mode != super::HeaderUpdateMode::None {
                let updated_header = self.update_header(
                    aggregator,
                    &mut self.state.lock().unwrap(),
                    &settings,
                    true,
                );
                match updated_header {
                    Ok(Some((buffer_list, caps))) => {
                        match settings.header_update_mode {
                            super::HeaderUpdateMode::None => unreachable!(),
                            super::HeaderUpdateMode::Rewrite => {
                                let src_pad = aggregator.src_pad();
                                let mut q = gst::query::Seeking::new(gst::Format::Bytes);
                                if src_pad.peer_query(&mut q) && q.result().0 {
                                    aggregator.set_src_caps(&caps);

                                    // Seek to the beginning with a default bytes segment
                                    aggregator
                                        .update_segment(
                                            &gst::FormattedSegment::<gst::format::Bytes>::new(),
                                        );

                                    if let Err(err) = aggregator.finish_buffer_list(buffer_list) {
                                        gst::error!(
                                            CAT,
                                            obj: aggregator,
                                            "Failed pushing updated header buffer downstream: {:?}",
                                            err,
                                        );
                                    }
                                } else {
                                    gst::error!(
                                        CAT,
                                        obj: aggregator,
                                        "Can't rewrite header because downstream is not seekable"
                                    );
                                }
                            }
                            super::HeaderUpdateMode::Update => {
                                aggregator.set_src_caps(&caps);
                                if let Err(err) = aggregator.finish_buffer_list(buffer_list) {
                                    gst::error!(
                                        CAT,
                                        obj: aggregator,
                                        "Failed pushing updated header buffer downstream: {:?}",
                                        err,
                                    );
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        gst::error!(
                            CAT,
                            obj: aggregator,
                            "Failed to generate updated header: {:?}",
                            err
                        );
                    }
                }
            }

            // Need to output new headers if started again after EOS
            self.state.lock().unwrap().sent_headers = false;

            Err(gst::FlowError::Eos)
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }
}

#[repr(C)]
pub(crate) struct Class {
    parent: gst_base::ffi::GstAggregatorClass,
    variant: super::Variant,
}

unsafe impl ClassStruct for Class {
    type Type = FMP4Mux;
}

impl std::ops::Deref for Class {
    type Target = glib::Class<gst_base::Aggregator>;

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

pub(crate) trait FMP4MuxImpl: AggregatorImpl {
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
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
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

impl AggregatorImpl for ISOFMP4Mux {}

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

impl AggregatorImpl for CMAFMux {}

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

impl AggregatorImpl for DASHMP4Mux {}

impl FMP4MuxImpl for DASHMP4Mux {
    const VARIANT: super::Variant = super::Variant::DASH;
}

#[derive(Default)]
pub(crate) struct ONVIFFMP4Mux;

#[glib::object_subclass]
impl ObjectSubclass for ONVIFFMP4Mux {
    const NAME: &'static str = "GstONVIFFMP4Mux";
    type Type = super::ONVIFFMP4Mux;
    type ParentType = super::FMP4Mux;
}

impl ObjectImpl for ONVIFFMP4Mux {}

impl GstObjectImpl for ONVIFFMP4Mux {}

impl ElementImpl for ONVIFFMP4Mux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "ONVIFFMP4Mux",
                "Codec/Muxer",
                "ONVIF fragmented MP4 muxer",
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
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
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
                    gst::Structure::builder("image/jpeg")
                        .field("width", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "raw")
                        .field("channels", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .field("rate", gst::IntRange::<i32>::new(1, i32::MAX))
                        .build(),
                    gst::Structure::builder("audio/x-alaw")
                        .field("channels", gst::IntRange::<i32>::new(1, 2))
                        .field("rate", gst::IntRange::<i32>::new(1, i32::MAX))
                        .build(),
                    gst::Structure::builder("audio/x-mulaw")
                        .field("channels", gst::IntRange::<i32>::new(1, 2))
                        .field("rate", gst::IntRange::<i32>::new(1, i32::MAX))
                        .build(),
                    gst::Structure::builder("audio/x-adpcm")
                        .field("layout", "g726")
                        .field("channels", 1i32)
                        .field("rate", 8000i32)
                        .field("bitrate", gst::List::new([16000i32, 24000, 32000, 40000]))
                        .build(),
                    gst::Structure::builder("application/x-onvif-metadata")
                        .field("encoding", "utf8")
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

impl AggregatorImpl for ONVIFFMP4Mux {}

impl FMP4MuxImpl for ONVIFFMP4Mux {
    const VARIANT: super::Variant = super::Variant::ONVIF;
}
