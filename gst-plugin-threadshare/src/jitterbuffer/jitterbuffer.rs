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

use either::Either;

use futures::future::BoxFuture;
use futures::future::{abortable, AbortHandle, Aborted};
use futures::lock::{Mutex, MutexGuard};
use futures::prelude::*;

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use glib::{glib_object_impl, glib_object_subclass};

use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error_msg, gst_info, gst_log, gst_trace};
use gst_rtp::RTPBuffer;

use lazy_static::lazy_static;

use std::cmp::{max, min, Ordering};
use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;

use crate::runtime::prelude::*;
use crate::runtime::{
    self, Context, JoinHandle, PadContext, PadSink, PadSinkRef, PadSrc, PadSrcRef, PadSrcWeak,
};

use super::{RTPJitterBuffer, RTPJitterBufferItem, RTPPacketRateCtx};

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

#[derive(Clone, Debug)]
struct JitterBufferPadSinkHandler;

impl PadSinkHandler for JitterBufferPadSinkHandler {
    type ElementImpl = JitterBuffer;

    fn sink_chain(
        &self,
        pad: &PadSinkRef,
        _jitterbuffer: &JitterBuffer,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let pad_weak = pad.downgrade();
        let element = element.clone();
        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");

            gst_debug!(CAT, obj: pad.gst_pad(), "Handling {:?}", buffer);
            let jitterbuffer = JitterBuffer::from_instance(&element);
            jitterbuffer
                .enqueue_item(pad.gst_pad(), &element, Some(buffer))
                .await
        }
        .boxed()
    }

    fn sink_event(
        &self,
        pad: &PadSinkRef,
        jitterbuffer: &JitterBuffer,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        use gst::EventView;

        if event.is_serialized() {
            let pad_weak = pad.downgrade();
            let element = element.clone();
            Either::Right(
                async move {
                    let pad = pad_weak.upgrade().expect("PadSink no longer exists");

                    let mut forward = true;

                    gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

                    let jitterbuffer = JitterBuffer::from_instance(&element);
                    match event.view() {
                        EventView::FlushStop(..) => {
                            jitterbuffer.flush(&element).await;
                        }
                        EventView::Segment(e) => {
                            let mut state = jitterbuffer.state.lock().await;
                            state.segment = e
                                .get_segment()
                                .clone()
                                .downcast::<gst::format::Time>()
                                .unwrap();
                        }
                        EventView::Eos(..) => {
                            let mut state = jitterbuffer.state.lock().await;
                            jitterbuffer.drain(&mut state, &element).await;
                        }
                        EventView::CustomDownstreamSticky(e) => {
                            if PadContext::is_pad_context_sticky_event(&e) {
                                forward = false;
                            }
                        }
                        _ => (),
                    };

                    if forward {
                        gst_log!(CAT, obj: pad.gst_pad(), "Forwarding serialized {:?}", event);
                        jitterbuffer.src_pad.push_event(event).await
                    } else {
                        true
                    }
                }
                .boxed(),
            )
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Forwarding non-serialized {:?}", event);
            Either::Left(jitterbuffer.src_pad.gst_pad().push_event(event))
        }
    }

    fn sink_query(
        &self,
        pad: &PadSinkRef,
        jitterbuffer: &JitterBuffer,
        element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", query);

        match query.view_mut() {
            QueryView::Drain(..) => {
                gst_info!(CAT, obj: pad.gst_pad(), "Draining");
                runtime::executor::block_on(jitterbuffer.enqueue_item(pad.gst_pad(), element, None))
                    .is_ok()
            }
            _ => jitterbuffer.src_pad.gst_pad().peer_query(query),
        }
    }
}

#[derive(Clone, Debug)]
struct JitterBufferPadSrcHandler;

impl PadSrcHandler for JitterBufferPadSrcHandler {
    type ElementImpl = JitterBuffer;

    fn src_query(
        &self,
        pad: &PadSrcRef,
        jitterbuffer: &JitterBuffer,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", query);

        match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                let mut peer_query = gst::query::Query::new_latency();

                let ret = jitterbuffer.sink_pad.gst_pad().peer_query(&mut peer_query);

                if ret {
                    let (_, mut min_latency, _) = peer_query.get_result();
                    let our_latency = runtime::executor::block_on(jitterbuffer.settings.lock())
                        .latency_ms as u64
                        * gst::MSECOND;

                    min_latency += our_latency;
                    let max_latency = gst::CLOCK_TIME_NONE;

                    q.set(true, min_latency, max_latency);
                }

                ret
            }
            QueryView::Position(ref mut q) => {
                if q.get_format() != gst::Format::Time {
                    jitterbuffer.sink_pad.gst_pad().peer_query(query)
                } else {
                    q.set(
                        runtime::executor::block_on(jitterbuffer.state.lock())
                            .segment
                            .get_position(),
                    );
                    true
                }
            }
            _ => jitterbuffer.sink_pad.gst_pad().peer_query(query),
        }
    }
}

#[derive(Eq)]
struct GapPacket(gst::Buffer);

impl Ord for GapPacket {
    fn cmp(&self, other: &Self) -> Ordering {
        let mut rtp_buffer = RTPBuffer::from_buffer_readable(&self.0).unwrap();
        let mut other_rtp_buffer = RTPBuffer::from_buffer_readable(&other.0).unwrap();

        let seq = rtp_buffer.get_seq();
        let other_seq = other_rtp_buffer.get_seq();

        drop(rtp_buffer);
        drop(other_rtp_buffer);

        0.cmp(&gst_rtp::compare_seqnum(seq, other_seq))
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

struct State {
    jbuf: glib::SendUniqueCell<RTPJitterBuffer>,
    packet_rate_ctx: RTPPacketRateCtx,
    clock_rate: i32,
    segment: gst::FormattedSegment<gst::ClockTime>,
    ips_rtptime: u32,
    ips_pts: gst::ClockTime,
    last_pt: u32,
    last_in_seqnum: u32,
    packet_spacing: gst::ClockTime,
    gap_packets: Option<BTreeSet<GapPacket>>,
    last_popped_seqnum: u32,
    num_pushed: u64,
    num_lost: u64,
    num_late: u64,
    last_rtptime: u32,
    equidistant: i32,
    earliest_pts: gst::ClockTime,
    earliest_seqnum: u16,
    last_popped_pts: gst::ClockTime,
    discont: bool,
    last_res: Result<gst::FlowSuccess, gst::FlowError>,
    task_queue_abort_handle: Option<AbortHandle>,
    wakeup_abort_handle: Option<AbortHandle>,
    wakeup_join_handle: Option<JoinHandle<Result<(), Aborted>>>,
}

impl Default for State {
    fn default() -> State {
        State {
            jbuf: glib::SendUniqueCell::new(RTPJitterBuffer::new()).unwrap(),
            packet_rate_ctx: RTPPacketRateCtx::new(),
            clock_rate: -1,
            segment: gst::FormattedSegment::<gst::ClockTime>::new(),
            ips_rtptime: 0,
            ips_pts: gst::CLOCK_TIME_NONE,
            last_pt: std::u32::MAX,
            last_in_seqnum: std::u32::MAX,
            packet_spacing: gst::ClockTime(Some(0)),
            gap_packets: Some(BTreeSet::new()),
            last_popped_seqnum: std::u32::MAX,
            num_pushed: 0,
            num_lost: 0,
            num_late: 0,
            last_rtptime: std::u32::MAX,
            equidistant: 0,
            earliest_pts: gst::CLOCK_TIME_NONE,
            earliest_seqnum: 0,
            last_popped_pts: gst::CLOCK_TIME_NONE,
            discont: false,
            last_res: Ok(gst::FlowSuccess::Ok),
            task_queue_abort_handle: None,
            wakeup_abort_handle: None,
            wakeup_join_handle: None,
        }
    }
}

struct JitterBuffer {
    sink_pad: PadSink,
    src_pad: PadSrc,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-jitterbuffer",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing jitterbuffer"),
    );
}

impl JitterBuffer {
    fn get_current_running_time(&self, element: &gst::Element) -> gst::ClockTime {
        if let Some(clock) = element.get_clock() {
            if clock.get_time() > element.get_base_time() {
                clock.get_time() - element.get_base_time()
            } else {
                gst::ClockTime(Some(0))
            }
        } else {
            gst::CLOCK_TIME_NONE
        }
    }

    fn parse_caps(
        &self,
        state: &mut MutexGuard<State>,
        element: &gst::Element,
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

        state.last_pt = pt as u32;
        state.clock_rate = s
            .get_some::<i32>("clock-rate")
            .map_err(|_| gst::FlowError::Error)?;

        if state.clock_rate <= 0 {
            return Err(gst::FlowError::Error);
        }

        let clock_rate = state.clock_rate;

        state.packet_rate_ctx.reset(clock_rate);
        state.jbuf.borrow().set_clock_rate(clock_rate as u32);

        Ok(gst::FlowSuccess::Ok)
    }

    fn calculate_packet_spacing(
        &self,
        state: &mut MutexGuard<State>,
        rtptime: u32,
        pts: gst::ClockTime,
    ) {
        if state.ips_rtptime != rtptime {
            if state.ips_pts.is_some() && pts.is_some() {
                let new_packet_spacing = pts - state.ips_pts;
                let old_packet_spacing = state.packet_spacing;

                if old_packet_spacing > new_packet_spacing {
                    state.packet_spacing = (new_packet_spacing + 3 * old_packet_spacing) / 4;
                } else if old_packet_spacing > gst::ClockTime(Some(0)) {
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
            state.ips_rtptime = rtptime;
            state.ips_pts = pts;
        }
    }

    fn handle_big_gap_buffer(
        &self,
        state: &mut MutexGuard<State>,
        element: &gst::Element,
        buffer: gst::Buffer,
        pt: u8,
    ) -> bool {
        let gap_packets = state.gap_packets.as_mut().unwrap();

        let gap_packets_length = gap_packets.len();
        let mut reset = false;

        gst_debug!(
            CAT,
            obj: element,
            "Handling big gap, gap packets length: {}",
            gap_packets_length
        );

        gap_packets.insert(GapPacket(buffer));

        if gap_packets_length > 0 {
            let mut prev_gap_seq = std::u32::MAX;
            let mut all_consecutive = true;

            for gap_packet in gap_packets.iter() {
                let mut rtp_buffer = RTPBuffer::from_buffer_readable(&gap_packet.0).unwrap();

                let gap_pt = rtp_buffer.get_payload_type();
                let gap_seq = rtp_buffer.get_seq();

                gst_log!(
                    CAT,
                    obj: element,
                    "Looking at gap packet with seq {}",
                    gap_seq
                );

                drop(rtp_buffer);

                all_consecutive = gap_pt == pt;

                if prev_gap_seq == std::u32::MAX {
                    prev_gap_seq = gap_seq as u32;
                } else if gst_rtp::compare_seqnum(gap_seq, prev_gap_seq as u16) != -1 {
                    all_consecutive = false;
                } else {
                    prev_gap_seq = gap_seq as u32;
                }

                if !all_consecutive {
                    break;
                }
            }

            gst_debug!(CAT, obj: element, "all consecutive: {}", all_consecutive);

            if all_consecutive && gap_packets_length > 3 {
                reset = true;
            } else if !all_consecutive {
                gap_packets.clear();
            }
        }

        reset
    }

    fn reset(
        &self,
        state: &mut MutexGuard<'_, State>,
        element: &gst::Element,
    ) -> BTreeSet<GapPacket> {
        gst_info!(CAT, obj: element, "Resetting");

        state.jbuf.borrow().flush();
        state.jbuf.borrow().reset_skew();
        state.discont = true;
        state.last_popped_seqnum = std::u32::MAX;
        state.last_in_seqnum = std::u32::MAX;
        state.ips_rtptime = 0;
        state.ips_pts = gst::CLOCK_TIME_NONE;

        let gap_packets = state.gap_packets.take();
        state.gap_packets = Some(BTreeSet::new());

        // Handle gap_packets in caller to avoid recursion
        gap_packets.unwrap()
    }

    async fn store(
        &self,
        state: &mut MutexGuard<'_, State>,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (max_misorder_time, max_dropout_time) = {
            let settings = self.settings.lock().await;
            (settings.max_misorder_time, settings.max_dropout_time)
        };

        let (seq, rtptime, pt) = {
            let mut rtp_buffer =
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

        if dts == gst::CLOCK_TIME_NONE {
            dts = pts;
        } else if pts == gst::CLOCK_TIME_NONE {
            pts = dts;
        }

        if dts == gst::CLOCK_TIME_NONE {
            dts = self.get_current_running_time(element);
            pts = dts;

            estimated_dts = state.clock_rate != -1;
        } else {
            dts = state.segment.to_running_time(dts);
        }

        if state.clock_rate == -1 {
            state.ips_rtptime = rtptime;
            state.ips_pts = pts;
        }

        if state.last_pt != pt as u32 {
            state.last_pt = pt as u32;
            state.clock_rate = -1;

            gst_debug!(CAT, obj: pad, "New payload type: {}", pt);

            if let Some(caps) = pad.get_current_caps() {
                self.parse_caps(state, element, &caps, pt)?;
            }
        }

        if state.clock_rate == -1 {
            let caps = element
                .emit("request-pt-map", &[&(pt as u32)])
                .map_err(|_| gst::FlowError::Error)?
                .ok_or(gst::FlowError::Error)?
                .get::<gst::Caps>()
                .map_err(|_| gst::FlowError::Error)?
                .ok_or(gst::FlowError::Error)?;
            self.parse_caps(state, element, &caps, pt)?;
        }

        state.packet_rate_ctx.update(seq, rtptime);

        let max_dropout = state
            .packet_rate_ctx
            .get_max_dropout(max_dropout_time as i32);
        let max_misorder = state
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

        if state.last_in_seqnum != std::u32::MAX {
            let gap = gst_rtp::compare_seqnum(state.last_in_seqnum as u16, seq);
            if gap == 1 {
                self.calculate_packet_spacing(state, rtptime, pts);
            } else {
                if (gap != -1 && gap < -(max_misorder as i32)) || (gap >= max_dropout as i32) {
                    let reset = self.handle_big_gap_buffer(state, element, buffer, pt);
                    if reset {
                        // Handle reset in `enqueue_item` to avoid recursion
                        return Err(gst::FlowError::CustomError);
                    } else {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                }
                state.ips_pts = gst::CLOCK_TIME_NONE;
                state.ips_rtptime = 0;
            }

            state.gap_packets.as_mut().unwrap().clear();
        }

        if state.last_popped_seqnum != std::u32::MAX {
            let gap = gst_rtp::compare_seqnum(state.last_popped_seqnum as u16, seq);

            if gap <= 0 {
                state.num_late += 1;
                gst_debug!(CAT, obj: element, "Dropping late {}", seq);
                return Ok(gst::FlowSuccess::Ok);
            }
        }

        state.last_in_seqnum = seq as u32;

        let jb_item = if estimated_dts {
            RTPJitterBufferItem::new(buffer, gst::CLOCK_TIME_NONE, pts, seq as u32, rtptime)
        } else {
            RTPJitterBufferItem::new(buffer, dts, pts, seq as u32, rtptime)
        };

        let (success, _, _) = state.jbuf.borrow().insert(jb_item);

        if !success {
            /* duplicate */
            return Ok(gst::FlowSuccess::Ok);
        }

        if rtptime == state.last_rtptime {
            state.equidistant -= 2;
        } else {
            state.equidistant += 1;
        }

        state.equidistant = min(max(state.equidistant, -7), 7);

        state.last_rtptime = rtptime;

        if state.earliest_pts.is_none()
            || (pts.is_some()
                && (pts < state.earliest_pts
                    || (pts == state.earliest_pts && seq > state.earliest_seqnum)))
        {
            state.earliest_pts = pts;
            state.earliest_seqnum = seq;
        }

        gst_log!(CAT, obj: pad, "Stored buffer");

        Ok(gst::FlowSuccess::Ok)
    }

    async fn push_lost_events(
        &self,
        state: &mut MutexGuard<'_, State>,
        element: &gst::Element,
        seqnum: u32,
        pts: gst::ClockTime,
        discont: &mut bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (latency_ns, do_lost) = {
            let settings = self.settings.lock().await;
            (
                settings.latency_ms as i64 * gst::MSECOND.nseconds().unwrap() as i64,
                settings.do_lost,
            )
        };

        let mut ret = true;

        gst_debug!(
            CAT,
            obj: element,
            "Pushing lost events seq: {}, last popped seq: {}",
            seqnum,
            state.last_popped_seqnum
        );

        if state.last_popped_seqnum != std::u32::MAX {
            let mut lost_seqnum = ((state.last_popped_seqnum + 1) & 0xffff) as i64;
            let gap = gst_rtp::compare_seqnum(lost_seqnum as u16, seqnum as u16) as i64;

            if gap > 0 {
                let interval = pts.nseconds().unwrap() as i64
                    - state.last_popped_pts.nseconds().unwrap() as i64;
                let spacing = if interval >= 0 {
                    interval / (gap as i64 + 1)
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
                                    &(state.last_popped_pts + gst::ClockTime(Some(spacing as u64))),
                                ),
                                ("duration", &((n_packets * spacing) as u64)),
                                ("retry", &0),
                            ],
                        );

                        let event = gst::Event::new_custom_downstream(s).build();

                        ret = self.src_pad.push_event(event).await;
                    }

                    lost_seqnum = (lost_seqnum + n_packets) & 0xffff;
                    state.last_popped_pts += gst::ClockTime(Some((n_packets * spacing) as u64));
                    state.num_lost += n_packets as u64;

                    if !ret {
                        return Err(gst::FlowError::Error);
                    }
                }

                while lost_seqnum != seqnum as i64 {
                    let timestamp = state.last_popped_pts + gst::ClockTime(Some(spacing as u64));
                    let duration = if state.equidistant > 0 { spacing } else { 0 };

                    state.last_popped_pts = timestamp;

                    if do_lost {
                        let s = gst::Structure::new(
                            "GstRTPPacketLost",
                            &[
                                ("seqnum", &(lost_seqnum as u32)),
                                ("timestamp", &timestamp),
                                ("duration", &(duration as u64)),
                                ("retry", &0),
                            ],
                        );

                        let event = gst::Event::new_custom_downstream(s).build();

                        ret = self.src_pad.push_event(event).await;
                    }

                    state.num_lost += 1;

                    if !ret {
                        break;
                    }

                    lost_seqnum = (lost_seqnum + 1) & 0xffff;
                }
            }
        }

        if ret {
            Ok(gst::FlowSuccess::Ok)
        } else {
            Err(gst::FlowError::Error)
        }
    }

    async fn pop_and_push(
        &self,
        state: &mut MutexGuard<'_, State>,
        element: &gst::Element,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut discont = false;
        let (jb_item, _) = state.jbuf.borrow().pop();

        let dts = jb_item.get_dts();
        let pts = jb_item.get_pts();
        let seq = jb_item.get_seqnum();
        let mut buffer = jb_item.get_buffer();

        let buffer = buffer.make_mut();

        buffer.set_dts(state.segment.to_running_time(dts));
        buffer.set_pts(state.segment.to_running_time(pts));

        if state.last_popped_pts.is_some() && buffer.get_pts() < state.last_popped_pts {
            buffer.set_pts(state.last_popped_pts)
        }

        self.push_lost_events(state, element, seq, pts, &mut discont)
            .await?;

        if state.discont {
            discont = true;
            state.discont = false;
        }

        state.last_popped_pts = buffer.get_pts();
        state.last_popped_seqnum = seq;

        if discont {
            buffer.set_flags(gst::BufferFlags::DISCONT);
        }

        state.num_pushed += 1;

        gst_debug!(CAT, obj: self.src_pad.gst_pad(), "Pushing {:?} with seq {}", buffer, seq);

        self.src_pad.push(buffer.to_owned()).await
    }

    async fn schedule(&self, state: &mut MutexGuard<'_, State>, element: &gst::Element) {
        let (latency_ns, context_wait_ns) = {
            let settings = self.settings.lock().await;
            (
                settings.latency_ms as u64 * gst::MSECOND,
                settings.context_wait as u64 * gst::MSECOND,
            )
        };

        let now = self.get_current_running_time(element);

        gst_debug!(
            CAT,
            obj: element,
            "now is {}, earliest pts is {}, packet_spacing {} and latency {}",
            now,
            state.earliest_pts,
            state.packet_spacing,
            latency_ns
        );

        if state.earliest_pts.is_none() {
            return;
        }

        let next_wakeup = state.earliest_pts + latency_ns - state.packet_spacing;

        let delay = {
            if next_wakeup > now {
                (next_wakeup - now).nseconds().unwrap()
            } else {
                0
            }
        };

        if let Some(wakeup_abort_handle) = state.wakeup_abort_handle.take() {
            wakeup_abort_handle.abort();
        }
        if let Some(wakeup_join_handle) = state.wakeup_join_handle.take() {
            let _ = wakeup_join_handle.await;
        }

        gst_debug!(CAT, obj: element, "Scheduling wakeup in {}", delay);

        let (wakeup_fut, abort_handle) = abortable(Self::wakeup_fut(
            Duration::from_nanos(delay),
            latency_ns,
            context_wait_ns,
            &element,
            self.src_pad.downgrade(),
        ));
        state.wakeup_join_handle = Some(self.src_pad.spawn(wakeup_fut));
        state.wakeup_abort_handle = Some(abort_handle);
    }

    fn wakeup_fut(
        delay: Duration,
        latency_ns: gst::ClockTime,
        context_wait_ns: gst::ClockTime,
        element: &gst::Element,
        pad_src_weak: PadSrcWeak,
    ) -> BoxFuture<'static, ()> {
        let element = element.clone();
        async move {
            runtime::time::delay_for(delay).await;

            let jb = Self::from_instance(&element);
            let mut state = jb.state.lock().await;

            let pad_src = match pad_src_weak.upgrade() {
                Some(pad_src) => pad_src,
                None => return,
            };

            let pad_ctx = pad_src.pad_context();
            let pad_ctx = match pad_ctx.upgrade() {
                Some(pad_ctx) => pad_ctx,
                None => return,
            };

            let now = jb.get_current_running_time(&element);

            gst_debug!(
                CAT,
                obj: &element,
                "Woke back up, earliest_pts {}",
                state.earliest_pts
            );

            /* Check earliest PTS as we have just taken the lock */
            if state.earliest_pts.is_some()
                && state.earliest_pts + latency_ns - state.packet_spacing - context_wait_ns / 2
                    < now
            {
                loop {
                    let (head_pts, head_seq) = state.jbuf.borrow().peek();

                    state.last_res = jb.pop_and_push(&mut state, &element).await;

                    if let Some(drain_fut) = pad_ctx.drain_pending_tasks() {
                        let (abortable_drain, abort_handle) = abortable(drain_fut);
                        state.task_queue_abort_handle = Some(abort_handle);

                        pad_src.spawn(abortable_drain.map(drop));
                    } else {
                        state.task_queue_abort_handle = None;
                    }

                    let has_pending_tasks = state.task_queue_abort_handle.is_some();

                    if head_pts == state.earliest_pts && head_seq == state.earliest_seqnum as u32 {
                        let (earliest_pts, earliest_seqnum) = state.jbuf.borrow().find_earliest();
                        state.earliest_pts = earliest_pts;
                        state.earliest_seqnum = earliest_seqnum as u16;
                    }

                    if has_pending_tasks
                        || state.earliest_pts.is_none()
                        || state.earliest_pts + latency_ns - state.packet_spacing >= now
                    {
                        break;
                    }
                }
            }

            jb.schedule(&mut state, &element).await;
        }
        .boxed()
    }

    async fn enqueue_item(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().await;

        let mut buffers = VecDeque::new();
        if let Some(buf) = buffer {
            buffers.push_back(buf);
        }

        // This is to avoid recursion with `store`, `reset` and `enqueue_item`
        while let Some(buf) = buffers.pop_front() {
            if let Err(err) = self.store(&mut state, pad, element, buf).await {
                match err {
                    gst::FlowError::CustomError => {
                        for gap_packet in &self.reset(&mut state, element) {
                            buffers.push_back(gap_packet.0.to_owned());
                        }
                    }
                    other => return Err(other),
                }
            }
        }

        self.schedule(&mut state, element).await;

        state.last_res
    }

    async fn drain(&self, state: &mut MutexGuard<'_, State>, element: &gst::Element) -> bool {
        let mut ret = true;

        loop {
            let (head_pts, _) = state.jbuf.borrow().peek();

            if head_pts == gst::CLOCK_TIME_NONE {
                break;
            }

            if self.pop_and_push(state, element).await.is_err() {
                ret = false;
                break;
            }
        }

        ret
    }

    async fn flush(&self, element: &gst::Element) {
        let mut state = self.state.lock().await;

        gst_info!(CAT, obj: element, "Flushing");

        *state = State::default();
    }

    async fn clear_pt_map(&self, element: &gst::Element) {
        gst_info!(CAT, obj: element, "Clearing PT map");

        let mut state = self.state.lock().await;
        state.clock_rate = -1;
        state.jbuf.borrow().reset_skew();
    }
}

impl ObjectSubclass for JitterBuffer {
    const NAME: &'static str = "RsTsJitterBuffer";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
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
                    .get::<gst::Element>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let jitterbuffer = Self::from_instance(&element);
                runtime::executor::block_on(jitterbuffer.clear_pt_map(&element));
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

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sink_pad = PadSink::new_from_template(&templ, Some("sink"));

        let templ = klass.get_pad_template("src").unwrap();
        let src_pad = PadSrc::new_from_template(&templ, Some("src"));

        Self {
            sink_pad,
            src_pad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for JitterBuffer {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("latency", ..) => {
                let latency_ms = {
                    let mut settings = runtime::executor::block_on(self.settings.lock());
                    settings.latency_ms = value.get_some().expect("type checked upstream");
                    settings.latency_ms as u64
                };

                runtime::executor::block_on(self.state.lock())
                    .jbuf
                    .borrow()
                    .set_delay(latency_ms * gst::MSECOND);

                /* TODO: post message */
            }
            subclass::Property("do-lost", ..) => {
                let mut settings = runtime::executor::block_on(self.settings.lock());
                settings.do_lost = value.get_some().expect("type checked upstream");
            }
            subclass::Property("max-dropout-time", ..) => {
                let mut settings = runtime::executor::block_on(self.settings.lock());
                settings.max_dropout_time = value.get_some().expect("type checked upstream");
            }
            subclass::Property("max-misorder-time", ..) => {
                let mut settings = runtime::executor::block_on(self.settings.lock());
                settings.max_misorder_time = value.get_some().expect("type checked upstream");
            }
            subclass::Property("context", ..) => {
                let mut settings = runtime::executor::block_on(self.settings.lock());
                settings.context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                let mut settings = runtime::executor::block_on(self.settings.lock());
                settings.context_wait = value.get_some().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("latency", ..) => {
                let settings = runtime::executor::block_on(self.settings.lock());
                Ok(settings.latency_ms.to_value())
            }
            subclass::Property("do-lost", ..) => {
                let settings = runtime::executor::block_on(self.settings.lock());
                Ok(settings.do_lost.to_value())
            }
            subclass::Property("max-dropout-time", ..) => {
                let settings = runtime::executor::block_on(self.settings.lock());
                Ok(settings.max_dropout_time.to_value())
            }
            subclass::Property("max-misorder-time", ..) => {
                let settings = runtime::executor::block_on(self.settings.lock());
                Ok(settings.max_misorder_time.to_value())
            }
            subclass::Property("stats", ..) => {
                let state = runtime::executor::block_on(self.state.lock());
                let s = gst::Structure::new(
                    "application/x-rtp-jitterbuffer-stats",
                    &[
                        ("num-pushed", &state.num_pushed),
                        ("num-lost", &state.num_lost),
                        ("num-late", &state.num_late),
                    ],
                );
                Ok(s.to_value())
            }
            subclass::Property("context", ..) => {
                let settings = runtime::executor::block_on(self.settings.lock());
                Ok(settings.context.to_value())
            }
            subclass::Property("context-wait", ..) => {
                let settings = runtime::executor::block_on(self.settings.lock());
                Ok(settings.context_wait.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(self.sink_pad.gst_pad()).unwrap();
        element.add_pad(self.src_pad.gst_pad()).unwrap();
    }
}

impl ElementImpl for JitterBuffer {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => runtime::executor::block_on(async {
                let _state = self.state.lock().await;

                let context = {
                    let settings = self.settings.lock().await;
                    Context::acquire(&settings.context, settings.context_wait).unwrap()
                };
                let _ = self
                    .src_pad
                    .prepare(context, &JitterBufferPadSrcHandler)
                    .await
                    .map_err(|err| {
                        gst_error_msg!(
                            gst::ResourceError::OpenRead,
                            ["Error preparing src_pad: {:?}", err]
                        );
                        gst::StateChangeError
                    });

                self.sink_pad.prepare(&JitterBufferPadSinkHandler).await;
            }),
            gst::StateChange::PausedToReady => runtime::executor::block_on(async {
                let mut state = self.state.lock().await;

                if let Some(wakeup_abort_handle) = state.wakeup_abort_handle.take() {
                    wakeup_abort_handle.abort();
                }

                if let Some(abort_handle) = state.task_queue_abort_handle.take() {
                    abort_handle.abort();
                }
            }),
            gst::StateChange::ReadyToNull => runtime::executor::block_on(async {
                let mut state = self.state.lock().await;

                self.sink_pad.unprepare().await;
                let _ = self.src_pad.unprepare().await;

                state.jbuf.borrow().flush();

                if let Some(wakeup_abort_handle) = state.wakeup_abort_handle.take() {
                    wakeup_abort_handle.abort();
                }
            }),
            _ => (),
        }

        self.parent_change_state(element, transition)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-jitterbuffer",
        gst::Rank::None,
        JitterBuffer::get_type(),
    )
}
