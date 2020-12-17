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

use futures::future::BoxFuture;
use futures::future::{abortable, AbortHandle, Aborted};
use futures::prelude::*;

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;

use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_element_error, gst_error, gst_error_msg, gst_info, gst_log, gst_trace};
use gst_rtp::RTPBuffer;

use once_cell::sync::Lazy;

use std::cmp::{max, min, Ordering};
use std::collections::{BTreeSet, VecDeque};
use std::mem;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use crate::runtime::prelude::*;
use crate::runtime::{self, Context, PadSink, PadSinkRef, PadSrc, PadSrcRef, Task};

use super::jitterbuffer::{RTPJitterBuffer, RTPJitterBufferItem, RTPPacketRateCtx};

const DEFAULT_LATENCY_MS: u32 = 200;
const DEFAULT_DO_LOST: bool = false;
const DEFAULT_MAX_DROPOUT_TIME: u32 = 60000;
const DEFAULT_MAX_MISORDER_TIME: u32 = 2000;
const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;

#[derive(Debug, Clone)]
struct Settings {
    latency_ms: u32,
    do_lost: bool,
    max_dropout_time: u32,
    max_misorder_time: u32,
    context: String,
    context_wait: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            latency_ms: DEFAULT_LATENCY_MS,
            do_lost: DEFAULT_DO_LOST,
            max_dropout_time: DEFAULT_MAX_DROPOUT_TIME,
            max_misorder_time: DEFAULT_MAX_MISORDER_TIME,
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
        }
    }
}

static PROPERTIES: [subclass::Property; 7] = [
    subclass::Property("latency", |name| {
        glib::ParamSpec::uint(
            name,
            "Buffer latency in ms",
            "Amount of ms to buffer",
            0,
            std::u32::MAX,
            DEFAULT_LATENCY_MS,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("do-lost", |name| {
        glib::ParamSpec::boolean(
            name,
            "Do Lost",
            "Send an event downstream when a packet is lost",
            DEFAULT_DO_LOST,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("max-dropout-time", |name| {
        glib::ParamSpec::uint(
            name,
            "Max dropout time",
            "The maximum time (milliseconds) of missing packets tolerated.",
            0,
            std::u32::MAX,
            DEFAULT_MAX_DROPOUT_TIME,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("max-misorder-time", |name| {
        glib::ParamSpec::uint(
            name,
            "Max misorder time",
            "The maximum time (milliseconds) of misordered packets tolerated.",
            0,
            std::u32::MAX,
            DEFAULT_MAX_MISORDER_TIME,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("stats", |name| {
        glib::ParamSpec::boxed(
            name,
            "Statistics",
            "Various statistics",
            gst::Structure::static_type(),
            glib::ParamFlags::READABLE,
        )
    }),
    subclass::Property("context", |name| {
        glib::ParamSpec::string(
            name,
            "Context",
            "Context name to share threads with",
            Some(DEFAULT_CONTEXT),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("context-wait", |name| {
        glib::ParamSpec::uint(
            name,
            "Context Wait",
            "Throttle poll loop to run at most once every this many ms",
            0,
            1000,
            DEFAULT_CONTEXT_WAIT,
            glib::ParamFlags::READWRITE,
        )
    }),
];

#[derive(Eq)]
struct GapPacket {
    buffer: gst::Buffer,
    seq: u16,
    pt: u8,
}

impl GapPacket {
    fn new(buffer: gst::Buffer) -> Self {
        let rtp_buffer = RTPBuffer::from_buffer_readable(&buffer).unwrap();
        let seq = rtp_buffer.get_seq();
        let pt = rtp_buffer.get_payload_type();
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

struct SinkHandlerInner {
    packet_rate_ctx: RTPPacketRateCtx,
    ips_rtptime: Option<u32>,
    ips_pts: gst::ClockTime,

    gap_packets: BTreeSet<GapPacket>,

    last_pt: Option<u8>,

    last_in_seqnum: Option<u16>,
    last_rtptime: Option<u32>,
}

impl Default for SinkHandlerInner {
    fn default() -> Self {
        SinkHandlerInner {
            packet_rate_ctx: RTPPacketRateCtx::new(),
            ips_rtptime: None,
            ips_pts: gst::CLOCK_TIME_NONE,
            gap_packets: BTreeSet::new(),
            last_pt: None,
            last_in_seqnum: None,
            last_rtptime: None,
        }
    }
}

#[derive(Clone, Default)]
struct SinkHandler(Arc<StdMutex<SinkHandlerInner>>);

impl SinkHandler {
    fn clear(&self) {
        let mut inner = self.0.lock().unwrap();
        *inner = SinkHandlerInner::default();
    }

    // For resetting if seqnum discontinuities
    fn reset(
        &self,
        inner: &mut SinkHandlerInner,
        state: &mut State,
        element: &super::JitterBuffer,
    ) -> BTreeSet<GapPacket> {
        gst_info!(CAT, obj: element, "Resetting");

        state.jbuf.borrow().flush();
        state.jbuf.borrow().reset_skew();
        state.discont = true;

        state.last_popped_seqnum = None;
        state.last_popped_pts = gst::CLOCK_TIME_NONE;

        inner.last_in_seqnum = None;
        inner.last_rtptime = None;

        state.earliest_pts = gst::CLOCK_TIME_NONE;
        state.earliest_seqnum = None;

        inner.ips_rtptime = None;
        inner.ips_pts = gst::CLOCK_TIME_NONE;

        mem::replace(&mut inner.gap_packets, BTreeSet::new())
    }

    fn parse_caps(
        &self,
        inner: &mut SinkHandlerInner,
        state: &mut State,
        element: &super::JitterBuffer,
        caps: &gst::Caps,
        pt: u8,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let s = caps.get_structure(0).ok_or(gst::FlowError::Error)?;

        gst_info!(CAT, obj: element, "Parsing {:?}", caps);

        let payload = s
            .get_some::<i32>("payload")
            .map_err(|_| gst::FlowError::Error)?;

        if pt != 0 && payload as u8 != pt {
            return Err(gst::FlowError::Error);
        }

        inner.last_pt = Some(pt);
        let clock_rate = s
            .get_some::<i32>("clock-rate")
            .map_err(|_| gst::FlowError::Error)?;

        if clock_rate <= 0 {
            return Err(gst::FlowError::Error);
        }
        state.clock_rate = Some(clock_rate as u32);

        inner.packet_rate_ctx.reset(clock_rate);
        state.jbuf.borrow().set_clock_rate(clock_rate as u32);

        Ok(gst::FlowSuccess::Ok)
    }

    fn calculate_packet_spacing(
        &self,
        inner: &mut SinkHandlerInner,
        state: &mut State,
        rtptime: u32,
        pts: gst::ClockTime,
    ) {
        if inner.ips_rtptime != Some(rtptime) {
            if inner.ips_pts.is_some() && pts.is_some() {
                let new_packet_spacing = pts - inner.ips_pts;
                let old_packet_spacing = state.packet_spacing;

                assert!(old_packet_spacing.is_some());
                if old_packet_spacing > new_packet_spacing {
                    state.packet_spacing = (new_packet_spacing + 3 * old_packet_spacing) / 4;
                } else if !old_packet_spacing.is_zero() {
                    state.packet_spacing = (3 * new_packet_spacing + old_packet_spacing) / 4;
                } else {
                    state.packet_spacing = new_packet_spacing;
                }

                gst_debug!(
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
        element: &super::JitterBuffer,
        buffer: gst::Buffer,
        pt: u8,
    ) -> bool {
        let gap_packets_length = inner.gap_packets.len();
        let mut reset = false;

        gst_debug!(
            CAT,
            obj: element,
            "Handling big gap, gap packets length: {}",
            gap_packets_length
        );

        inner.gap_packets.insert(GapPacket::new(buffer));

        if gap_packets_length > 0 {
            let mut prev_gap_seq = std::u32::MAX;
            let mut all_consecutive = true;

            for gap_packet in inner.gap_packets.iter() {
                gst_log!(
                    CAT,
                    obj: element,
                    "Looking at gap packet with seq {}",
                    gap_packet.seq,
                );

                all_consecutive = gap_packet.pt == pt;

                if prev_gap_seq == std::u32::MAX {
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

            gst_debug!(CAT, obj: element, "all consecutive: {}", all_consecutive);

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
        element: &super::JitterBuffer,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let jb = JitterBuffer::from_instance(element);
        let mut state = jb.state.lock().unwrap();

        let (max_misorder_time, max_dropout_time) = {
            let settings = jb.settings.lock().unwrap();
            (settings.max_misorder_time, settings.max_dropout_time)
        };

        let (seq, rtptime, pt) = {
            let rtp_buffer =
                RTPBuffer::from_buffer_readable(&buffer).map_err(|_| gst::FlowError::Error)?;
            (
                rtp_buffer.get_seq(),
                rtp_buffer.get_timestamp(),
                rtp_buffer.get_payload_type(),
            )
        };

        let mut pts = buffer.get_pts();
        let mut dts = buffer.get_dts();
        let mut estimated_dts = false;

        gst_log!(
            CAT,
            obj: element,
            "Storing buffer, seq: {}, rtptime: {}, pt: {}",
            seq,
            rtptime,
            pt
        );

        if dts.is_none() {
            dts = pts;
        } else if pts.is_none() {
            pts = dts;
        }

        if dts.is_none() {
            dts = element.get_current_running_time();
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

            gst_debug!(CAT, obj: pad, "New payload type: {}", pt);

            if let Some(caps) = pad.get_current_caps() {
                /* Ignore errors at this point, as we want to emit request-pt-map */
                let _ = self.parse_caps(inner, &mut state, element, &caps, pt);
            }
        }

        let mut state = {
            if state.clock_rate.is_none() {
                drop(state);
                let caps = element
                    .emit("request-pt-map", &[&(pt as u32)])
                    .map_err(|_| gst::FlowError::Error)?
                    .ok_or(gst::FlowError::Error)?
                    .get::<gst::Caps>()
                    .map_err(|_| gst::FlowError::Error)?
                    .ok_or(gst::FlowError::Error)?;
                let mut state = jb.state.lock().unwrap();
                self.parse_caps(inner, &mut state, element, &caps, pt)?;
                state
            } else {
                state
            }
        };

        inner.packet_rate_ctx.update(seq, rtptime);

        let max_dropout = inner
            .packet_rate_ctx
            .get_max_dropout(max_dropout_time as i32);
        let max_misorder = inner
            .packet_rate_ctx
            .get_max_dropout(max_misorder_time as i32);

        pts = state.jbuf.borrow().calculate_pts(
            dts,
            estimated_dts,
            rtptime,
            element.get_base_time(),
            0,
            false,
        );

        if pts.is_none() {
            gst_debug!(
                CAT,
                obj: element,
                "cannot calculate a valid pts for #{}, discard",
                seq
            );
            return Ok(gst::FlowSuccess::Ok);
        }

        if let Some(last_in_seqnum) = inner.last_in_seqnum {
            let gap = gst_rtp::compare_seqnum(last_in_seqnum as u16, seq);
            if gap == 1 {
                self.calculate_packet_spacing(inner, &mut state, rtptime, pts);
            } else {
                if (gap != -1 && gap < -(max_misorder as i32)) || (gap >= max_dropout as i32) {
                    let reset = self.handle_big_gap_buffer(inner, element, buffer, pt);
                    if reset {
                        // Handle reset in `enqueue_item` to avoid recursion
                        return Err(gst::FlowError::CustomError);
                    } else {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                }
                inner.ips_pts = gst::CLOCK_TIME_NONE;
                inner.ips_rtptime = None;
            }

            inner.gap_packets.clear();
        }

        if let Some(last_popped_seqnum) = state.last_popped_seqnum {
            let gap = gst_rtp::compare_seqnum(last_popped_seqnum, seq);

            if gap <= 0 {
                state.stats.num_late += 1;
                gst_debug!(CAT, obj: element, "Dropping late {}", seq);
                return Ok(gst::FlowSuccess::Ok);
            }
        }

        inner.last_in_seqnum = Some(seq);

        let jb_item = if estimated_dts {
            RTPJitterBufferItem::new(buffer, gst::CLOCK_TIME_NONE, pts, Some(seq), rtptime)
        } else {
            RTPJitterBufferItem::new(buffer, dts, pts, Some(seq), rtptime)
        };

        let (success, _, _) = state.jbuf.borrow().insert(jb_item);

        if !success {
            /* duplicate */
            return Ok(gst::FlowSuccess::Ok);
        }

        if Some(rtptime) == inner.last_rtptime {
            state.equidistant -= 2;
        } else {
            state.equidistant += 1;
        }

        state.equidistant = min(max(state.equidistant, -7), 7);

        inner.last_rtptime = Some(rtptime);

        if state.earliest_pts.is_none()
            || (pts.is_some()
                && (pts < state.earliest_pts
                    || (pts == state.earliest_pts
                        && state
                            .earliest_seqnum
                            .map(|earliest_seqnum| seq > earliest_seqnum)
                            .unwrap_or(false))))
        {
            state.earliest_pts = pts;
            state.earliest_seqnum = Some(seq);
        }

        gst_log!(CAT, obj: pad, "Stored buffer");

        Ok(gst::FlowSuccess::Ok)
    }

    fn enqueue_item(
        &self,
        pad: &gst::Pad,
        element: &super::JitterBuffer,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut inner = self.0.lock().unwrap();

        let mut buffers = VecDeque::new();
        if let Some(buf) = buffer {
            buffers.push_back(buf);
        }

        // This is to avoid recursion with `store`, `reset` and `enqueue_item`
        while let Some(buf) = buffers.pop_front() {
            if let Err(err) = self.store(&mut inner, pad, element, buf) {
                match err {
                    gst::FlowError::CustomError => {
                        let jb = JitterBuffer::from_instance(element);
                        let mut state = jb.state.lock().unwrap();
                        for gap_packet in self.reset(&mut inner, &mut state, element) {
                            buffers.push_back(gap_packet.buffer);
                        }
                    }
                    other => return Err(other),
                }
            }
        }

        let jb = JitterBuffer::from_instance(element);
        let mut state = jb.state.lock().unwrap();

        let (latency, context_wait) = {
            let settings = jb.settings.lock().unwrap();
            (
                settings.latency_ms as u64 * gst::MSECOND,
                settings.context_wait as u64 * gst::MSECOND,
            )
        };

        // Reschedule if needed
        let (_, next_wakeup) =
            jb.src_pad_handler
                .get_next_wakeup(&element, &state, latency, context_wait);
        if let Some((next_wakeup, _)) = next_wakeup {
            if let Some((previous_next_wakeup, ref abort_handle)) = state.wait_handle {
                if previous_next_wakeup.is_none() || previous_next_wakeup > next_wakeup {
                    gst_debug!(
                        CAT,
                        obj: pad,
                        "Rescheduling for new item {} < {}",
                        next_wakeup,
                        previous_next_wakeup
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

    fn sink_chain(
        &self,
        pad: &PadSinkRef,
        _jb: &JitterBuffer,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let pad_weak = pad.downgrade();
        let element = element.clone().downcast::<super::JitterBuffer>().unwrap();
        let this = self.clone();

        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");

            gst_debug!(CAT, obj: pad.gst_pad(), "Handling {:?}", buffer);
            this.enqueue_item(pad.gst_pad(), &element, Some(buffer))
        }
        .boxed()
    }

    fn sink_event(
        &self,
        pad: &PadSinkRef,
        jb: &JitterBuffer,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        if let EventView::FlushStart(..) = event.view() {
            if let Err(err) = jb.task.flush_start() {
                gst_error!(CAT, obj: pad.gst_pad(), "FlushStart failed {:?}", err);
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["FlushStart failed {:?}", err]
                );
                return false;
            }
        }

        gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", event);
        jb.src_pad.gst_pad().push_event(event)
    }

    fn sink_event_serialized(
        &self,
        pad: &PadSinkRef,
        _jb: &JitterBuffer,
        element: &gst::Element,
        event: gst::Event,
    ) -> BoxFuture<'static, bool> {
        use gst::EventView;

        let pad_weak = pad.downgrade();
        let element = element.clone().downcast::<super::JitterBuffer>().unwrap();

        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");

            gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

            let jb = JitterBuffer::from_instance(&element);

            let mut forward = true;
            match event.view() {
                EventView::Segment(e) => {
                    let mut state = jb.state.lock().unwrap();
                    state.segment = e
                        .get_segment()
                        .clone()
                        .downcast::<gst::format::Time>()
                        .unwrap();
                }
                EventView::FlushStop(..) => {
                    if let Err(err) = jb.task.flush_stop() {
                        gst_error!(CAT, obj: pad.gst_pad(), "FlushStop failed {:?}", err);
                        gst_element_error!(
                            element,
                            gst::StreamError::Failed,
                            ("Internal data stream error"),
                            ["FlushStop failed {:?}", err]
                        );
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
                gst_log!(CAT, obj: pad.gst_pad(), "Forwarding serialized {:?}", event);
                jb.src_pad.push_event(event).await
            } else {
                true
            }
        }
        .boxed()
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
        pts: gst::ClockTime,
        discont: &mut bool,
    ) -> Vec<gst::Event> {
        let (latency_ns, do_lost) = {
            let jb = JitterBuffer::from_instance(element);
            let settings = jb.settings.lock().unwrap();
            (
                settings.latency_ms as u64 * gst::MSECOND.nseconds().unwrap(),
                settings.do_lost,
            )
        };

        let mut events = vec![];

        let last_popped_seqnum = match state.last_popped_seqnum {
            None => return events,
            Some(seq) => seq,
        };

        gst_debug!(
            CAT,
            obj: element,
            "Generating lost events seq: {}, last popped seq: {:?}",
            seqnum,
            last_popped_seqnum,
        );

        let mut lost_seqnum = last_popped_seqnum.wrapping_add(1);
        let gap = gst_rtp::compare_seqnum(lost_seqnum, seqnum) as i64;

        if gap > 0 {
            let interval =
                pts.nseconds().unwrap() as i64 - state.last_popped_pts.nseconds().unwrap() as i64;
            let gap = gap as u64;
            let spacing = if interval >= 0 {
                interval as u64 / (gap + 1)
            } else {
                0
            };

            *discont = true;

            if state.equidistant > 0 && gap > 1 && gap * spacing > latency_ns {
                let n_packets = gap - latency_ns / spacing;

                if do_lost {
                    let s = gst::Structure::new(
                        "GstRTPPacketLost",
                        &[
                            ("seqnum", &(lost_seqnum as u32)),
                            (
                                "timestamp",
                                &(state.last_popped_pts + gst::ClockTime(Some(spacing))),
                            ),
                            ("duration", &(n_packets * spacing)),
                            ("retry", &0),
                        ],
                    );

                    events.push(gst::event::CustomDownstream::new(s));
                }

                lost_seqnum = lost_seqnum.wrapping_add(n_packets as u16);
                state.last_popped_pts += gst::ClockTime(Some(n_packets * spacing));
                state.stats.num_lost += n_packets;
            }

            while lost_seqnum != seqnum {
                let timestamp = state.last_popped_pts + gst::ClockTime(Some(spacing));
                let duration = if state.equidistant > 0 { spacing } else { 0 };

                state.last_popped_pts = timestamp;

                if do_lost {
                    let s = gst::Structure::new(
                        "GstRTPPacketLost",
                        &[
                            ("seqnum", &(lost_seqnum as u32)),
                            ("timestamp", &timestamp),
                            ("duration", &duration),
                            ("retry", &0),
                        ],
                    );

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
        let jb = JitterBuffer::from_instance(element);

        let (lost_events, buffer, seq) = {
            let mut state = jb.state.lock().unwrap();

            let mut discont = false;
            let (jb_item, _) = state.jbuf.borrow().pop();

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

            let dts = jb_item.get_dts();
            let pts = jb_item.get_pts();
            let seq = jb_item.get_seqnum();
            let mut buffer = jb_item.into_buffer();

            let lost_events = {
                let buffer = buffer.make_mut();

                buffer.set_dts(state.segment.to_running_time(dts));
                buffer.set_pts(state.segment.to_running_time(pts));

                if state.last_popped_pts.is_some() && buffer.get_pts() < state.last_popped_pts {
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

            state.last_popped_pts = buffer.get_pts();
            if let Some(pts) = state.last_popped_pts.nseconds() {
                state.position = pts.into();
            }
            state.last_popped_seqnum = seq;

            state.stats.num_pushed += 1;

            (lost_events, buffer, seq)
        };

        for event in lost_events {
            gst_debug!(CAT, obj: jb.src_pad.gst_pad(), "Pushing lost event {:?}", event);
            let _ = jb.src_pad.push_event(event).await;
        }

        gst_debug!(CAT, obj: jb.src_pad.gst_pad(), "Pushing {:?} with seq {:?}", buffer, seq);

        jb.src_pad.push(buffer).await
    }

    fn get_next_wakeup(
        &self,
        element: &super::JitterBuffer,
        state: &State,
        latency: gst::ClockTime,
        context_wait: gst::ClockTime,
    ) -> (gst::ClockTime, Option<(gst::ClockTime, Duration)>) {
        let now = element.get_current_running_time();

        gst_debug!(
            CAT,
            obj: element,
            "Now is {}, EOS {}, earliest pts is {}, packet_spacing {} and latency {}",
            now,
            state.eos,
            state.earliest_pts,
            state.packet_spacing,
            latency
        );

        if state.eos {
            gst_debug!(CAT, obj: element, "EOS, not waiting");
            return (now, Some((now, Duration::from_nanos(0))));
        }

        if state.earliest_pts.is_none() {
            return (now, None);
        }

        let next_wakeup = state.earliest_pts + latency - state.packet_spacing - context_wait / 2;

        let delay = next_wakeup
            .saturating_sub(now)
            .unwrap_or_else(gst::ClockTime::zero)
            .nseconds()
            .unwrap();

        gst_debug!(
            CAT,
            obj: element,
            "Next wakeup at {} with delay {}",
            next_wakeup,
            delay
        );

        (now, Some((next_wakeup, Duration::from_nanos(delay))))
    }
}

impl PadSrcHandler for SrcHandler {
    type ElementImpl = JitterBuffer;

    fn src_event(
        &self,
        pad: &PadSrcRef,
        jb: &JitterBuffer,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        match event.view() {
            EventView::FlushStart(..) => {
                if let Err(err) = jb.task.flush_start() {
                    gst_error!(CAT, obj: pad.gst_pad(), "FlushStart failed {:?}", err);
                    gst_element_error!(
                        element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["FlushStart failed {:?}", err]
                    );
                    return false;
                }
            }
            EventView::FlushStop(..) => {
                if let Err(err) = jb.task.flush_stop() {
                    gst_error!(CAT, obj: pad.gst_pad(), "FlushStop failed {:?}", err);
                    gst_element_error!(
                        element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["FlushStop failed {:?}", err]
                    );
                    return false;
                }
            }
            _ => (),
        }

        gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", event);
        jb.sink_pad.gst_pad().push_event(event)
    }

    fn src_query(
        &self,
        pad: &PadSrcRef,
        jb: &JitterBuffer,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", query);

        match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = jb.sink_pad.gst_pad().peer_query(&mut peer_query);

                if ret {
                    let settings = jb.settings.lock().unwrap();
                    let (_, mut min_latency, _) = peer_query.get_result();
                    min_latency += (settings.latency_ms as u64) * gst::SECOND;
                    let max_latency = gst::CLOCK_TIME_NONE;

                    q.set(true, min_latency, max_latency);
                }

                ret
            }
            QueryView::Position(ref mut q) => {
                if q.get_format() != gst::Format::Time {
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

#[derive(Debug)]
struct Stats {
    num_pushed: u64,
    num_lost: u64,
    num_late: u64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            num_pushed: 0,
            num_lost: 0,
            num_late: 0,
        }
    }
}

// Shared state between element, sink and source pad
struct State {
    jbuf: glib::SendUniqueCell<RTPJitterBuffer>,

    last_res: Result<gst::FlowSuccess, gst::FlowError>,
    position: gst::ClockTime,

    segment: gst::FormattedSegment<gst::ClockTime>,
    clock_rate: Option<u32>,

    packet_spacing: gst::ClockTime,
    equidistant: i32,

    discont: bool,
    eos: bool,

    last_popped_seqnum: Option<u16>,
    last_popped_pts: gst::ClockTime,

    stats: Stats,

    earliest_pts: gst::ClockTime,
    earliest_seqnum: Option<u16>,

    wait_handle: Option<(gst::ClockTime, AbortHandle)>,
}

impl Default for State {
    fn default() -> State {
        State {
            jbuf: glib::SendUniqueCell::new(RTPJitterBuffer::new()).unwrap(),

            last_res: Ok(gst::FlowSuccess::Ok),
            position: gst::CLOCK_TIME_NONE,

            segment: gst::FormattedSegment::<gst::ClockTime>::new(),
            clock_rate: None,

            packet_spacing: gst::ClockTime::zero(),
            equidistant: 0,

            discont: true,
            eos: false,

            last_popped_seqnum: None,
            last_popped_pts: gst::CLOCK_TIME_NONE,

            stats: Stats::default(),

            earliest_pts: gst::CLOCK_TIME_NONE,
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
    fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Starting task");

            self.src_pad_handler.clear();
            self.sink_pad_handler.clear();

            let jb = JitterBuffer::from_instance(&self.element);
            *jb.state.lock().unwrap() = State::default();

            gst_log!(CAT, obj: &self.element, "Task started");
            Ok(())
        }
        .boxed()
    }

    fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            let jb = JitterBuffer::from_instance(&self.element);
            let (latency, context_wait) = {
                let settings = jb.settings.lock().unwrap();
                (
                    settings.latency_ms as u64 * gst::MSECOND,
                    settings.context_wait as u64 * gst::MSECOND,
                )
            };

            loop {
                let delay_fut = {
                    let mut state = jb.state.lock().unwrap();
                    let (_, next_wakeup) = self.src_pad_handler.get_next_wakeup(
                        &self.element,
                        &state,
                        latency,
                        context_wait,
                    );

                    let (delay_fut, abort_handle) = match next_wakeup {
                        Some((_, delay)) if delay == Duration::from_nanos(0) => (None, None),
                        _ => {
                            let (delay_fut, abort_handle) = abortable(async move {
                                match next_wakeup {
                                    Some((_, delay)) => {
                                        runtime::time::delay_for(delay).await;
                                    }
                                    None => {
                                        future::pending::<()>().await;
                                    }
                                };
                            });

                            let next_wakeup =
                                next_wakeup.map(|w| w.0).unwrap_or(gst::CLOCK_TIME_NONE);
                            (Some(delay_fut), Some((next_wakeup, abort_handle)))
                        }
                    };

                    state.wait_handle = abort_handle;

                    delay_fut
                };

                // Got aborted, reschedule if needed
                if let Some(delay_fut) = delay_fut {
                    gst_debug!(CAT, obj: &self.element, "Waiting");
                    if let Err(Aborted) = delay_fut.await {
                        gst_debug!(CAT, obj: &self.element, "Waiting aborted");
                        return Ok(());
                    }
                }

                let (head_pts, head_seq) = {
                    let state = jb.state.lock().unwrap();
                    //
                    // Check earliest PTS as we have just taken the lock
                    let (now, next_wakeup) = self.src_pad_handler.get_next_wakeup(
                        &self.element,
                        &state,
                        latency,
                        context_wait,
                    );

                    gst_debug!(
                        CAT,
                        obj: &self.element,
                        "Woke up at {}, earliest_pts {}",
                        now,
                        state.earliest_pts
                    );

                    if let Some((next_wakeup, _)) = next_wakeup {
                        if next_wakeup > now {
                            // Reschedule and wait a bit longer in the next iteration
                            return Ok(());
                        }
                    } else {
                        return Ok(());
                    }

                    let (head_pts, head_seq) = state.jbuf.borrow().peek();

                    (head_pts, head_seq)
                };

                let res = self.src_pad_handler.pop_and_push(&self.element).await;

                {
                    let mut state = jb.state.lock().unwrap();

                    state.last_res = res;

                    if head_pts == state.earliest_pts && head_seq == state.earliest_seqnum {
                        let (earliest_pts, earliest_seqnum) = state.jbuf.borrow().find_earliest();
                        state.earliest_pts = earliest_pts;
                        state.earliest_seqnum = earliest_seqnum;
                    }

                    if res.is_ok() {
                        // Return and reschedule if the next packet would be in the future
                        // Check earliest PTS as we have just taken the lock
                        let (now, next_wakeup) = self.src_pad_handler.get_next_wakeup(
                            &self.element,
                            &state,
                            latency,
                            context_wait,
                        );
                        if let Some((next_wakeup, _)) = next_wakeup {
                            if next_wakeup > now {
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
                            gst_debug!(CAT, obj: &self.element, "Pushing EOS event");
                            let _ = jb.src_pad.push_event(gst::event::Eos::new()).await;
                        }
                        gst::FlowError::Flushing => gst_debug!(CAT, obj: &self.element, "Flushing"),
                        err => gst_error!(CAT, obj: &self.element, "Error {}", err),
                    }

                    return Err(err);
                }
            }
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Stopping task");

            let jb = JitterBuffer::from_instance(&self.element);
            let mut jb_state = jb.state.lock().unwrap();

            if let Some((_, abort_handle)) = jb_state.wait_handle.take() {
                abort_handle.abort();
            }

            self.src_pad_handler.clear();
            self.sink_pad_handler.clear();

            *jb_state = State::default();

            gst_log!(CAT, obj: &self.element, "Task stopped");
            Ok(())
        }
        .boxed()
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

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-jitterbuffer",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing jitterbuffer"),
    )
});

impl JitterBuffer {
    fn clear_pt_map(&self, element: &super::JitterBuffer) {
        gst_info!(CAT, obj: element, "Clearing PT map");

        let mut state = self.state.lock().unwrap();
        state.clock_rate = None;
        state.jbuf.borrow().reset_skew();
    }

    fn prepare(&self, element: &super::JitterBuffer) -> Result<(), gst::ErrorMessage> {
        gst_info!(CAT, obj: element, "Preparing");

        let context = {
            let settings = self.settings.lock().unwrap();
            Context::acquire(&settings.context, settings.context_wait).unwrap()
        };

        self.task
            .prepare(
                JitterBufferTask::new(element, &self.src_pad_handler, &self.sink_pad_handler),
                context,
            )
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing Task: {:?}", err]
                )
            })?;

        gst_info!(CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &super::JitterBuffer) {
        gst_debug!(CAT, obj: element, "Unpreparing");
        self.task.unprepare().unwrap();
        gst_debug!(CAT, obj: element, "Unprepared");
    }

    fn start(&self, element: &super::JitterBuffer) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Starting");
        self.task.start()?;
        gst_debug!(CAT, obj: element, "Started");
        Ok(())
    }

    fn stop(&self, element: &super::JitterBuffer) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Stopping");
        self.task.stop()?;
        gst_debug!(CAT, obj: element, "Stopped");
        Ok(())
    }
}

impl ObjectSubclass for JitterBuffer {
    const NAME: &'static str = "RsTsJitterBuffer";
    type Type = super::JitterBuffer;
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib::object_subclass!();

    fn class_init(klass: &mut Self::Class) {
        klass.set_metadata(
            "Thread-sharing jitterbuffer",
            "Generic",
            "Simple jitterbuffer",
            "Mathieu Duponchelle <mathieu@centricular.com>",
        );

        let caps = gst::Caps::new_any();

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);
        klass.add_signal(
            "request-pt-map",
            glib::SignalFlags::RUN_LAST,
            &[u32::static_type()],
            gst::Caps::static_type(),
        );

        klass.add_signal_with_class_handler(
            "clear-pt-map",
            glib::SignalFlags::RUN_LAST | glib::SignalFlags::ACTION,
            &[],
            glib::types::Type::Unit,
            |_, args| {
                let element = args[0]
                    .get::<super::JitterBuffer>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let jb = Self::from_instance(&element);
                jb.clear_pt_map(&element);
                None
            },
        );

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);
        klass.install_properties(&PROPERTIES);
    }

    fn with_class(klass: &Self::Class) -> Self {
        let sink_pad_handler = SinkHandler::default();
        let src_pad_handler = SrcHandler::default();

        Self {
            sink_pad: PadSink::new(
                gst::Pad::from_template(&klass.get_pad_template("sink").unwrap(), Some("sink")),
                sink_pad_handler.clone(),
            ),
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.get_pad_template("src").unwrap(), Some("src")),
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
    fn set_property(&self, obj: &Self::Type, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("latency", ..) => {
                let latency_ms = {
                    let mut settings = self.settings.lock().unwrap();
                    settings.latency_ms = value.get_some().expect("type checked upstream");
                    settings.latency_ms as u64
                };

                let state = self.state.lock().unwrap();
                state.jbuf.borrow().set_delay(latency_ms * gst::MSECOND);

                let _ = obj.post_message(gst::message::Latency::builder().src(obj).build());
            }
            subclass::Property("do-lost", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.do_lost = value.get_some().expect("type checked upstream");
            }
            subclass::Property("max-dropout-time", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_dropout_time = value.get_some().expect("type checked upstream");
            }
            subclass::Property("max-misorder-time", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_misorder_time = value.get_some().expect("type checked upstream");
            }
            subclass::Property("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_wait = value.get_some().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &Self::Type, id: usize) -> glib::Value {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("latency", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.latency_ms.to_value()
            }
            subclass::Property("do-lost", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.do_lost.to_value()
            }
            subclass::Property("max-dropout-time", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.max_dropout_time.to_value()
            }
            subclass::Property("max-misorder-time", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.max_misorder_time.to_value()
            }
            subclass::Property("stats", ..) => {
                let state = self.state.lock().unwrap();
                let s = gst::Structure::new(
                    "application/x-rtp-jitterbuffer-stats",
                    &[
                        ("num-pushed", &state.stats.num_pushed),
                        ("num-lost", &state.stats.num_lost),
                        ("num-late", &state.stats.num_late),
                    ],
                );
                s.to_value()
            }
            subclass::Property("context", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.context.to_value()
            }
            subclass::Property("context-wait", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.context_wait.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(self.sink_pad.gst_pad()).unwrap();
        obj.add_pad(self.src_pad.gst_pad()).unwrap();
        obj.set_element_flags(gst::ElementFlags::PROVIDE_CLOCK | gst::ElementFlags::REQUIRE_CLOCK);
    }
}

impl ElementImpl for JitterBuffer {
    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare(element).map_err(|err| {
                    element.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PausedToReady => {
                self.stop(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element);
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                self.start(element).map_err(|_| gst::StateChangeError)?;
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            _ => (),
        }

        Ok(success)
    }

    fn provide_clock(&self, _element: &Self::Type) -> Option<gst::Clock> {
        Some(gst::SystemClock::obtain())
    }
}
