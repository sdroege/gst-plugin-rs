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
//
// SPDX-License-Identifier: LGPL-2.1-or-later

use futures::future::{abortable, AbortHandle, Aborted};
use futures::prelude::*;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_rtp::RTPBuffer;

use std::sync::LazyLock;

use std::cmp::Ordering;
use std::collections::{BTreeSet, VecDeque};
use std::mem;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use crate::runtime::prelude::*;
use crate::runtime::{self, Context, PadSink, PadSrc, Task};

use super::jitterbuffer::{RTPJitterBuffer, RTPJitterBufferItem, RTPPacketRateCtx};

const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::from_mseconds(200);
const DEFAULT_DO_LOST: bool = false;
const DEFAULT_MAX_DROPOUT_TIME: u32 = 60000;
const DEFAULT_MAX_MISORDER_TIME: u32 = 2000;
const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: gst::ClockTime = gst::ClockTime::ZERO;

#[derive(Debug, Clone)]
struct Settings {
    latency: gst::ClockTime,
    do_lost: bool,
    max_dropout_time: u32,
    max_misorder_time: u32,
    context: String,
    context_wait: gst::ClockTime,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            latency: DEFAULT_LATENCY,
            do_lost: DEFAULT_DO_LOST,
            max_dropout_time: DEFAULT_MAX_DROPOUT_TIME,
            max_misorder_time: DEFAULT_MAX_MISORDER_TIME,
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
        }
    }
}

#[derive(Eq)]
struct GapPacket {
    buffer: gst::Buffer,
    seq: u16,
    pt: u8,
}

impl GapPacket {
    fn new(buffer: gst::Buffer) -> Self {
        let rtp_buffer = RTPBuffer::from_buffer_readable(&buffer).unwrap();
        let seq = rtp_buffer.seq();
        let pt = rtp_buffer.payload_type();
        drop(rtp_buffer);

        Self { buffer, seq, pt }
    }
}

impl Ord for GapPacket {
    fn cmp(&self, other: &Self) -> Ordering {
        0.cmp(&gst_rtp::compare_seqnum(self.seq, other.seq))
    }
}

impl PartialOrd for GapPacket {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for GapPacket {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

#[derive(Default)]
struct SinkHandlerInner {
    packet_rate_ctx: RTPPacketRateCtx,
    ips_rtptime: Option<u32>,
    ips_pts: Option<gst::ClockTime>,

    gap_packets: BTreeSet<GapPacket>,

    last_pt: Option<u8>,

    last_in_seqnum: Option<u16>,
    last_rtptime: Option<u32>,
}

#[derive(Clone, Default)]
struct SinkHandler(Arc<StdMutex<SinkHandlerInner>>);

impl SinkHandler {
    fn clear(&self) {
        let mut inner = self.0.lock().unwrap();
        *inner = SinkHandlerInner::default();
    }

    // For resetting if seqnum discontinuities
    fn reset(&self, inner: &mut SinkHandlerInner, jb: &JitterBuffer) -> BTreeSet<GapPacket> {
        gst::info!(CAT, imp = jb, "Resetting");

        let mut state = jb.state.lock().unwrap();
        state.jbuf.flush();
        state.jbuf.reset_skew();
        state.discont = true;

        state.last_popped_seqnum = None;
        state.last_popped_pts = None;
        state.last_popped_buffer_pts = None;

        inner.last_in_seqnum = None;
        inner.last_rtptime = None;

        state.earliest_pts = None;
        state.earliest_seqnum = None;

        inner.ips_rtptime = None;
        inner.ips_pts = None;

        mem::take(&mut inner.gap_packets)
    }

    fn parse_caps(
        &self,
        inner: &mut SinkHandlerInner,
        state: &mut State,
        jb: &JitterBuffer,
        caps: &gst::Caps,
        pt: u8,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let s = caps.structure(0).ok_or(gst::FlowError::Error)?;

        gst::debug!(CAT, imp = jb, "Parsing {:?}", caps);

        let payload = s.get::<i32>("payload").map_err(|err| {
            gst::debug!(CAT, imp = jb, "Caps 'payload': {}", err);
            gst::FlowError::Error
        })?;

        if pt != 0 && payload as u8 != pt {
            gst::debug!(
                CAT,
                imp = jb,
                "Caps 'payload' ({}) doesn't match payload type ({})",
                payload,
                pt
            );
            return Err(gst::FlowError::Error);
        }

        inner.last_pt = Some(pt);
        let clock_rate = s.get::<i32>("clock-rate").map_err(|err| {
            gst::debug!(CAT, imp = jb, "Caps 'clock-rate': {}", err);
            gst::FlowError::Error
        })?;

        if clock_rate <= 0 {
            gst::debug!(CAT, imp = jb, "Caps 'clock-rate' <= 0");
            return Err(gst::FlowError::Error);
        }
        state.clock_rate = Some(clock_rate as u32);

        inner.packet_rate_ctx.reset(clock_rate);
        state.jbuf.set_clock_rate(clock_rate as u32);

        Ok(gst::FlowSuccess::Ok)
    }

    fn calculate_packet_spacing(
        &self,
        inner: &mut SinkHandlerInner,
        state: &mut State,
        rtptime: u32,
        pts: impl Into<Option<gst::ClockTime>>,
    ) {
        if inner.ips_rtptime != Some(rtptime) {
            let pts = pts.into();
            let new_packet_spacing = pts.opt_checked_sub(inner.ips_pts).ok().flatten();
            if let Some(new_packet_spacing) = new_packet_spacing {
                let old_packet_spacing = state.packet_spacing;

                if old_packet_spacing > new_packet_spacing {
                    state.packet_spacing = (new_packet_spacing + 3 * old_packet_spacing) / 4;
                } else if !old_packet_spacing.is_zero() {
                    state.packet_spacing = (3 * new_packet_spacing + old_packet_spacing) / 4;
                } else {
                    state.packet_spacing = new_packet_spacing;
                }

                gst::debug!(
                    CAT,
                    "new packet spacing {}, old packet spacing {} combined to {}",
                    new_packet_spacing,
                    old_packet_spacing,
                    state.packet_spacing
                );
            }
            inner.ips_rtptime = Some(rtptime);
            inner.ips_pts = pts;
        }
    }

    fn handle_big_gap_buffer(
        &self,
        inner: &mut SinkHandlerInner,
        jb: &JitterBuffer,
        buffer: gst::Buffer,
        pt: u8,
    ) -> bool {
        let gap_packets_length = inner.gap_packets.len();
        let mut reset = false;

        gst::debug!(
            CAT,
            imp = jb,
            "Handling big gap, gap packets length: {}",
            gap_packets_length
        );

        inner.gap_packets.insert(GapPacket::new(buffer));

        if gap_packets_length > 0 {
            let mut prev_gap_seq = u32::MAX;
            let mut all_consecutive = true;

            for gap_packet in inner.gap_packets.iter() {
                gst::log!(
                    CAT,
                    imp = jb,
                    "Looking at gap packet with seq {}",
                    gap_packet.seq,
                );

                all_consecutive = gap_packet.pt == pt;

                if prev_gap_seq == u32::MAX {
                    prev_gap_seq = gap_packet.seq as u32;
                } else if gst_rtp::compare_seqnum(gap_packet.seq, prev_gap_seq as u16) != -1 {
                    all_consecutive = false;
                } else {
                    prev_gap_seq = gap_packet.seq as u32;
                }

                if !all_consecutive {
                    break;
                }
            }

            gst::debug!(CAT, imp = jb, "all consecutive: {}", all_consecutive);

            if all_consecutive && gap_packets_length > 3 {
                reset = true;
            } else if !all_consecutive {
                inner.gap_packets.clear();
            }
        }

        reset
    }

    fn store(
        &self,
        inner: &mut SinkHandlerInner,
        pad: &gst::Pad,
        jb: &JitterBuffer,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = jb.state.lock().unwrap();

        let (max_misorder_time, max_dropout_time) = {
            let settings = jb.settings.lock().unwrap();
            (settings.max_misorder_time, settings.max_dropout_time)
        };

        let (seq, rtptime, pt) = {
            let rtp_buffer =
                RTPBuffer::from_buffer_readable(&buffer).map_err(|_| gst::FlowError::Error)?;
            (
                rtp_buffer.seq(),
                rtp_buffer.timestamp(),
                rtp_buffer.payload_type(),
            )
        };

        let mut pts = buffer.pts();
        let mut dts = buffer.dts();
        let mut estimated_dts = false;

        gst::log!(
            CAT,
            imp = jb,
            "Storing buffer, seq: {}, rtptime: {}, pt: {}",
            seq,
            rtptime,
            pt
        );

        let element = jb.obj();

        if dts.is_none() {
            dts = pts;
        } else if pts.is_none() {
            pts = dts;
        }

        if dts.is_none() {
            dts = element.current_running_time();
            pts = dts;

            estimated_dts = state.clock_rate.is_some();
        } else {
            dts = state.segment.to_running_time(dts);
        }

        if state.clock_rate.is_none() {
            inner.ips_rtptime = Some(rtptime);
            inner.ips_pts = pts;
        }

        if inner.last_pt != Some(pt) {
            inner.last_pt = Some(pt);
            state.clock_rate = None;

            gst::debug!(CAT, obj = pad, "New payload type: {}", pt);

            if let Some(caps) = pad.current_caps() {
                /* Ignore errors at this point, as we want to emit request-pt-map */
                let _ = self.parse_caps(inner, &mut state, jb, &caps, pt);
            }
        }

        let mut state = {
            if state.clock_rate.is_none() {
                drop(state);
                let caps = element
                    .emit_by_name::<Option<gst::Caps>>("request-pt-map", &[&(pt as u32)])
                    .ok_or_else(|| {
                        gst::error!(CAT, obj = pad, "Signal 'request-pt-map' returned None");
                        gst::FlowError::Error
                    })?;
                let mut state = jb.state.lock().unwrap();
                self.parse_caps(inner, &mut state, jb, &caps, pt)?;
                state
            } else {
                state
            }
        };

        inner.packet_rate_ctx.update(seq, rtptime);

        let max_dropout = inner.packet_rate_ctx.max_dropout(max_dropout_time as i32);
        let max_misorder = inner.packet_rate_ctx.max_misorder(max_misorder_time as i32);

        pts = state
            .jbuf
            .calculate_pts(dts, estimated_dts, rtptime, element.base_time(), 0, false);

        if pts.is_none() {
            gst::debug!(
                CAT,
                imp = jb,
                "cannot calculate a valid pts for #{}, discard",
                seq
            );
            return Ok(gst::FlowSuccess::Ok);
        }

        if let Some(last_in_seqnum) = inner.last_in_seqnum {
            let gap = gst_rtp::compare_seqnum(last_in_seqnum, seq);
            if gap == 1 {
                self.calculate_packet_spacing(inner, &mut state, rtptime, pts);
            } else {
                if (gap != -1 && gap < -(max_misorder as i32)) || (gap >= max_dropout as i32) {
                    let reset = self.handle_big_gap_buffer(inner, jb, buffer, pt);
                    if reset {
                        // Handle reset in `enqueue_item` to avoid recursion
                        return Err(gst::FlowError::CustomError);
                    } else {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                }
                inner.ips_pts = None;
                inner.ips_rtptime = None;
            }

            inner.gap_packets.clear();
        }

        if let Some(last_popped_seqnum) = state.last_popped_seqnum {
            let gap = gst_rtp::compare_seqnum(last_popped_seqnum, seq);

            if gap <= 0 {
                state.stats.num_late += 1;
                gst::debug!(CAT, imp = jb, "Dropping late {}", seq);
                return Ok(gst::FlowSuccess::Ok);
            }
        }

        inner.last_in_seqnum = Some(seq);

        let jb_item = if estimated_dts {
            RTPJitterBufferItem::new(buffer, gst::ClockTime::NONE, pts, Some(seq), rtptime)
        } else {
            RTPJitterBufferItem::new(buffer, dts, pts, Some(seq), rtptime)
        };

        let (success, _, _) = state.jbuf.insert(jb_item);

        if !success {
            /* duplicate */
            return Ok(gst::FlowSuccess::Ok);
        }

        if Some(rtptime) == inner.last_rtptime {
            state.equidistant -= 2;
        } else {
            state.equidistant += 1;
        }

        state.equidistant = state.equidistant.clamp(-7, 7);

        inner.last_rtptime = Some(rtptime);

        let must_update = match (state.earliest_pts, pts) {
            (None, _) => true,
            (Some(earliest_pts), Some(pts)) if pts < earliest_pts => true,
            (Some(earliest_pts), Some(pts)) if pts == earliest_pts => state
                .earliest_seqnum
                .is_some_and(|earliest_seqnum| seq > earliest_seqnum),
            _ => false,
        };

        if must_update {
            state.earliest_pts = pts;
            state.earliest_seqnum = Some(seq);
        }

        gst::log!(CAT, obj = pad, "Stored buffer");

        Ok(gst::FlowSuccess::Ok)
    }

    fn enqueue_item(
        &self,
        pad: gst::Pad,
        jb: &JitterBuffer,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut inner = self.0.lock().unwrap();

        let mut buffers = VecDeque::new();
        if let Some(buf) = buffer {
            buffers.push_back(buf);
        }

        // This is to avoid recursion with `store`, `reset` and `enqueue_item`
        while let Some(buf) = buffers.pop_front() {
            if let Err(err) = self.store(&mut inner, &pad, jb, buf) {
                match err {
                    gst::FlowError::CustomError => {
                        for gap_packet in self.reset(&mut inner, jb) {
                            buffers.push_back(gap_packet.buffer);
                        }
                    }
                    other => return Err(other),
                }
            }
        }

        let mut state = jb.state.lock().unwrap();

        let (latency, context_wait, do_lost, max_dropout_time) = {
            let settings = jb.settings.lock().unwrap();
            (
                settings.latency,
                settings.context_wait,
                settings.do_lost,
                gst::ClockTime::from_mseconds(settings.max_dropout_time as u64),
            )
        };

        // Reschedule if needed
        let (_, next_wakeup) = jb.src_pad_handler.next_wakeup(
            &jb.obj(),
            &state,
            do_lost,
            latency,
            context_wait,
            max_dropout_time,
        );
        if let Some((next_wakeup, _)) = next_wakeup {
            if let Some((previous_next_wakeup, ref abort_handle)) = state.wait_handle {
                if previous_next_wakeup.is_none()
                    || next_wakeup.is_some_and(|next| previous_next_wakeup.unwrap() > next)
                {
                    gst::debug!(
                        CAT,
                        obj = pad,
                        "Rescheduling for new item {} < {}",
                        next_wakeup.display(),
                        previous_next_wakeup.display(),
                    );
                    abort_handle.abort();
                    state.wait_handle = None;
                }
            }
        }
        state.last_res
    }
}

impl PadSinkHandler for SinkHandler {
    type ElementImpl = JitterBuffer;

    async fn sink_chain(
        self,
        pad: gst::Pad,
        elem: super::JitterBuffer,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::debug!(CAT, obj = pad, "Handling {:?}", buffer);
        self.enqueue_item(pad, elem.imp(), Some(buffer))
    }

    fn sink_event(self, pad: &gst::Pad, jb: &JitterBuffer, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling {:?}", event);

        if let EventView::FlushStart(..) = event.view() {
            if jb
                .task
                .flush_start()
                .block_on_or_add_subtask_then(jb.obj(), |elem, res| {
                    if let Err(err) = res {
                        gst::error!(CAT, obj = elem, "FlushStart failed {err:?}");
                        gst::element_error!(
                            elem,
                            gst::StreamError::Failed,
                            ("Internal data stream error"),
                            ["FlushStart failed {err:?}"]
                        );
                    }
                })
                .is_err()
            {
                return false;
            }
        }

        gst::log!(CAT, obj = pad, "Forwarding {:?}", event);
        jb.src_pad.gst_pad().push_event(event)
    }

    async fn sink_event_serialized(
        self,
        pad: gst::Pad,
        elem: super::JitterBuffer,
        event: gst::Event,
    ) -> bool {
        gst::log!(CAT, obj = pad, "Handling {:?}", event);

        let jb = elem.imp();

        let mut forward = true;
        use gst::EventView;
        match event.view() {
            EventView::Segment(e) => {
                let mut state = jb.state.lock().unwrap();
                state.segment = e.segment().clone().downcast::<gst::format::Time>().unwrap();
            }
            EventView::FlushStop(..) => {
                if jb
                    .task
                    .flush_stop()
                    .block_on_or_add_subtask_then(jb.obj(), |elem, res| {
                        if let Err(err) = res {
                            gst::error!(CAT, obj = elem, "FlushStop failed {err:?}");
                            gst::element_error!(
                                elem,
                                gst::StreamError::Failed,
                                ("Internal data stream error"),
                                ["FlushStop failed {err:?}"]
                            );
                        }
                    })
                    .is_err()
                {
                    return false;
                }
            }
            EventView::Eos(..) => {
                let mut state = jb.state.lock().unwrap();
                state.eos = true;
                if let Some((_, abort_handle)) = state.wait_handle.take() {
                    abort_handle.abort();
                }
                forward = false;
            }
            _ => (),
        };

        if forward {
            // FIXME: These events should really be queued up and stay in order
            gst::log!(CAT, obj = pad, "Forwarding serialized {:?}", event);
            jb.src_pad.push_event(event).await
        } else {
            true
        }
    }
}

#[derive(Clone, Default)]
struct SrcHandler;

impl SrcHandler {
    fn clear(&self) {}

    fn generate_lost_events(
        &self,
        state: &mut State,
        element: &super::JitterBuffer,
        seqnum: u16,
        pts: impl Into<Option<gst::ClockTime>>,
        discont: &mut bool,
    ) -> Vec<gst::Event> {
        let pts = pts.into();
        let (latency, do_lost) = {
            let jb = element.imp();
            let settings = jb.settings.lock().unwrap();
            (settings.latency, settings.do_lost)
        };

        let mut events = vec![];

        let last_popped_seqnum = match state.last_popped_seqnum {
            None => return events,
            Some(seq) => seq,
        };

        gst::debug!(
            CAT,
            obj = element,
            "Generating lost events seq: {}, last popped seq: {:?}",
            seqnum,
            last_popped_seqnum,
        );

        let mut lost_seqnum = last_popped_seqnum.wrapping_add(1);
        let gap = gst_rtp::compare_seqnum(lost_seqnum, seqnum) as i64;

        if gap > 0 {
            let gap = gap as u64;
            // FIXME reason why we can expect Some for the 2 lines below
            let mut last_popped_pts = state.last_popped_pts.unwrap();

            let spacing = if state.equidistant > 3 {
                state.packet_spacing
            } else {
                let interval = pts.unwrap().saturating_sub(last_popped_pts);
                interval / (gap + 1)
            };

            *discont = true;

            if state.equidistant > 0 && gap > 1 && gap * spacing > latency {
                let n_packets = gap - latency.nseconds() / spacing.nseconds();

                if do_lost {
                    let s = gst::Structure::builder("GstRTPPacketLost")
                        .field("seqnum", lost_seqnum as u32)
                        .field("timestamp", last_popped_pts + spacing)
                        .field("duration", (n_packets * spacing).nseconds())
                        .field("retry", 0)
                        .build();

                    events.push(gst::event::CustomDownstream::new(s));
                }

                lost_seqnum = lost_seqnum.wrapping_add(n_packets as u16);
                last_popped_pts += n_packets * spacing;
                state.last_popped_pts = Some(last_popped_pts);
                state.stats.num_lost += n_packets;
            }

            while lost_seqnum != seqnum {
                let timestamp = last_popped_pts + spacing;
                let duration = if state.equidistant > 0 {
                    spacing
                } else {
                    gst::ClockTime::ZERO
                };

                if timestamp.opt_gt(pts).unwrap_or(false) {
                    break;
                }

                state.last_popped_pts = Some(timestamp);

                if do_lost {
                    let s = gst::Structure::builder("GstRTPPacketLost")
                        .field("seqnum", lost_seqnum as u32)
                        .field("timestamp", timestamp)
                        .field("duration", duration.nseconds())
                        .field("retry", 0)
                        .build();

                    events.push(gst::event::CustomDownstream::new(s));
                }

                state.stats.num_lost += 1;

                lost_seqnum = lost_seqnum.wrapping_add(1);
            }
        }

        events
    }

    async fn pop_and_push(
        &self,
        element: &super::JitterBuffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let jb = element.imp();

        let (lost_events, buffer, seq) = {
            let mut state = jb.state.lock().unwrap();

            let mut discont = false;
            let (jb_item, _) = state.jbuf.pop();

            let jb_item = match jb_item {
                None => {
                    if state.eos {
                        return Err(gst::FlowError::Eos);
                    } else {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                }
                Some(item) => item,
            };

            let dts = jb_item.dts();
            let pts = jb_item.pts();
            let seq = jb_item.seqnum();
            let mut buffer = jb_item.into_buffer();

            let lost_events = {
                let buffer = buffer.make_mut();

                buffer.set_dts(state.segment.to_running_time(dts));
                buffer.set_pts(state.segment.to_running_time(pts));

                if state.last_popped_pts.is_some() && buffer.pts() < state.last_popped_pts {
                    buffer.set_pts(state.last_popped_pts)
                }

                let lost_events = if let Some(seq) = seq {
                    self.generate_lost_events(&mut state, element, seq, pts, &mut discont)
                } else {
                    vec![]
                };

                if state.discont {
                    discont = true;
                    state.discont = false;
                }

                if discont {
                    buffer.set_flags(gst::BufferFlags::DISCONT);
                }

                lost_events
            };

            state.last_popped_pts = buffer.pts();
            state.last_popped_buffer_pts = buffer.pts();
            if state.last_popped_pts.is_some() {
                state.position = state.last_popped_pts;
            }
            state.last_popped_seqnum = seq;

            state.stats.num_pushed += 1;

            (lost_events, buffer, seq)
        };

        for event in lost_events {
            gst::debug!(
                CAT,
                obj = jb.src_pad.gst_pad(),
                "Pushing lost event {:?}",
                event
            );
            let _ = jb.src_pad.push_event(event).await;
        }

        gst::debug!(
            CAT,
            obj = jb.src_pad.gst_pad(),
            "Pushing {:?} with seq {:?}",
            buffer,
            seq
        );

        jb.src_pad.push(buffer).await
    }

    // If there is a gap between the next seqnum we must output and the seqnum of
    // the earliest item currently stored, we may want to wake up earlier in order
    // to push the corresponding lost event, provided we are reasonably sure that
    // packets are equidistant and we have calculated a packet spacing.
    fn next_lost_wakeup(
        &self,
        state: &State,
        do_lost: bool,
        latency: gst::ClockTime,
        context_wait: gst::ClockTime,
        max_dropout_time: gst::ClockTime,
    ) -> Option<gst::ClockTime> {
        // No reason to wake up if we've already pushed longer than
        // max_dropout_time consecutive PacketLost events
        let dropout_time = state
            .last_popped_pts
            .opt_saturating_sub(state.last_popped_buffer_pts);
        if dropout_time.opt_gt(max_dropout_time).unwrap_or(false) {
            return None;
        }

        if do_lost && state.equidistant > 3 && !state.packet_spacing.is_zero() {
            if let Some(last_popped_pts) = state.last_popped_pts {
                if let Some(earliest) = state.earliest_seqnum {
                    if let Some(gap) = state
                        .last_popped_seqnum
                        .map(|last| gst_rtp::compare_seqnum(last, earliest))
                    {
                        if gap > 1 {
                            return Some(last_popped_pts + latency - context_wait / 2);
                        }
                    }
                } else {
                    return Some(last_popped_pts + latency - context_wait / 2);
                }
            }
        }

        None
    }

    // The time we should wake up at in order to push our earliest item on time
    fn next_packet_wakeup(
        &self,
        state: &State,
        latency: gst::ClockTime,
        context_wait: gst::ClockTime,
    ) -> Option<gst::ClockTime> {
        state.earliest_pts.map(|earliest_pts| {
            (earliest_pts + latency)
                .saturating_sub(state.packet_spacing)
                .saturating_sub(context_wait / 2)
        })
    }

    fn next_wakeup(
        &self,
        element: &super::JitterBuffer,
        state: &State,
        do_lost: bool,
        latency: gst::ClockTime,
        context_wait: gst::ClockTime,
        max_dropout_time: gst::ClockTime,
    ) -> (
        Option<gst::ClockTime>,
        Option<(Option<gst::ClockTime>, Duration)>,
    ) {
        let now = element.current_running_time();

        gst::debug!(
            CAT,
            obj = element,
            "Now is {}, EOS {}, earliest pts is {}, packet_spacing {} and latency {}",
            now.display(),
            state.eos,
            state.earliest_pts.display(),
            state.packet_spacing,
            latency
        );

        if state.eos {
            gst::debug!(CAT, obj = element, "EOS, not waiting");
            return (now, Some((now, Duration::ZERO)));
        }

        if let Some(next_wakeup) = self
            .next_lost_wakeup(state, do_lost, latency, context_wait, max_dropout_time)
            .or_else(|| self.next_packet_wakeup(state, latency, context_wait))
        {
            let delay = next_wakeup
                .opt_saturating_sub(now)
                .unwrap_or(gst::ClockTime::ZERO);

            gst::debug!(
                CAT,
                obj = element,
                "Next wakeup at {} with delay {}",
                next_wakeup.display(),
                delay
            );

            (now, Some((Some(next_wakeup), delay.into())))
        } else {
            (now, None)
        }
    }
}

impl PadSrcHandler for SrcHandler {
    type ElementImpl = JitterBuffer;

    fn src_event(self, pad: &gst::Pad, jb: &JitterBuffer, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling {:?}", event);

        match event.view() {
            EventView::FlushStart(..) => {
                if jb
                    .task
                    .flush_start()
                    .block_on_or_add_subtask_then(jb.obj(), |elem, res| {
                        if let Err(err) = res {
                            gst::error!(CAT, obj = elem, "FlushStart failed {err:?}");
                            gst::element_error!(
                                elem,
                                gst::StreamError::Failed,
                                ("Internal data stream error"),
                                ["FlushStart failed {err:?}"]
                            );
                        }
                    })
                    .is_err()
                {
                    return false;
                }
            }
            EventView::FlushStop(..) => {
                if jb
                    .task
                    .flush_stop()
                    .block_on_or_add_subtask_then(jb.obj(), |elem, res| {
                        if let Err(err) = res {
                            gst::error!(CAT, obj = elem, "FlushStop failed {err:?}");
                            gst::element_error!(
                                elem,
                                gst::StreamError::Failed,
                                ("Internal data stream error"),
                                ["FlushStop failed {err:?}"]
                            );
                        }
                    })
                    .is_err()
                {
                    return false;
                }
            }
            _ => (),
        }

        gst::log!(CAT, obj = pad, "Forwarding {:?}", event);
        jb.sink_pad.gst_pad().push_event(event)
    }

    fn src_query(self, pad: &gst::Pad, jb: &JitterBuffer, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        gst::log!(CAT, obj = pad, "Forwarding {:?}", query);

        match query.view_mut() {
            QueryViewMut::Latency(q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = jb.sink_pad.gst_pad().peer_query(&mut peer_query);

                if ret {
                    let settings = jb.settings.lock().unwrap();
                    let (_, mut min_latency, _) = peer_query.result();
                    min_latency += settings.latency;
                    let max_latency = gst::ClockTime::NONE;

                    q.set(true, min_latency, max_latency);
                }

                ret
            }
            QueryViewMut::Position(q) => {
                if q.format() != gst::Format::Time {
                    jb.sink_pad.gst_pad().peer_query(query)
                } else {
                    let state = jb.state.lock().unwrap();
                    let position = state.position;
                    q.set(position);
                    true
                }
            }
            _ => jb.sink_pad.gst_pad().peer_query(query),
        }
    }
}

#[derive(Debug, Default)]
struct Stats {
    num_pushed: u64,
    num_lost: u64,
    num_late: u64,
}

// Shared state between element, sink and source pad
struct State {
    jbuf: RTPJitterBuffer,

    last_res: Result<gst::FlowSuccess, gst::FlowError>,
    position: Option<gst::ClockTime>,

    segment: gst::FormattedSegment<gst::ClockTime>,
    clock_rate: Option<u32>,

    packet_spacing: gst::ClockTime,
    equidistant: i32,

    discont: bool,
    eos: bool,

    last_popped_seqnum: Option<u16>,
    last_popped_pts: Option<gst::ClockTime>,
    /* Not affected by PacketLost events */
    last_popped_buffer_pts: Option<gst::ClockTime>,

    stats: Stats,

    earliest_pts: Option<gst::ClockTime>,
    earliest_seqnum: Option<u16>,

    wait_handle: Option<(Option<gst::ClockTime>, AbortHandle)>,
}

impl Default for State {
    fn default() -> State {
        State {
            jbuf: RTPJitterBuffer::new(),

            last_res: Ok(gst::FlowSuccess::Ok),
            position: None,

            segment: gst::FormattedSegment::<gst::ClockTime>::new(),
            clock_rate: None,

            packet_spacing: gst::ClockTime::ZERO,
            equidistant: 0,

            discont: true,
            eos: false,

            last_popped_seqnum: None,
            last_popped_pts: None,
            last_popped_buffer_pts: None,

            stats: Stats::default(),

            earliest_pts: None,
            earliest_seqnum: None,

            wait_handle: None,
        }
    }
}

struct JitterBufferTask {
    element: super::JitterBuffer,
    src_pad_handler: SrcHandler,
    sink_pad_handler: SinkHandler,
}

impl JitterBufferTask {
    fn new(
        element: &super::JitterBuffer,
        src_pad_handler: &SrcHandler,
        sink_pad_handler: &SinkHandler,
    ) -> Self {
        JitterBufferTask {
            element: element.clone(),
            src_pad_handler: src_pad_handler.clone(),
            sink_pad_handler: sink_pad_handler.clone(),
        }
    }
}

impl TaskImpl for JitterBufferTask {
    type Item = ();

    fn obj(&self) -> &impl IsA<glib::Object> {
        &self.element
    }

    async fn start(&mut self) -> Result<(), gst::ErrorMessage> {
        gst::log!(CAT, obj = self.element, "Starting task");

        self.src_pad_handler.clear();
        self.sink_pad_handler.clear();

        let jb = self.element.imp();

        let latency = jb.settings.lock().unwrap().latency;
        let state = State::default();

        state.jbuf.set_delay(latency);
        *jb.state.lock().unwrap() = state;

        gst::log!(CAT, obj = self.element, "Task started");
        Ok(())
    }

    // FIXME this function was migrated to the try_next / handle_item model
    // but hasn't been touched as there are pending changes to jitterbuffer
    // in https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/merge_requests/756.
    // It should be possible to remove the loop below as try_next /  handle_item
    // are executed in a loop by the Task state machine.
    // It should also be possible to store latency and context_wait as
    // fields of JitterBufferTask so as to avoid locking the settings.
    // If latency can change during processing, a command based mechanism
    // could be implemented. See the command implementation for ts-udpsink as
    // an example.
    async fn try_next(&mut self) -> Result<(), gst::FlowError> {
        let jb = self.element.imp();
        let (latency, context_wait, do_lost, max_dropout_time) = {
            let settings = jb.settings.lock().unwrap();
            (
                settings.latency,
                settings.context_wait,
                settings.do_lost,
                gst::ClockTime::from_mseconds(settings.max_dropout_time as u64),
            )
        };

        loop {
            let delay_fut = {
                let mut state = jb.state.lock().unwrap();
                let (_, next_wakeup) = self.src_pad_handler.next_wakeup(
                    &self.element,
                    &state,
                    do_lost,
                    latency,
                    context_wait,
                    max_dropout_time,
                );

                let (delay_fut, abort_handle) = match next_wakeup {
                    Some((_, delay)) if delay.is_zero() => (None, None),
                    _ => {
                        let (delay_fut, abort_handle) = abortable(async move {
                            match next_wakeup {
                                Some((_, delay)) => {
                                    runtime::timer::delay_for_at_least(delay).await;
                                }
                                None => {
                                    future::pending::<()>().await;
                                }
                            };
                        });

                        let next_wakeup = next_wakeup.and_then(|w| w.0);
                        (Some(delay_fut), Some((next_wakeup, abort_handle)))
                    }
                };

                state.wait_handle = abort_handle;

                delay_fut
            };

            // Got aborted, reschedule if needed
            if let Some(delay_fut) = delay_fut {
                gst::debug!(CAT, obj = self.element, "Waiting");
                if let Err(Aborted) = delay_fut.await {
                    gst::debug!(CAT, obj = self.element, "Waiting aborted");
                    return Ok(());
                }
            }

            let (head_pts, head_seq, lost_events) = {
                let mut state = jb.state.lock().unwrap();
                //
                // Check earliest PTS as we have just taken the lock
                let (now, next_wakeup) = self.src_pad_handler.next_wakeup(
                    &self.element,
                    &state,
                    do_lost,
                    latency,
                    context_wait,
                    max_dropout_time,
                );

                gst::debug!(
                    CAT,
                    obj = self.element,
                    "Woke up at {}, earliest_pts {}",
                    now.display(),
                    state.earliest_pts.display()
                );

                if let Some((next_wakeup, _)) = next_wakeup {
                    if next_wakeup.opt_gt(now).unwrap_or(false) {
                        // Reschedule and wait a bit longer in the next iteration
                        return Ok(());
                    }
                } else {
                    return Ok(());
                }

                let (head_pts, head_seq) = state.jbuf.peek();
                let mut events = vec![];

                // We may have woken up in order to push lost events on time
                // (see next_packet_wakeup())
                if do_lost && state.equidistant > 3 && !state.packet_spacing.is_zero() {
                    loop {
                        // Make sure we don't push longer than max_dropout_time
                        // consecutive PacketLost events
                        let dropout_time = state
                            .last_popped_pts
                            .opt_saturating_sub(state.last_popped_buffer_pts);
                        if dropout_time.opt_gt(max_dropout_time).unwrap_or(false) {
                            break;
                        }

                        if let Some((lost_seq, lost_pts)) =
                            state.last_popped_seqnum.and_then(|last| {
                                if let Some(last_popped_pts) = state.last_popped_pts {
                                    let next = last.wrapping_add(1);
                                    if (last_popped_pts + latency - context_wait / 2)
                                        .opt_lt(now)
                                        .unwrap_or(false)
                                    {
                                        if let Some(earliest) = state.earliest_seqnum {
                                            if next != earliest {
                                                Some((next, last_popped_pts + state.packet_spacing))
                                            } else {
                                                None
                                            }
                                        } else {
                                            Some((next, last_popped_pts + state.packet_spacing))
                                        }
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                        {
                            if (lost_pts + latency).opt_lt(now).unwrap_or(false) {
                                /* We woke up to push the next lost event exactly on time, yet
                                 * clearly we are now too late to do so. This may have happened
                                 * because of a seqnum jump on the input side or some other
                                 * condition, but in any case we want to let the regular
                                 * generate_lost_events method take over, with its lost events
                                 * aggregation logic.
                                 */
                                break;
                            }

                            if lost_pts.opt_gt(state.earliest_pts).unwrap_or(false) {
                                /* Don't let our logic carry us too far in the future */
                                break;
                            }

                            let s = gst::Structure::builder("GstRTPPacketLost")
                                .field("seqnum", lost_seq as u32)
                                .field("timestamp", lost_pts)
                                .field("duration", state.packet_spacing)
                                .field("retry", 0)
                                .build();

                            events.push(gst::event::CustomDownstream::new(s));
                            state.stats.num_lost += 1;
                            state.last_popped_pts = Some(lost_pts);
                            state.last_popped_seqnum = Some(lost_seq);
                        } else {
                            break;
                        }
                    }
                }

                (head_pts, head_seq, events)
            };

            {
                // Push any lost events we may have woken up to push on schedule
                for event in lost_events {
                    gst::debug!(
                        CAT,
                        obj = jb.src_pad.gst_pad(),
                        "Pushing lost event {:?}",
                        event
                    );
                    let _ = jb.src_pad.push_event(event).await;
                }

                let state = jb.state.lock().unwrap();
                //
                // Now recheck earliest PTS as we have just retaken the lock and may
                // have advanced last_popped_* fields
                let (now, next_wakeup) = self.src_pad_handler.next_wakeup(
                    &self.element,
                    &state,
                    do_lost,
                    latency,
                    context_wait,
                    max_dropout_time,
                );

                gst::debug!(
                    CAT,
                    obj = &self.element,
                    "Woke up at {}, earliest_pts {}",
                    now.display(),
                    state.earliest_pts.display()
                );

                if let Some((next_wakeup, _)) = next_wakeup {
                    if next_wakeup.opt_gt(now).unwrap_or(false) {
                        // Reschedule and wait a bit longer in the next iteration
                        return Ok(());
                    }
                } else {
                    return Ok(());
                }
            }

            let res = self.src_pad_handler.pop_and_push(&self.element).await;

            {
                let mut state = jb.state.lock().unwrap();

                state.last_res = res;

                if head_pts == state.earliest_pts && head_seq == state.earliest_seqnum {
                    if state.jbuf.num_packets() > 0 {
                        let (earliest_pts, earliest_seqnum) = state.jbuf.find_earliest();
                        state.earliest_pts = earliest_pts;
                        state.earliest_seqnum = earliest_seqnum;
                    } else {
                        state.earliest_pts = None;
                        state.earliest_seqnum = None;
                    }
                }

                if res.is_ok() {
                    // Return and reschedule if the next packet would be in the future
                    // Check earliest PTS as we have just taken the lock
                    let (now, next_wakeup) = self.src_pad_handler.next_wakeup(
                        &self.element,
                        &state,
                        do_lost,
                        latency,
                        context_wait,
                        max_dropout_time,
                    );
                    if let Some((Some(next_wakeup), _)) = next_wakeup {
                        if now.is_some_and(|now| next_wakeup > now) {
                            // Reschedule and wait a bit longer in the next iteration
                            return Ok(());
                        }
                    } else {
                        return Ok(());
                    }
                }
            }

            if let Err(err) = res {
                match err {
                    gst::FlowError::Eos => {
                        gst::debug!(CAT, obj = self.element, "Pushing EOS event");
                        let _ = jb.src_pad.push_event(gst::event::Eos::new()).await;
                    }
                    gst::FlowError::Flushing => {
                        gst::debug!(CAT, obj = self.element, "Flushing")
                    }
                    err => gst::error!(CAT, obj = self.element, "Error {}", err),
                }

                return Err(err);
            }
        }
    }

    async fn handle_item(&mut self, _item: ()) -> Result<(), gst::FlowError> {
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), gst::ErrorMessage> {
        gst::log!(CAT, obj = self.element, "Stopping task");

        let jb = self.element.imp();
        let mut jb_state = jb.state.lock().unwrap();

        if let Some((_, abort_handle)) = jb_state.wait_handle.take() {
            abort_handle.abort();
        }

        self.src_pad_handler.clear();
        self.sink_pad_handler.clear();

        *jb_state = State::default();

        gst::log!(CAT, obj = self.element, "Task stopped");
        Ok(())
    }
}

pub struct JitterBuffer {
    sink_pad: PadSink,
    src_pad: PadSrc,
    sink_pad_handler: SinkHandler,
    src_pad_handler: SrcHandler,
    task: Task,
    state: StdMutex<State>,
    settings: StdMutex<Settings>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ts-jitterbuffer",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing jitterbuffer"),
    )
});

impl JitterBuffer {
    fn clear_pt_map(&self) {
        gst::debug!(CAT, imp = self, "Clearing PT map");

        let mut state = self.state.lock().unwrap();
        state.clock_rate = None;
        state.jbuf.reset_skew();
    }

    fn prepare(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Preparing");

        let context = {
            let settings = self.settings.lock().unwrap();
            Context::acquire(&settings.context, settings.context_wait.into()).unwrap()
        };

        self.task
            .prepare(
                JitterBufferTask::new(&self.obj(), &self.src_pad_handler, &self.sink_pad_handler),
                context,
            )
            .block_on_or_add_subtask_then(self.obj(), |elem, res| {
                if res.is_ok() {
                    gst::debug!(CAT, obj = elem, "Prepared");
                }
            })
    }

    fn unprepare(&self) {
        gst::debug!(CAT, imp = self, "Unpreparing");
        let _ = self
            .task
            .unprepare()
            .block_on_or_add_subtask_then(self.obj(), |elem, _| {
                gst::debug!(CAT, obj = elem, "Unprepared");
            });
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Starting");
        self.task
            .start()
            .block_on_or_add_subtask_then(self.obj(), |elem, res| {
                if res.is_ok() {
                    gst::debug!(CAT, obj = elem, "Started");
                }
            })
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Stopping");
        self.task
            .stop()
            .block_on_or_add_subtask_then(self.obj(), |elem, res| {
                if res.is_ok() {
                    gst::debug!(CAT, obj = elem, "Stopped");
                }
            })
    }
}

#[glib::object_subclass]
impl ObjectSubclass for JitterBuffer {
    const NAME: &'static str = "GstTsJitterBuffer";
    type Type = super::JitterBuffer;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let sink_pad_handler = SinkHandler::default();
        let src_pad_handler = SrcHandler;

        Self {
            sink_pad: PadSink::new(
                gst::Pad::from_template(&klass.pad_template("sink").unwrap()),
                sink_pad_handler.clone(),
            ),
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap()),
                src_pad_handler.clone(),
            ),
            sink_pad_handler,
            src_pad_handler,
            task: Task::default(),
            state: StdMutex::new(State::default()),
            settings: StdMutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for JitterBuffer {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("context")
                    .nick("Context")
                    .blurb("Context name to share threads with")
                    .default_value(Some(DEFAULT_CONTEXT))
                    .build(),
                glib::ParamSpecUInt::builder("context-wait")
                    .nick("Context Wait")
                    .blurb("Throttle poll loop to run at most once every this many ms")
                    .maximum(1000)
                    .default_value(DEFAULT_CONTEXT_WAIT.mseconds() as u32)
                    .build(),
                glib::ParamSpecUInt::builder("latency")
                    .nick("Buffer latency in ms")
                    .blurb("Amount of ms to buffer")
                    .default_value(DEFAULT_LATENCY.mseconds() as u32)
                    .build(),
                glib::ParamSpecBoolean::builder("do-lost")
                    .nick("Do Lost")
                    .blurb("Send an event downstream when a packet is lost")
                    .default_value(DEFAULT_DO_LOST)
                    .build(),
                glib::ParamSpecUInt::builder("max-dropout-time")
                    .nick("Max dropout time")
                    .blurb("The maximum time (milliseconds) of missing packets tolerated.")
                    .default_value(DEFAULT_MAX_DROPOUT_TIME)
                    .build(),
                glib::ParamSpecUInt::builder("max-misorder-time")
                    .nick("Max misorder time")
                    .blurb("The maximum time (milliseconds) of misordered packets tolerated.")
                    .default_value(DEFAULT_MAX_MISORDER_TIME)
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
                    .nick("Statistics")
                    .blurb("Various statistics")
                    .read_only()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
            vec![
                glib::subclass::Signal::builder("clear-pt-map")
                    .action()
                    .class_handler(|args| {
                        let element = args[0].get::<super::JitterBuffer>().expect("signal arg");
                        let jb = element.imp();
                        jb.clear_pt_map();
                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("request-pt-map")
                    .param_types([u32::static_type()])
                    .return_type::<gst::Caps>()
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "latency" => {
                let latency = {
                    let mut settings = self.settings.lock().unwrap();
                    settings.latency = gst::ClockTime::from_mseconds(
                        value.get::<u32>().expect("type checked upstream").into(),
                    );
                    settings.latency
                };

                let state = self.state.lock().unwrap();
                state.jbuf.set_delay(latency);

                let _ = self
                    .obj()
                    .post_message(gst::message::Latency::builder().src(&*self.obj()).build());
            }
            "do-lost" => {
                let mut settings = self.settings.lock().unwrap();
                settings.do_lost = value.get().expect("type checked upstream");
            }
            "max-dropout-time" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_dropout_time = value.get().expect("type checked upstream");
            }
            "max-misorder-time" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_misorder_time = value.get().expect("type checked upstream");
            }
            "context" => {
                let mut settings = self.settings.lock().unwrap();
                settings.context = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| DEFAULT_CONTEXT.into());
            }
            "context-wait" => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_wait = gst::ClockTime::from_mseconds(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "latency" => {
                let settings = self.settings.lock().unwrap();
                (settings.latency.mseconds() as u32).to_value()
            }
            "do-lost" => {
                let settings = self.settings.lock().unwrap();
                settings.do_lost.to_value()
            }
            "max-dropout-time" => {
                let settings = self.settings.lock().unwrap();
                settings.max_dropout_time.to_value()
            }
            "max-misorder-time" => {
                let settings = self.settings.lock().unwrap();
                settings.max_misorder_time.to_value()
            }
            "stats" => {
                let state = self.state.lock().unwrap();
                let s = gst::Structure::builder("application/x-rtp-jitterbuffer-stats")
                    .field("num-pushed", state.stats.num_pushed)
                    .field("num-lost", state.stats.num_lost)
                    .field("num-late", state.stats.num_late)
                    .build();
                s.to_value()
            }
            "context" => {
                let settings = self.settings.lock().unwrap();
                settings.context.to_value()
            }
            "context-wait" => {
                let settings = self.settings.lock().unwrap();
                (settings.context_wait.mseconds() as u32).to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(self.sink_pad.gst_pad()).unwrap();
        obj.add_pad(self.src_pad.gst_pad()).unwrap();
        obj.set_element_flags(gst::ElementFlags::PROVIDE_CLOCK | gst::ElementFlags::REQUIRE_CLOCK);
    }
}

impl GstObjectImpl for JitterBuffer {}

impl ElementImpl for JitterBuffer {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing jitterbuffer",
                "Generic",
                "Simple jitterbuffer",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::new_any();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare().map_err(|err| {
                    self.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PausedToReady => {
                self.stop().map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare();
            }
            _ => (),
        }

        let mut success = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                self.start().map_err(|_| gst::StateChangeError)?;
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            _ => (),
        }

        Ok(success)
    }

    fn provide_clock(&self) -> Option<gst::Clock> {
        Some(gst::SystemClock::obtain())
    }
}
