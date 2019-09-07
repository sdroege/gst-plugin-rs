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

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_rtp::RTPBuffer;

use std::cmp::{max, min, Ordering};
use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard};
use std::time;

use futures::sync::oneshot;
use futures::Future;

use iocontext::*;

use RTPJitterBuffer;
use RTPJitterBufferItem;
use RTPPacketRateCtx;

const DEFAULT_LATENCY_MS: u32 = 200;
const DEFAULT_DO_LOST: bool = false;
const DEFAULT_MAX_DROPOUT_TIME: u32 = 60000;
const DEFAULT_MAX_MISORDER_TIME: u32 = 2000;
const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 20;

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
struct GapPacket(gst::Buffer);

impl Ord for GapPacket {
    fn cmp(&self, other: &Self) -> Ordering {
        let mut rtp_buffer = RTPBuffer::from_buffer_readable(&self.0).unwrap();
        let mut other_rtp_buffer = RTPBuffer::from_buffer_readable(&other.0).unwrap();

        let seq = rtp_buffer.get_seq();
        let other_seq = other_rtp_buffer.get_seq();

        drop(rtp_buffer);
        drop(other_rtp_buffer);

        let gap = gst_rtp::compare_seqnum(seq, other_seq);

        if gap < 0 {
            Ordering::Greater
        } else if gap == 0 {
            Ordering::Equal
        } else {
            Ordering::Less
        }
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
    io_context: Option<IOContext>,
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
    cancel: Option<oneshot::Sender<()>>,
    last_res: Result<gst::FlowSuccess, gst::FlowError>,
    pending_future_id: Option<PendingFutureId>,
    pending_future_cancel: Option<futures::sync::oneshot::Sender<()>>,
}

impl Default for State {
    fn default() -> State {
        State {
            io_context: None,
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
            cancel: None,
            last_res: Ok(gst::FlowSuccess::Ok),
            pending_future_id: None,
            pending_future_cancel: None,
        }
    }
}

struct JitterBuffer {
    cat: gst::DebugCategory,
    sink_pad: gst::Pad,
    src_pad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
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

        gst_info!(self.cat, obj: element, "Parsing caps: {:?}", caps);

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
            self.cat,
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
                    self.cat,
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

            gst_debug!(
                self.cat,
                obj: element,
                "all consecutive: {}",
                all_consecutive
            );

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
        state: &mut MutexGuard<State>,
        pad: &gst::Pad,
        element: &gst::Element,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_info!(self.cat, obj: element, "Resetting");

        state.jbuf.borrow().flush();
        state.jbuf.borrow().reset_skew();
        state.discont = true;
        state.last_popped_seqnum = std::u32::MAX;
        state.last_in_seqnum = std::u32::MAX;
        state.ips_rtptime = 0;
        state.ips_pts = gst::CLOCK_TIME_NONE;
        let mut ret = Ok(gst::FlowSuccess::Ok);

        let gap_packets = state.gap_packets.take();

        state.gap_packets = Some(BTreeSet::new());

        for gap_packet in &gap_packets.unwrap() {
            ret = self.enqueue_item(state, pad, element, Some(gap_packet.0.to_owned()));

            if ret != Ok(gst::FlowSuccess::Ok) {
                break;
            }
        }

        ret
    }

    fn store(
        &self,
        state: &mut MutexGuard<State>,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();
        let max_misorder_time = settings.max_misorder_time;
        let max_dropout_time = settings.max_dropout_time;
        drop(settings);

        let mut rtp_buffer =
            RTPBuffer::from_buffer_readable(&buffer).map_err(|_| gst::FlowError::Error)?;
        let seq = rtp_buffer.get_seq();
        let rtptime = rtp_buffer.get_timestamp();
        let pt = rtp_buffer.get_payload_type();
        let mut pts = buffer.get_pts();
        let mut dts = buffer.get_dts();
        let mut estimated_dts = false;
        drop(rtp_buffer);

        gst_log!(
            self.cat,
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

            gst_debug!(self.cat, obj: pad, "New payload type: {}", pt);

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

        if state.last_in_seqnum != std::u32::MAX {
            let gap = gst_rtp::compare_seqnum(state.last_in_seqnum as u16, seq);
            if gap == 1 {
                self.calculate_packet_spacing(state, rtptime, pts);
            } else if (gap != -1 && gap < -(max_misorder as i32)) || (gap >= max_dropout as i32) {
                let reset = self.handle_big_gap_buffer(state, element, buffer, pt);
                if reset {
                    return self.reset(state, pad, element);
                } else {
                    return Ok(gst::FlowSuccess::Ok);
                }
            }

            state.gap_packets.as_mut().unwrap().clear();
        }

        if state.last_popped_seqnum != std::u32::MAX {
            let gap = gst_rtp::compare_seqnum(state.last_popped_seqnum as u16, seq);

            if gap <= 0 {
                state.num_late += 1;
                gst_debug!(self.cat, obj: element, "Dropping late {}", seq);
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

        gst_log!(self.cat, obj: pad, "Stored buffer");

        Ok(gst::FlowSuccess::Ok)
    }

    fn push_lost_events(
        &self,
        state: &mut MutexGuard<State>,
        element: &gst::Element,
        seqnum: u32,
        pts: gst::ClockTime,
        discont: &mut bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();
        let latency_ns = settings.latency_ms as i64 * gst::MSECOND.nseconds().unwrap() as i64;
        let do_lost = settings.do_lost;
        drop(settings);

        let mut ret = true;

        gst_debug!(
            self.cat,
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

                        ret = self.src_pad.push_event(event);
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

                        ret = self.src_pad.push_event(event);
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

    fn send_io_context_event(&self, state: &State) -> Result<gst::FlowSuccess, gst::FlowError> {
        if self.src_pad.check_reconfigure() {
            if let (&Some(ref pending_future_id), &Some(ref io_context)) =
                (&state.pending_future_id, &state.io_context)
            {
                let s = gst::Structure::new(
                    "ts-io-context",
                    &[
                        ("io-context", &io_context),
                        ("pending-future-id", &*pending_future_id),
                    ],
                );
                let event = gst::Event::new_custom_downstream_sticky(s).build();

                if !self.src_pad.push_event(event) {
                    return Err(gst::FlowError::Error);
                }
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn pop_and_push(
        &self,
        state: &mut MutexGuard<State>,
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

        self.push_lost_events(state, element, seq, pts, &mut discont)?;

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

        gst_debug!(self.cat, obj: &self.src_pad, "Pushing buffer {:?} with seq {}", buffer, seq);

        self.send_io_context_event(&state)?;

        self.src_pad.push(buffer.to_owned())
    }

    fn schedule(&self, state: &mut MutexGuard<State>, element: &gst::Element) {
        let settings = self.settings.lock().unwrap().clone();
        let latency_ns = settings.latency_ms as u64 * gst::MSECOND;
        drop(settings);

        let now = self.get_current_running_time(element);

        gst_debug!(
            self.cat,
            obj: element,
            "now is {}, earliest pts is {}, packet_spacing {} and latency {}",
            now,
            state.earliest_pts,
            state.packet_spacing,
            latency_ns
        );

        if state.earliest_pts.is_some() {
            let next_wakeup = state.earliest_pts + latency_ns - state.packet_spacing;

            let timeout = {
                if next_wakeup > now {
                    (next_wakeup - now).nseconds().unwrap()
                } else {
                    0
                }
            };

            if let Some(cancel) = state.cancel.take() {
                let _ = cancel.send(());
            }

            let (cancel, cancel_handler) = oneshot::channel();

            let element_clone = element.clone();

            gst_debug!(self.cat, obj: element, "Scheduling wakeup in {}", timeout);

            let timer = Timeout::new(
                state.io_context.as_ref().unwrap(),
                time::Duration::from_nanos(timeout),
            )
            .map_err(|e| panic!("timer failed; err={:?}", e))
            .and_then(move |_| {
                let jb = Self::from_instance(&element_clone);
                let mut state = jb.state.lock().unwrap();
                let now = jb.get_current_running_time(&element_clone);

                gst_debug!(
                    jb.cat,
                    obj: &element_clone,
                    "Woke back up, earliest_pts {}",
                    state.earliest_pts
                );

                let _ = state.cancel.take();

                /* Check earliest PTS as we have just taken the lock */
                if state.earliest_pts.is_some()
                    && state.earliest_pts + latency_ns - state.packet_spacing < now
                {
                    loop {
                        let (head_pts, head_seq) = state.jbuf.borrow().peek();

                        state.last_res = jb.pop_and_push(&mut state, &element_clone);

                        if let Some(pending_future_id) = state.pending_future_id {
                            let (cancel, future) = state
                                .io_context
                                .as_ref()
                                .unwrap()
                                .drain_pending_futures(pending_future_id);

                            state.pending_future_cancel = cancel;

                            state.io_context.as_ref().unwrap().spawn(future);
                        }

                        if head_pts == state.earliest_pts
                            && head_seq == state.earliest_seqnum as u32
                        {
                            let (earliest_pts, earliest_seqnum) =
                                state.jbuf.borrow().find_earliest();
                            state.earliest_pts = earliest_pts;
                            state.earliest_seqnum = earliest_seqnum as u16;
                        }

                        if state.pending_future_cancel.is_some()
                            || state.earliest_pts.is_none()
                            || state.earliest_pts + latency_ns - state.packet_spacing >= now
                        {
                            break;
                        }
                    }
                }

                jb.schedule(&mut state, &element_clone);

                Ok(())
            });

            let future = timer.select(cancel_handler).then(|_| Ok(()));

            state.cancel = Some(cancel);

            state.io_context.as_ref().unwrap().spawn(future);
        }
    }

    fn enqueue_item(
        &self,
        state: &mut MutexGuard<State>,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if let Some(buf) = buffer {
            self.store(state, pad, element, buf)?;
        }

        self.schedule(state, element);

        state.last_res
    }

    fn drain(&self, state: &mut MutexGuard<State>, element: &gst::Element) -> bool {
        let mut ret = true;

        loop {
            let (head_pts, _) = state.jbuf.borrow().peek();

            if head_pts == gst::CLOCK_TIME_NONE {
                break;
            }

            if self.pop_and_push(state, element).is_err() {
                ret = false;
                break;
            }
        }

        ret
    }

    fn flush(&self, element: &gst::Element) {
        let mut state = self.state.lock().unwrap();

        gst_info!(self.cat, obj: element, "Flushing");

        let io_context = state.io_context.take();

        *state = State::default();

        state.io_context = io_context;
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_debug!(self.cat, obj: pad, "Handling buffer {:?}", buffer);
        let mut state = self.state.lock().unwrap();
        self.enqueue_item(&mut state, pad, element, Some(buffer))
    }

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        let mut forward = true;
        use gst::EventView;

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::FlushStop(..) => {
                self.flush(element);
            }
            EventView::Segment(e) => {
                let mut state = self.state.lock().unwrap();
                state.segment = e
                    .get_segment()
                    .clone()
                    .downcast::<gst::format::Time>()
                    .unwrap();
            }
            EventView::Eos(..) => {
                let mut state = self.state.lock().unwrap();
                self.drain(&mut state, element);
            }
            EventView::CustomDownstreamSticky(e) => {
                let s = e.get_structure().unwrap();
                if s.get_name() == "ts-io-context" {
                    forward = false;
                }
            }
            _ => (),
        };

        if forward {
            gst_log!(self.cat, obj: pad, "Forwarding event {:?}", event);
            self.src_pad.push_event(event)
        } else {
            true
        }
    }

    fn sink_query(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;
        gst_log!(self.cat, obj: pad, "Forwarding query {:?}", query);

        match query.view_mut() {
            QueryView::Drain(..) => {
                let mut state = self.state.lock().unwrap();
                gst_info!(self.cat, obj: pad, "Draining");
                self.enqueue_item(&mut state, pad, element, None).is_ok()
            }
            _ => self.src_pad.peer_query(query),
        }
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(self.cat, obj: pad, "Forwarding query {:?}", query);

        match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                let mut peer_query = gst::query::Query::new_latency();

                let ret = self.sink_pad.peer_query(&mut peer_query);

                if ret {
                    let (_, mut min_latency, _) = peer_query.get_result();
                    let settings = self.settings.lock().unwrap().clone();
                    let our_latency = settings.latency_ms as u64 * gst::MSECOND;
                    drop(settings);

                    min_latency += our_latency;
                    let max_latency = gst::CLOCK_TIME_NONE;

                    q.set(true, min_latency, max_latency);
                }

                ret
            }
            QueryView::Position(ref mut q) => {
                if q.get_format() != gst::Format::Time {
                    self.sink_pad.peer_query(query)
                } else {
                    let state = self.state.lock().unwrap();
                    q.set(state.segment.get_position());
                    true
                }
            }
            _ => self.sink_pad.peer_query(query),
        }
    }

    fn clear_pt_map(&self, element: &gst::Element) {
        gst_info!(self.cat, obj: element, "Clearing PT map");

        let mut state = self.state.lock().unwrap();
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
                jitterbuffer.clear_pt_map(&element);
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
        let sink_pad = gst::Pad::new_from_template(&templ, Some("sink"));
        let templ = klass.get_pad_template("src").unwrap();
        let src_pad = gst::Pad::new_from_template(&templ, Some("src"));

        sink_pad.set_chain_function(|pad, parent, buffer| {
            JitterBuffer::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |queue, element| queue.sink_chain(pad, element, buffer),
            )
        });
        sink_pad.set_event_function(|pad, parent, event| {
            JitterBuffer::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.sink_event(pad, element, event),
            )
        });
        sink_pad.set_query_function(|pad, parent, query| {
            JitterBuffer::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.sink_query(pad, element, query),
            )
        });

        src_pad.set_query_function(|pad, parent, query| {
            JitterBuffer::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.src_query(pad, element, query),
            )
        });

        Self {
            cat: gst::DebugCategory::new(
                "ts-jitterbuffer",
                gst::DebugColorFlags::empty(),
                Some("Thread-sharing jitterbuffer"),
            ),
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
                let mut settings = self.settings.lock().unwrap();
                settings.latency_ms = value.get_some().expect("type checked upstream");

                let state = self.state.lock().unwrap();
                state
                    .jbuf
                    .borrow()
                    .set_delay(settings.latency_ms as u64 * gst::MSECOND);

                /* TODO: post message */
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

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("latency", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.latency_ms.to_value())
            }
            subclass::Property("do-lost", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.do_lost.to_value())
            }
            subclass::Property("max-dropout-time", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.max_dropout_time.to_value())
            }
            subclass::Property("max-misorder-time", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.max_misorder_time.to_value())
            }
            subclass::Property("stats", ..) => {
                let state = self.state.lock().unwrap();
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
                let settings = self.settings.lock().unwrap();
                Ok(settings.context.to_value())
            }
            subclass::Property("context-wait", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.context_wait.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sink_pad).unwrap();
        element.add_pad(&self.src_pad).unwrap();
    }
}

impl ElementImpl for JitterBuffer {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(self.cat, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                let settings = self.settings.lock().unwrap().clone();
                let mut state = self.state.lock().unwrap();

                state.io_context =
                    Some(IOContext::new(&settings.context, settings.context_wait).unwrap());
                state.pending_future_id = Some(
                    state
                        .io_context
                        .as_ref()
                        .unwrap()
                        .acquire_pending_future_id(),
                );
            }
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                let _ = state.pending_future_cancel.take();
            }
            gst::StateChange::ReadyToNull => {
                let mut state = self.state.lock().unwrap();

                let _ = state.io_context.take();
            }
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
