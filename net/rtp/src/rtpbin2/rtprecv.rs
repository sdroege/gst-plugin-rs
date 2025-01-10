// SPDX-License-Identifier: MPL-2.0

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant, SystemTime};

use futures::StreamExt;
use gst::{glib, prelude::*, subclass::prelude::*};
use std::sync::LazyLock;

use super::internal::{pt_clock_rate_from_caps, GstRustLogger, SharedRtpState, SharedSession};
use super::jitterbuffer::{self, JitterBuffer};
use super::session::{
    KeyUnitRequestType, RecvReply, RequestRemoteKeyUnitReply, RtcpRecvReply, RtpProfile,
    RTCP_MIN_REPORT_INTERVAL,
};
use super::source::SourceState;
use super::sync;

use crate::rtpbin2::RUNTIME;

const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::from_mseconds(200);

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtprecv",
        gst::DebugColorFlags::empty(),
        Some("RTP session receiver"),
    )
});

#[derive(Debug, Clone)]
struct Settings {
    rtp_id: String,
    latency: gst::ClockTime,
    timestamping_mode: sync::TimestampingMode,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            rtp_id: String::from("rtp-id"),
            latency: DEFAULT_LATENCY,
            timestamping_mode: sync::TimestampingMode::default(),
        }
    }
}

#[derive(Debug)]
#[must_use = "futures/streams/sinks do nothing unless you `.await` or poll them"]
struct JitterBufferStream {
    store: Arc<Mutex<JitterBufferStore>>,
    sleep: Pin<Box<tokio::time::Sleep>>,
    pending_item: Option<JitterBufferItem>,
}

impl JitterBufferStream {
    fn new(store: Arc<Mutex<JitterBufferStore>>) -> Self {
        Self {
            store,
            sleep: Box::pin(tokio::time::sleep(Duration::from_secs(1))),
            pending_item: None,
        }
    }
}

impl futures::stream::Stream for JitterBufferStream {
    type Item = JitterBufferItem;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let now = Instant::now();
        let mut lowest_wait = None;

        if let Some(item) = self.pending_item.take() {
            return Poll::Ready(Some(item));
        }

        let mut jitterbuffer_store = self.store.lock().unwrap();
        let mut pending_item = None;
        let mut next_pending_item = None;
        loop {
            let ret = jitterbuffer_store.jitterbuffer.poll(now);
            gst::trace!(CAT, "jitterbuffer poll ret: {ret:?}");
            match ret {
                jitterbuffer::PollResult::Flushing => {
                    return Poll::Ready(None);
                }
                jitterbuffer::PollResult::Drop(id) => {
                    jitterbuffer_store
                        .store
                        .remove(&id)
                        .unwrap_or_else(|| panic!("Buffer with id {id} not in store!"));
                }
                jitterbuffer::PollResult::Forward { id, discont } => {
                    let mut item = jitterbuffer_store
                        .store
                        .remove(&id)
                        .unwrap_or_else(|| panic!("Buffer with id {id} not in store!"));
                    if let JitterBufferItem::Packet(ref mut packet) = item {
                        if discont {
                            gst::debug!(CAT, "Forwarding discont buffer");
                            let packet_mut = packet.make_mut();
                            packet_mut.set_flags(gst::BufferFlags::DISCONT);
                        }
                    }

                    match item {
                        // we don't currently push packet lists into the jitterbuffer
                        JitterBufferItem::PacketList(_list) => unreachable!(),
                        // forward events and queries as-is
                        JitterBufferItem::Event(_) | JitterBufferItem::Query(_, _) => {
                            if pending_item.is_some() {
                                // but only after sending the previous pending item
                                next_pending_item = Some(item);
                                break;
                            }
                        }
                        JitterBufferItem::Packet(ref packet) => {
                            match pending_item {
                                Some(
                                    JitterBufferItem::Event(_) | JitterBufferItem::Query(_, _),
                                ) => unreachable!(),
                                Some(JitterBufferItem::Packet(pending_buffer)) => {
                                    let mut list = gst::BufferList::new();
                                    let list_mut = list.make_mut();
                                    list_mut.add(pending_buffer);
                                    list_mut.add(packet.clone());
                                    pending_item = Some(JitterBufferItem::PacketList(list));
                                }
                                Some(JitterBufferItem::PacketList(mut pending_list)) => {
                                    let list_mut = pending_list.make_mut();
                                    list_mut.add(packet.clone());
                                    pending_item = Some(JitterBufferItem::PacketList(pending_list));
                                }
                                None => {
                                    pending_item = Some(item);
                                }
                            }
                            continue;
                        }
                    }
                    return Poll::Ready(Some(item));
                }
                jitterbuffer::PollResult::Timeout(timeout) => {
                    if lowest_wait.map_or(true, |lowest_wait| timeout < lowest_wait) {
                        lowest_wait = Some(timeout);
                    }
                    break;
                }
                // Will be woken up when necessary
                jitterbuffer::PollResult::Empty => break,
            }
        }

        jitterbuffer_store.waker = Some(cx.waker().clone());
        drop(jitterbuffer_store);

        if next_pending_item.is_some() {
            self.pending_item = next_pending_item;
        }

        if pending_item.is_some() {
            return Poll::Ready(pending_item);
        }

        if let Some(timeout) = lowest_wait {
            let this = self.get_mut();
            this.sleep.as_mut().reset(timeout.into());
            if !std::future::Future::poll(this.sleep.as_mut(), cx).is_pending() {
                cx.waker().wake_by_ref();
            }
        }

        Poll::Pending
    }
}

#[derive(Debug)]
enum JitterBufferItem {
    Packet(gst::Buffer),
    PacketList(gst::BufferList),
    Event(gst::Event),
    Query(
        std::ptr::NonNull<gst::QueryRef>,
        std::sync::mpsc::SyncSender<bool>,
    ),
}

// SAFETY: Need to be able to pass *mut gst::QueryRef
unsafe impl Send for JitterBufferItem {}

#[derive(Debug)]
struct JitterBufferStore {
    store: BTreeMap<usize, JitterBufferItem>,
    waker: Option<Waker>,
    jitterbuffer: JitterBuffer,
}

#[derive(Debug, Clone)]
struct RtpRecvSrcPad {
    pt: u8,
    ssrc: u32,
    pad: gst::Pad,
    jitter_buffer_store: Arc<Mutex<JitterBufferStore>>,
}

impl PartialEq for RtpRecvSrcPad {
    fn eq(&self, other: &Self) -> bool {
        self.pt == other.pt && self.ssrc == other.ssrc && self.pad == other.pad
    }
}

impl Eq for RtpRecvSrcPad {}

impl RtpRecvSrcPad {
    fn activate(&mut self, state: MutexGuard<State>, session_id: usize) {
        let session = state.session_by_id(session_id).unwrap();
        let seqnum = session.rtp_recv_sink_seqnum.unwrap();
        let stream_id = format!("{}/{}", self.pt, self.ssrc);
        let stream_start = gst::event::StreamStart::builder(&stream_id)
            .group_id(session.rtp_recv_sink_group_id.unwrap())
            .seqnum(seqnum)
            .build();

        let session_inner = session.internal_session.inner.lock().unwrap();
        let caps = session_inner.caps_from_pt(self.pt);
        let caps = gst::event::Caps::builder(&caps).seqnum(seqnum).build();
        drop(session_inner);

        let segment = gst::event::Segment::builder(session.rtp_recv_sink_segment.as_ref().unwrap())
            .seqnum(seqnum)
            .build();
        drop(state);

        self.pad.set_active(true).unwrap();
        let _ = self.pad.store_sticky_event(&stream_start);
        let _ = self.pad.store_sticky_event(&caps);
        let _ = self.pad.store_sticky_event(&segment);
    }
}

#[derive(Debug)]
struct HeldRecvBuffer {
    hold_id: Option<usize>,
    buffer: gst::Buffer,
    jb: Arc<Mutex<JitterBufferStore>>,
}

#[derive(Debug)]
struct HeldRecvBufferList {
    list: gst::BufferList,
    jb: Arc<Mutex<JitterBufferStore>>,
}

#[derive(Debug)]
enum HeldRecvItem {
    NewPad(RtpRecvSrcPad),
    Buffer(HeldRecvBuffer),
    BufferList(HeldRecvBufferList),
}

impl HeldRecvItem {
    fn hold_id(&self) -> Option<usize> {
        match self {
            Self::NewPad(_) => None,
            Self::Buffer(buf) => buf.hold_id,
            Self::BufferList(_list) => None,
        }
    }
}

#[derive(Debug)]
struct RecvSession {
    internal_session: SharedSession,

    // State for received RTP streams
    rtp_recv_sinkpad: Option<gst::Pad>,
    rtp_recv_sink_group_id: Option<gst::GroupId>,
    rtp_recv_sink_caps: Option<gst::Caps>,
    rtp_recv_sink_segment: Option<gst::FormattedSegment<gst::ClockTime>>,
    rtp_recv_sink_seqnum: Option<gst::Seqnum>,

    recv_store: Vec<HeldRecvItem>,

    rtp_recv_srcpads: Vec<RtpRecvSrcPad>,
    recv_flow_combiner: Arc<Mutex<gst_base::UniqueFlowCombiner>>,

    rtcp_recv_sinkpad: Option<gst::Pad>,
}

impl RecvSession {
    fn new(shared_state: &SharedRtpState, id: usize) -> Self {
        let internal_session = shared_state.session_get_or_init(id, || {
            SharedSession::new(id, RtpProfile::Avp, RTCP_MIN_REPORT_INTERVAL, false)
        });
        Self {
            internal_session,
            rtp_recv_sinkpad: None,
            rtp_recv_sink_group_id: None,
            rtp_recv_sink_caps: None,
            rtp_recv_sink_segment: None,
            rtp_recv_sink_seqnum: None,

            recv_store: vec![],

            rtp_recv_srcpads: vec![],
            recv_flow_combiner: Arc::new(Mutex::new(gst_base::UniqueFlowCombiner::new())),

            rtcp_recv_sinkpad: None,
        }
    }

    fn start_rtp_task(&mut self, pad: &gst::Pad) -> Result<(), glib::BoolError> {
        gst::debug!(CAT, obj = pad, "Starting rtp recv src task");

        let recv_pad = self
            .rtp_recv_srcpads
            .iter_mut()
            .find(|recv| &recv.pad == pad)
            .unwrap();

        let pad_weak = pad.downgrade();
        let recv_flow_combiner = self.recv_flow_combiner.clone();
        let store = recv_pad.jitter_buffer_store.clone();

        {
            let mut store = store.lock().unwrap();
            store.jitterbuffer.set_flushing(false);
            store.waker.take();
        }

        // A task per received ssrc may be a bit excessive.
        // Other options are:
        // - Single task per received input stream rather than per output ssrc/pt
        // - somehow pool multiple recv tasks together (thread pool)
        pad.start_task(move || {
            let Some(pad) = pad_weak.upgrade() else {
                return;
            };

            let recv_flow_combiner = recv_flow_combiner.clone();
            let store = store.clone();

            RUNTIME.block_on(async move {
                let mut stream = JitterBufferStream::new(store);
                while let Some(item) = stream.next().await {
                    match item {
                        JitterBufferItem::PacketList(list) => {
                            let flow = pad.push_list(list);
                            gst::trace!(CAT, obj = pad, "Pushed buffer list, flow ret {:?}", flow);
                            let mut recv_flow_combiner = recv_flow_combiner.lock().unwrap();
                            let _combined_flow = recv_flow_combiner.update_pad_flow(&pad, flow);
                            // TODO: store flow, return only on session pads?
                        }
                        JitterBufferItem::Packet(buffer) => {
                            let flow = pad.push(buffer);
                            gst::trace!(CAT, obj = pad, "Pushed buffer, flow ret {:?}", flow);
                            let mut recv_flow_combiner = recv_flow_combiner.lock().unwrap();
                            let _combined_flow = recv_flow_combiner.update_pad_flow(&pad, flow);
                            // TODO: store flow, return only on session pads?
                        }
                        JitterBufferItem::Event(event) => {
                            let res = pad.push_event(event);
                            gst::trace!(CAT, obj = pad, "Pushed serialized event, result: {}", res);
                        }
                        JitterBufferItem::Query(mut query, tx) => {
                            // This is safe because the thread holding the original reference is waiting
                            // for us exclusively
                            let res = pad.peer_query(unsafe { query.as_mut() });
                            let _ = tx.send(res);
                        }
                    }
                }
            })
        })?;

        gst::debug!(CAT, obj = pad, "Task started");

        Ok(())
    }

    fn stop_rtp_task(&mut self, pad: &gst::Pad) -> Result<(), glib::BoolError> {
        gst::debug!(CAT, obj = pad, "Stopping rtp recv src task");
        let recv_pad = self
            .rtp_recv_srcpads
            .iter_mut()
            .find(|recv| &recv.pad == pad)
            .unwrap();

        let mut store = recv_pad.jitter_buffer_store.lock().unwrap();
        store.jitterbuffer.set_flushing(true);
        if let Some(waker) = store.waker.take() {
            waker.wake();
        }

        Ok(())
    }

    fn get_or_create_rtp_src(
        &mut self,
        rtpbin: &RtpRecv,
        pt: u8,
        ssrc: u32,
    ) -> (RtpRecvSrcPad, bool) {
        if let Some(pad) = self
            .rtp_recv_srcpads
            .iter()
            .find(|&r| r.ssrc == ssrc && r.pt == pt)
        {
            (pad.clone(), false)
        } else {
            let src_templ = rtpbin.obj().pad_template("rtp_src_%u_%u_%u").unwrap();
            let id = self.internal_session.id;
            let srcpad = gst::Pad::builder_from_template(&src_templ)
                .iterate_internal_links_function(|pad, parent| {
                    RtpRecv::catch_panic_pad_function(
                        parent,
                        || gst::Iterator::from_vec(vec![]),
                        |this| this.iterate_internal_links(pad),
                    )
                })
                .query_function(|pad, parent, query| {
                    RtpRecv::catch_panic_pad_function(
                        parent,
                        || false,
                        |this| this.src_query(pad, query),
                    )
                })
                .event_function(move |pad, parent, event| {
                    RtpRecv::catch_panic_pad_function(
                        parent,
                        || false,
                        |this| this.rtp_src_event(pad, event, id, pt, ssrc),
                    )
                })
                .activatemode_function({
                    let this = rtpbin.downgrade();
                    move |pad, _parent, mode, active| {
                        let Some(this) = this.upgrade() else {
                            return Err(gst::LoggableError::new(
                                *CAT,
                                glib::bool_error!("rtprecv does not exist anymore"),
                            ));
                        };
                        this.rtp_src_activatemode(pad, mode, active, id)
                    }
                })
                .name(format!("rtp_src_{}_{}_{}", id, pt, ssrc))
                .build();

            srcpad.use_fixed_caps();

            let settings = rtpbin.settings.lock().unwrap();

            let recv_pad = RtpRecvSrcPad {
                pt,
                ssrc,
                pad: srcpad.clone(),
                jitter_buffer_store: Arc::new(Mutex::new(JitterBufferStore {
                    waker: None,
                    store: BTreeMap::new(),
                    jitterbuffer: JitterBuffer::new(settings.latency.into()),
                })),
            };

            self.recv_flow_combiner
                .lock()
                .unwrap()
                .add_pad(&recv_pad.pad);
            self.rtp_recv_srcpads.push(recv_pad.clone());
            (recv_pad, true)
        }
    }
}

#[derive(Debug, Default)]
struct State {
    shared_state: Option<SharedRtpState>,
    sessions: Vec<RecvSession>,
    max_session_id: usize,
    pads_session_id_map: HashMap<gst::Pad, usize>,
}

enum RecvRtpBuffer {
    IsRtcp(gst::Buffer),
    SsrcCollision(u32),
    Forward((gst::Buffer, Arc<Mutex<JitterBufferStore>>)),
    Drop,
}

impl State {
    fn session_by_id(&self, id: usize) -> Option<&RecvSession> {
        self.sessions
            .iter()
            .find(|session| session.internal_session.id == id)
    }

    fn mut_session_by_id(&mut self, id: usize) -> Option<&mut RecvSession> {
        self.sessions
            .iter_mut()
            .find(|session| session.internal_session.id == id)
    }

    fn stats(&self) -> gst::Structure {
        let mut ret = gst::Structure::builder("application/x-rtp2-stats");
        for session in self.sessions.iter() {
            let sess_id = session.internal_session.id;
            let session_inner = session.internal_session.inner.lock().unwrap();

            let mut session_stats = session_inner.stats();
            let jb_stats = gst::List::new(session.rtp_recv_srcpads.iter().map(|pad| {
                let mut jb_stats = pad.jitter_buffer_store.lock().unwrap().jitterbuffer.stats();
                jb_stats.set_value("ssrc", (pad.ssrc as i32).to_send_value());
                jb_stats.set_value("pt", (pad.pt as i32).to_send_value());
                jb_stats
            }));

            session_stats.set("jitterbuffer-stats", jb_stats);
            ret = ret.field(sess_id.to_string(), session_stats);
        }
        ret.build()
    }
}

pub struct RtpRecv {
    settings: Mutex<Settings>,
    state: Arc<Mutex<State>>,
    sync_context: Arc<Mutex<Option<sync::Context>>>,
}

impl RtpRecv {
    fn rtp_src_activatemode(
        &self,
        pad: &gst::Pad,
        mode: gst::PadMode,
        active: bool,
        id: usize,
    ) -> Result<(), gst::LoggableError> {
        if let gst::PadMode::Push = mode {
            let mut state = self.state.lock().unwrap();
            let Some(session) = state.mut_session_by_id(id) else {
                if active {
                    return Err(gst::LoggableError::new(
                        *CAT,
                        glib::bool_error!("Can't activate pad of unknown session {id}"),
                    ));
                } else {
                    return Ok(());
                }
            };

            if active {
                session.start_rtp_task(pad)?;
            } else {
                session.stop_rtp_task(pad)?;

                gst::debug!(CAT, obj = pad, "Stopping task");

                let _ = pad.stop_task();
            }

            Ok(())
        } else {
            Err(gst::LoggableError::new(
                *CAT,
                glib::bool_error!("Unsupported pad mode {mode:?}"),
            ))
        }
    }

    pub fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj = pad, "Handling query {query:?}");

        use gst::QueryViewMut::*;
        match query.view_mut() {
            Latency(q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = gst::Pad::query_default(pad, Some(&*self.obj()), &mut peer_query);
                let our_latency = self.settings.lock().unwrap().latency;

                let min = if ret {
                    let (_, min, _) = peer_query.result();

                    our_latency + min
                } else {
                    our_latency
                };

                gst::info!(
                    CAT,
                    obj = pad,
                    "Handled latency query, our latency {our_latency}, minimum latency: {min}"
                );
                q.set(true, min, gst::ClockTime::NONE);

                ret
            }
            _ => gst::Pad::query_default(pad, Some(pad), query),
        }
    }

    fn iterate_internal_links(&self, pad: &gst::Pad) -> gst::Iterator<gst::Pad> {
        let state = self.state.lock().unwrap();
        if let Some(&id) = state.pads_session_id_map.get(pad) {
            if let Some(session) = state.session_by_id(id) {
                if let Some(ref sinkpad) = session.rtp_recv_sinkpad {
                    if sinkpad == pad {
                        let pads = session
                            .rtp_recv_srcpads
                            .iter()
                            // Only include pads that are already part of the element
                            .filter(|r| state.pads_session_id_map.contains_key(&r.pad))
                            .map(|r| r.pad.clone())
                            .collect();
                        return gst::Iterator::from_vec(pads);
                    } else if session.rtp_recv_srcpads.iter().any(|r| &r.pad == pad) {
                        return gst::Iterator::from_vec(vec![sinkpad.clone()]);
                    }
                }
                // nothing to do for rtcp pads
            }
        }
        gst::Iterator::from_vec(vec![])
    }

    fn handle_buffer_locked<const H: usize, const P: usize>(
        &self,
        pad: &gst::Pad,
        session: &mut RecvSession,
        mut buffer: gst::Buffer,
        now: Instant,
        items_to_pre_push: &mut smallvec::SmallVec<[HeldRecvItem; P]>,
        held_buffers: &mut smallvec::SmallVec<[HeldRecvBuffer; H]>,
    ) -> Result<RecvRtpBuffer, gst::FlowError> {
        // TODO: this is different from the old C implementation, where we
        // simply used the RTP timestamps as they were instead of doing any
        // sort of skew calculations.
        //
        // Check if this makes sense or if this leads to issue with eg interleaved
        // TCP.
        let arrival_time = match buffer.dts() {
            Some(dts) => {
                let segment = session.rtp_recv_sink_segment.as_ref().unwrap();
                // TODO: use running_time_full if we care to support that
                match segment.to_running_time(dts) {
                    Some(time) => time,
                    None => {
                        gst::error!(CAT, obj = pad, "out of segment DTS are not supported");
                        return Err(gst::FlowError::Error);
                    }
                }
            }
            None => match self.obj().current_running_time() {
                Some(time) => time,
                None => {
                    gst::error!(CAT, obj = pad, "Failed to get current time");
                    return Err(gst::FlowError::Error);
                }
            },
        };

        let addr: Option<SocketAddr> =
            buffer
                .meta::<gst_net::NetAddressMeta>()
                .and_then(|net_meta| {
                    net_meta
                        .addr()
                        .dynamic_cast::<gio::InetSocketAddress>()
                        .map(|a| a.into())
                        .ok()
                });
        let mapped = buffer.map_readable().map_err(|e| {
            gst::error!(CAT, imp = self, "Failed to map input buffer {e:?}");
            gst::FlowError::Error
        })?;

        let rtp = match rtp_types::RtpPacket::parse(&mapped) {
            Ok(rtp) => rtp,
            Err(e) => {
                // If this is a valid RTCP packet then it was muxed with the RTP stream and can be
                // handled just fine.
                if rtcp_types::Compound::parse(&mapped)
                    .is_ok_and(|mut rtcp| rtcp.next().is_some_and(|rtcp| rtcp.is_ok()))
                {
                    drop(mapped);
                    return Ok(RecvRtpBuffer::IsRtcp(buffer));
                }

                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to parse input as valid rtp packet: {e:?}"
                );
                return Ok(RecvRtpBuffer::Drop);
            }
        };

        gst::trace!(CAT, obj = pad, "using arrival time {}", arrival_time);

        let internal_session = session.internal_session.clone();
        let mut session_inner = internal_session.inner.lock().unwrap();

        let pts = {
            let mut sync_context = self.sync_context.lock().unwrap();
            let sync_context = sync_context.as_mut().unwrap();
            if !sync_context.has_clock_rate(rtp.ssrc()) {
                let clock_rate = session_inner
                    .session
                    .clock_rate_from_pt(rtp.payload_type())
                    .unwrap();
                sync_context.set_clock_rate(rtp.ssrc(), clock_rate);
            }

            // TODO: Put NTP time as `gst::ReferenceTimeStampMeta` on the buffers if selected via property
            let (pts, _ntp_time) =
                sync_context.calculate_pts(rtp.ssrc(), rtp.timestamp(), arrival_time.nseconds());
            pts
        };

        let segment = session.rtp_recv_sink_segment.as_ref().unwrap();
        let pts = segment
            .position_from_running_time(gst::ClockTime::from_nseconds(pts))
            .unwrap();
        gst::debug!(CAT, obj = pad, "Calculated PTS: {}", pts);

        loop {
            let recv_ret = session_inner.session.handle_recv(&rtp, addr, now);
            gst::trace!(CAT, obj = pad, "session handle_recv ret: {recv_ret:?}");
            match recv_ret {
                RecvReply::SsrcCollision(ssrc) => return Ok(RecvRtpBuffer::SsrcCollision(ssrc)),
                RecvReply::NewSsrc(ssrc, _pt) => {
                    drop(session_inner);
                    internal_session
                        .config
                        .emit_by_name::<()>("new-ssrc", &[&ssrc]);
                    session_inner = internal_session.inner.lock().unwrap();
                }
                RecvReply::Hold(hold_id) => {
                    let pt = rtp.payload_type();
                    let ssrc = rtp.ssrc();
                    drop(mapped);
                    {
                        let buf_mut = buffer.make_mut();
                        buf_mut.set_pts(pts);
                    }
                    let (pad, new_pad) = session.get_or_create_rtp_src(self, pt, ssrc);
                    let jb = pad.jitter_buffer_store.clone();
                    if new_pad {
                        items_to_pre_push.push(HeldRecvItem::NewPad(pad));
                    }
                    held_buffers.push(HeldRecvBuffer {
                        hold_id: Some(hold_id),
                        buffer,
                        jb,
                    });
                    break;
                }
                RecvReply::Drop(hold_id) => {
                    if let Some(pos) = held_buffers.iter().position(|b| b.hold_id == Some(hold_id))
                    {
                        held_buffers.remove(pos);
                    } else if let Some(pos) = session
                        .recv_store
                        .iter()
                        .position(|b| b.hold_id() == Some(hold_id))
                    {
                        session.recv_store.remove(pos);
                    }
                }
                RecvReply::Forward(hold_id) => {
                    if let Some(pos) = held_buffers.iter().position(|b| b.hold_id == Some(hold_id))
                    {
                        items_to_pre_push.push(HeldRecvItem::Buffer(held_buffers.remove(pos)));
                    } else if let Some(pos) = session
                        .recv_store
                        .iter()
                        .position(|b| b.hold_id() == Some(hold_id))
                    {
                        items_to_pre_push.push(session.recv_store.remove(pos));
                    } else {
                        unreachable!();
                    }
                }
                RecvReply::Ignore => return Ok(RecvRtpBuffer::Drop),
                RecvReply::Passthrough => {
                    let pt = rtp.payload_type();
                    let ssrc = rtp.ssrc();
                    drop(mapped);
                    {
                        let buf_mut = buffer.make_mut();
                        buf_mut.set_pts(pts);
                    }
                    let (pad, new_pad) = session.get_or_create_rtp_src(self, pt, ssrc);
                    let jb = pad.jitter_buffer_store.clone();
                    if new_pad {
                        items_to_pre_push.push(HeldRecvItem::NewPad(pad));
                    }
                    return Ok(RecvRtpBuffer::Forward((buffer, jb)));
                }
            }
        }

        Ok(RecvRtpBuffer::Drop)
    }

    fn handle_ssrc_collision(
        &self,
        session: &mut RecvSession,
        ssrc_collision: impl IntoIterator<Item = u32>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let session_inner = session.internal_session.inner.lock().unwrap();
        let send_rtp_sink = session_inner.rtp_send_sinkpad.clone();
        drop(session_inner);

        if let Some(pad) = send_rtp_sink {
            // XXX: Another option is to have us rewrite ssrc's instead of asking upstream to do
            // so.
            for ssrc in ssrc_collision {
                pad.send_event(
                    gst::event::CustomUpstream::builder(
                        gst::Structure::builder("GstRTPCollision")
                            .field("ssrc", ssrc)
                            .build(),
                    )
                    .build(),
                );
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn handle_push_jitterbuffer<'a>(
        &'a self,
        mut state: MutexGuard<'a, State>,
        id: usize,
        buffers_to_push: impl IntoIterator<Item = HeldRecvItem>,
        now: Instant,
    ) -> Result<MutexGuard<'a, State>, gst::FlowError> {
        for mut held in buffers_to_push {
            match held {
                HeldRecvItem::NewPad(ref mut pad) => {
                    // TODO: handle other processing
                    state.pads_session_id_map.insert(pad.pad.clone(), id);
                    // drops the state lock
                    pad.activate(state, id);
                    self.obj().add_pad(&pad.pad).unwrap();
                    state = self.state.lock().unwrap();
                }
                HeldRecvItem::Buffer(buffer) => {
                    let mapped = buffer.buffer.map_readable().map_err(|e| {
                        gst::error!(CAT, imp = self, "Failed to map input buffer {e:?}");
                        gst::FlowError::Error
                    })?;
                    let rtp = match rtp_types::RtpPacket::parse(&mapped) {
                        Ok(rtp) => rtp,
                        Err(e) => {
                            gst::error!(
                                CAT,
                                imp = self,
                                "Failed to parse input as valid rtp packet: {e:?}"
                            );
                            return Ok(state);
                        }
                    };

                    // FIXME: Should block if too many packets are stored here because the source pad task
                    // is blocked
                    let mut jitterbuffer_store = buffer.jb.lock().unwrap();

                    let ret = jitterbuffer_store.jitterbuffer.queue_packet(
                        &rtp,
                        buffer.buffer.pts().unwrap().nseconds(),
                        now,
                    );
                    gst::trace!(CAT, "jb queue buffer: {ret:?}");
                    match ret {
                        jitterbuffer::QueueResult::Flushing => {
                            // TODO: return flushing result upstream
                        }
                        jitterbuffer::QueueResult::Queued(id) => {
                            drop(mapped);

                            jitterbuffer_store
                                .store
                                .insert(id, JitterBufferItem::Packet(buffer.buffer));
                            if let Some(waker) = jitterbuffer_store.waker.take() {
                                waker.wake()
                            }
                        }
                        jitterbuffer::QueueResult::Late => {
                            gst::warning!(CAT, "Late buffer was dropped");
                        }
                        jitterbuffer::QueueResult::Duplicate => {
                            gst::warning!(CAT, "Duplicate buffer was dropped");
                        }
                    }
                }
                HeldRecvItem::BufferList(list) => {
                    // FIXME: Should block if too many packets are stored here because the source pad task
                    // is blocked
                    let mut jitterbuffer_store = list.jb.lock().unwrap();

                    for buffer in list.list.iter_owned() {
                        let mapped = buffer.map_readable().map_err(|e| {
                            gst::error!(CAT, imp = self, "Failed to map input buffer {e:?}");
                            gst::FlowError::Error
                        })?;
                        let rtp = match rtp_types::RtpPacket::parse(&mapped) {
                            Ok(rtp) => rtp,
                            Err(e) => {
                                gst::error!(
                                    CAT,
                                    imp = self,
                                    "Failed to parse input as valid rtp packet: {e:?}"
                                );
                                return Ok(state);
                            }
                        };

                        let ret = jitterbuffer_store.jitterbuffer.queue_packet(
                            &rtp,
                            buffer.pts().unwrap().nseconds(),
                            now,
                        );
                        gst::trace!(CAT, "jb queue buffer in list: {ret:?}");
                        match ret {
                            jitterbuffer::QueueResult::Flushing => {
                                return Err(gst::FlowError::Flushing);
                            }
                            jitterbuffer::QueueResult::Queued(id) => {
                                drop(mapped);

                                jitterbuffer_store
                                    .store
                                    .insert(id, JitterBufferItem::Packet(buffer));

                                if let Some(waker) = jitterbuffer_store.waker.take() {
                                    waker.wake()
                                }
                            }
                            jitterbuffer::QueueResult::Late => {
                                gst::warning!(CAT, "Late buffer was dropped");
                            }
                            jitterbuffer::QueueResult::Duplicate => {
                                gst::warning!(CAT, "Duplicate buffer was dropped");
                            }
                        }
                    }
                }
            }
        }

        Ok(state)
    }

    fn rtp_sink_chain_list(
        &self,
        pad: &gst::Pad,
        id: usize,
        mut list: gst::BufferList,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let Some(session) = state.mut_session_by_id(id) else {
            return Err(gst::FlowError::Error);
        };

        let now = Instant::now();
        let mut ssrc_collision: smallvec::SmallVec<[u32; 4]> = Default::default();
        let mut items_to_pre_push: smallvec::SmallVec<[HeldRecvItem; 4]> =
            smallvec::SmallVec::with_capacity(list.len() + 2);
        let mut held_buffers: smallvec::SmallVec<[HeldRecvBuffer; 4]> = Default::default();
        let mut split_bufferlist = false;
        let mut previous_jb = None;
        let list_mut = list.make_mut();
        let mut ret = Ok(());
        list_mut.foreach_mut(|buffer, _i| {
            match self.handle_buffer_locked(
                pad,
                session,
                buffer,
                now,
                &mut items_to_pre_push,
                &mut held_buffers,
            ) {
                Ok(RecvRtpBuffer::SsrcCollision(ssrc)) => {
                    ssrc_collision.push(ssrc);
                    ControlFlow::Continue(None)
                }
                Ok(RecvRtpBuffer::IsRtcp(buffer)) => {
                    match Self::rtcp_sink_chain(self, id, buffer) {
                        Ok(_buf) => ControlFlow::Continue(None),
                        Err(e) => {
                            ret = Err(e);
                            ControlFlow::Break(None)
                        }
                    }
                }
                Ok(RecvRtpBuffer::Drop) => ControlFlow::Continue(None),
                Ok(RecvRtpBuffer::Forward((buffer, jb))) => {
                    // if all the buffers do not end up in the same jitterbuffer, then we need to
                    // split
                    if !split_bufferlist
                        && previous_jb
                            .as_ref()
                            .is_some_and(|previous| !Arc::ptr_eq(previous, &jb))
                    {
                        split_bufferlist = true;
                    }
                    previous_jb = Some(jb);
                    ControlFlow::Continue(Some(buffer))
                }
                Err(e) => {
                    ret = Err(e);
                    ControlFlow::Break(None)
                }
            }
        });
        ret?;
        session
            .recv_store
            .extend(held_buffers.into_iter().map(HeldRecvItem::Buffer));

        self.handle_ssrc_collision(session, ssrc_collision)?;
        state = self.handle_push_jitterbuffer(state, id, items_to_pre_push, now)?;
        if split_bufferlist {
            // this abomination is to work around passing state through handle_push_jitterbuffer
            // inside a closure
            let mut maybe_state = Some(state);
            list_mut.foreach_mut({
                let maybe_state = &mut maybe_state;
                |buffer, _i| match self.handle_push_jitterbuffer(
                    maybe_state.take().unwrap(),
                    id,
                    [HeldRecvItem::Buffer(HeldRecvBuffer {
                        hold_id: None,
                        buffer,
                        jb: previous_jb.clone().unwrap(),
                    })],
                    now,
                ) {
                    Ok(state) => {
                        *maybe_state = Some(state);
                        ControlFlow::Continue(None)
                    }
                    Err(e) => {
                        ret = Err(e);
                        ControlFlow::Break(None)
                    }
                }
            });
            state = maybe_state.unwrap();
            ret?;
        } else {
            state = self.handle_push_jitterbuffer(
                state,
                id,
                [HeldRecvItem::BufferList(HeldRecvBufferList {
                    list,
                    jb: previous_jb.unwrap(),
                })],
                now,
            )?;
        }
        drop(state);

        Ok(gst::FlowSuccess::Ok)
    }

    fn rtp_sink_chain(
        &self,
        pad: &gst::Pad,
        id: usize,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let Some(session) = state.mut_session_by_id(id) else {
            return Err(gst::FlowError::Error);
        };

        let now = Instant::now();
        let mut items_to_pre_push: smallvec::SmallVec<[HeldRecvItem; 4]> = Default::default();
        let mut held_buffers: smallvec::SmallVec<[HeldRecvBuffer; 4]> = Default::default();
        let forward = match self.handle_buffer_locked(
            pad,
            session,
            buffer,
            now,
            &mut items_to_pre_push,
            &mut held_buffers,
        )? {
            RecvRtpBuffer::SsrcCollision(ssrc) => {
                return self.handle_ssrc_collision(session, [ssrc])
            }
            RecvRtpBuffer::IsRtcp(buffer) => return Self::rtcp_sink_chain(self, id, buffer),
            RecvRtpBuffer::Drop => None,
            RecvRtpBuffer::Forward((buffer, jb)) => Some((buffer, jb)),
        };
        session
            .recv_store
            .extend(held_buffers.into_iter().map(HeldRecvItem::Buffer));

        state = self.handle_push_jitterbuffer(state, id, items_to_pre_push, now)?;
        if let Some((buffer, jb)) = forward {
            state = self.handle_push_jitterbuffer(
                state,
                id,
                [HeldRecvItem::Buffer(HeldRecvBuffer {
                    hold_id: None,
                    buffer,
                    jb,
                })],
                now,
            )?;
        }
        drop(state);

        Ok(gst::FlowSuccess::Ok)
    }

    fn rtcp_sink_chain(
        &self,
        id: usize,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let state = self.state.lock().unwrap();
        let Some(session) = state.session_by_id(id) else {
            return Err(gst::FlowError::Error);
        };

        let addr: Option<SocketAddr> =
            buffer
                .meta::<gst_net::NetAddressMeta>()
                .and_then(|net_meta| {
                    net_meta
                        .addr()
                        .dynamic_cast::<gio::InetSocketAddress>()
                        .map(|a| a.into())
                        .ok()
                });
        let mapped = buffer.map_readable().map_err(|e| {
            gst::error!(CAT, imp = self, "Failed to map input buffer {e:?}");
            gst::FlowError::Error
        })?;
        let rtcp = match rtcp_types::Compound::parse(&mapped) {
            Ok(rtcp) => rtcp,
            Err(e) => {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to parse input as valid rtcp packet: {e:?}"
                );
                return Ok(gst::FlowSuccess::Ok);
            }
        };

        let internal_session = session.internal_session.clone();
        let mut session_inner = internal_session.inner.lock().unwrap();

        let now = Instant::now();
        let ntp_now = SystemTime::now();
        let replies =
            session_inner
                .session
                .handle_rtcp_recv(rtcp, mapped.len(), addr, now, ntp_now);
        let rtp_send_sinkpad = session_inner.rtp_send_sinkpad.clone();
        drop(session_inner);
        drop(state);

        for reply in replies {
            match reply {
                RtcpRecvReply::NewSsrc(ssrc) => {
                    internal_session
                        .config
                        .emit_by_name::<()>("new-ssrc", &[&ssrc]);
                }
                RtcpRecvReply::SsrcCollision(ssrc) => {
                    if let Some(pad) = rtp_send_sinkpad.as_ref() {
                        // XXX: Another option is to have us rewrite ssrc's instead of asking
                        // upstream to do so.
                        pad.send_event(
                            gst::event::CustomUpstream::builder(
                                gst::Structure::builder("GstRTPCollision")
                                    .field("ssrc", ssrc)
                                    .build(),
                            )
                            .build(),
                        );
                    }
                }
                RtcpRecvReply::TimerReconsideration => {
                    let state = self.state.lock().unwrap();
                    let session = state.session_by_id(id).unwrap();
                    let mut session_inner = session.internal_session.inner.lock().unwrap();
                    if let Some(waker) = session_inner.rtcp_waker.take() {
                        // reconsider timers means that we wake the rtcp task to get a new timeout
                        waker.wake();
                    }
                }
                RtcpRecvReply::RequestKeyUnit { ssrcs, fir } => {
                    if let Some(ref rtp_send_sinkpad) = rtp_send_sinkpad {
                        gst::debug!(
                            CAT,
                            imp = self,
                            "Sending force-keyunit event for ssrcs {ssrcs:?} (all headers: {fir})"
                        );
                        // TODO what to do with the ssrc?
                        let event = gst_video::UpstreamForceKeyUnitEvent::builder()
                            .all_headers(fir)
                            .other_field("ssrcs", gst::Array::new(ssrcs))
                            .build();

                        let _ = rtp_send_sinkpad.push_event(event);
                    } else {
                        gst::debug!(
                            CAT,
                            imp = self,
                            "Can't send force-keyunit event because of missing sinkpad"
                        );
                    }
                }
                RtcpRecvReply::NewCName((cname, ssrc)) => {
                    let mut sync_context = self.sync_context.lock().unwrap();

                    sync_context.as_mut().unwrap().associate(ssrc, &cname);
                }
                RtcpRecvReply::NewRtpNtp((ssrc, rtp, ntp)) => {
                    let mut sync_context = self.sync_context.lock().unwrap();

                    sync_context
                        .as_mut()
                        .unwrap()
                        .add_sender_report(ssrc, rtp, ntp);
                }
                RtcpRecvReply::SsrcBye(ssrc) => internal_session
                    .config
                    .emit_by_name::<()>("bye-ssrc", &[&ssrc]),
            }
        }
        drop(mapped);

        Ok(gst::FlowSuccess::Ok)
    }

    pub fn rtp_sink_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef, id: usize) -> bool {
        gst::log!(CAT, obj = pad, "Handling query {query:?}");

        if query.is_serialized() {
            let state = self.state.lock().unwrap();
            let mut ret = true;

            if let Some(session) = state.session_by_id(id) {
                let jb_stores: Vec<Arc<Mutex<JitterBufferStore>>> = session
                    .rtp_recv_srcpads
                    .iter()
                    .filter(|r| state.pads_session_id_map.contains_key(&r.pad))
                    .map(|p| p.jitter_buffer_store.clone())
                    .collect();

                drop(state);

                let query = std::ptr::NonNull::from(query);

                // The idea here is to reproduce the default behavior of GstPad, where
                // queries will run sequentially on each internally linked source pad
                // until one succeeds.
                //
                // We however jump through hoops here in order to keep the query
                // reasonably synchronized with the data flow.
                //
                // While the GstPad behavior makes complete sense for allocation
                // queries (can't have it succeed for two downstream branches as they
                // need to modify the query), we could in the future decide to have
                // the drain query run on all relevant source pads no matter what.
                //
                // Also note that if there were no internally linked pads, GstPad's
                // behavior is to return TRUE, we do this here too.
                for jb_store in jb_stores {
                    let mut jitterbuffer_store = jb_store.lock().unwrap();

                    let jitterbuffer::QueueResult::Queued(id) =
                        jitterbuffer_store.jitterbuffer.queue_serialized_item()
                    else {
                        unreachable!()
                    };

                    let (query_tx, query_rx) = std::sync::mpsc::sync_channel(1);

                    jitterbuffer_store
                        .store
                        .insert(id, JitterBufferItem::Query(query, query_tx));

                    drop(jitterbuffer_store);

                    // Now block until the jitterbuffer has processed the query
                    match query_rx.recv() {
                        Ok(res) => {
                            ret |= res;
                            if ret {
                                break;
                            }
                        }
                        _ => {
                            // The sender was closed because of a state change
                            break;
                        }
                    }
                }
            }

            ret
        } else {
            gst::Pad::query_default(pad, Some(pad), query)
        }
    }

    // Serialized events received on our sink pads have to navigate
    // through the relevant jitterbuffers in order to remain (reasonably)
    // consistently ordered with the RTP packets once output on our source
    // pads
    fn rtp_sink_queue_serialized_event(&self, id: usize, event: gst::Event) -> bool {
        let state = self.state.lock().unwrap();
        if let Some(session) = state.session_by_id(id) {
            for srcpad in session
                .rtp_recv_srcpads
                .iter()
                .filter(|r| state.pads_session_id_map.contains_key(&r.pad))
            {
                let mut jitterbuffer_store = srcpad.jitter_buffer_store.lock().unwrap();

                let jitterbuffer::QueueResult::Queued(id) =
                    jitterbuffer_store.jitterbuffer.queue_serialized_item()
                else {
                    unreachable!()
                };

                jitterbuffer_store
                    .store
                    .insert(id, JitterBufferItem::Event(event.clone()));
                if let Some(waker) = jitterbuffer_store.waker.take() {
                    waker.wake();
                }
            }
        }

        true
    }

    fn rtp_sink_event(&self, pad: &gst::Pad, mut event: gst::Event, id: usize) -> bool {
        match event.view() {
            gst::EventView::StreamStart(stream_start) => {
                let mut state = self.state.lock().unwrap();

                if let Some(session) = state.mut_session_by_id(id) {
                    let group_id = stream_start.group_id();
                    session.rtp_recv_sink_group_id =
                        Some(group_id.unwrap_or_else(gst::GroupId::next));
                }

                true
            }
            gst::EventView::Caps(caps) => {
                let mut state = self.state.lock().unwrap();

                if let Some((pt, clock_rate)) = pt_clock_rate_from_caps(caps.caps()) {
                    if let Some(session) = state.mut_session_by_id(id) {
                        let caps = caps.caps_owned();
                        session.rtp_recv_sink_caps = Some(caps.clone());

                        let mut session_inner = session.internal_session.inner.lock().unwrap();
                        session_inner.session.set_pt_clock_rate(pt, clock_rate);
                        session_inner.add_caps(caps);
                    }
                } else {
                    gst::warning!(
                        CAT,
                        obj = pad,
                        "input caps are missing payload or clock-rate fields"
                    );
                }
                true
            }
            gst::EventView::Segment(segment) => {
                let mut state = self.state.lock().unwrap();

                if let Some(session) = state.mut_session_by_id(id) {
                    let segment = segment.segment();
                    let segment = match segment.downcast_ref::<gst::ClockTime>() {
                        Some(segment) => segment.clone(),
                        None => {
                            gst::warning!(CAT, obj = pad, "Only TIME segments are supported");

                            let segment = gst::FormattedSegment::new();
                            let seqnum = event.seqnum();

                            event = gst::event::Segment::builder(&segment)
                                .seqnum(seqnum)
                                .build();

                            segment
                        }
                    };

                    session.rtp_recv_sink_segment = Some(segment);
                    session.rtp_recv_sink_seqnum = Some(event.seqnum());
                }

                drop(state);

                self.rtp_sink_queue_serialized_event(id, event)
            }
            gst::EventView::Eos(_eos) => {
                let now = Instant::now();
                let state = self.state.lock().unwrap();
                if let Some(session) = state.session_by_id(id) {
                    let mut session = session.internal_session.inner.lock().unwrap();
                    let ssrcs = session.session.ssrcs().collect::<Vec<_>>();
                    // we can only Bye the entire session if we do not have any local send sources
                    // currently sending data
                    let mut all_remote = true;
                    let internal_ssrc = session.session.internal_ssrc();
                    for ssrc in ssrcs {
                        let Some(_local_recv) = session.session.local_receive_source_by_ssrc(ssrc)
                        else {
                            if let Some(local_send) =
                                session.session.local_send_source_by_ssrc(ssrc)
                            {
                                if local_send.state() != SourceState::Bye
                                    && Some(ssrc) != internal_ssrc
                                {
                                    all_remote = false;
                                    break;
                                }
                            }
                            continue;
                        };
                    }
                    if all_remote {
                        session.session.schedule_bye("End of stream", now);
                    }
                    drop(session);
                }
                drop(state);
                // FIXME: may need to delay sending eos under some circumstances
                self.rtp_sink_queue_serialized_event(id, event);
                true
            }
            gst::EventView::FlushStart(_fs) => {
                let state = self.state.lock().unwrap();
                let mut pause_tasks = vec![];
                if let Some(session) = state.session_by_id(id) {
                    for recv_pad in session.rtp_recv_srcpads.iter() {
                        let mut store = recv_pad.jitter_buffer_store.lock().unwrap();
                        store.jitterbuffer.set_flushing(true);
                        if let Some(waker) = store.waker.take() {
                            waker.wake();
                        }
                        pause_tasks.push(recv_pad.pad.clone());
                    }
                }
                drop(state);
                for pad in pause_tasks {
                    let _ = pad.pause_task();
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            gst::EventView::FlushStop(_fs) => {
                let mut state = self.state.lock().unwrap();
                if let Some(session) = state.mut_session_by_id(id) {
                    let pads = session
                        .rtp_recv_srcpads
                        .iter()
                        .map(|r| r.pad.clone())
                        .collect::<Vec<_>>();
                    for pad in pads {
                        // Will reset flushing to false and ensure task is woken up
                        let _ = session.start_rtp_task(&pad);
                    }
                }
                drop(state);
                self.rtp_sink_queue_serialized_event(id, event)
            }
            _ => {
                if event.is_serialized() {
                    self.rtp_sink_queue_serialized_event(id, event)
                } else {
                    gst::Pad::event_default(pad, Some(&*self.obj()), event)
                }
            }
        }
    }

    fn rtp_src_event(
        &self,
        pad: &gst::Pad,
        event: gst::Event,
        id: usize,
        pt: u8,
        ssrc: u32,
    ) -> bool {
        match event.view() {
            gst::EventView::CustomUpstream(custom) => {
                if let Ok(fku) = gst_video::UpstreamForceKeyUnitEvent::parse(custom) {
                    let all_headers = fku.all_headers;
                    let count = fku.count;

                    let state = self.state.lock().unwrap();
                    if let Some(session) = state.session_by_id(id) {
                        let now = Instant::now();
                        let mut session = session.internal_session.inner.lock().unwrap();
                        let caps = session.caps_from_pt(pt);
                        let s = caps.structure(0).unwrap();

                        let pli = s.has_field("rtcp-fb-nack-pli");
                        let fir = s.has_field("rtcp-fb-ccm-fir") && all_headers;

                        let typ = if fir {
                            KeyUnitRequestType::Fir(count)
                        } else {
                            KeyUnitRequestType::Pli
                        };

                        if pli || fir {
                            let replies = session.session.request_remote_key_unit(now, typ, ssrc);

                            for reply in replies {
                                match reply {
                                    RequestRemoteKeyUnitReply::TimerReconsideration => {
                                        if let Some(waker) = session.rtcp_waker.take() {
                                            // reconsider timers means that we wake the rtcp task to get a new timeout
                                            waker.wake();
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Don't forward
                    return true;
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RtpRecv {
    const NAME: &'static str = "GstRtpRecv";
    type Type = super::RtpRecv;
    type ParentType = gst::Element;

    fn new() -> Self {
        GstRustLogger::install();
        Self {
            settings: Default::default(),
            state: Default::default(),
            sync_context: Default::default(),
        }
    }
}

impl ObjectImpl for RtpRecv {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("rtp-id")
                    .nick("The RTP Connection ID")
                    .blurb("A connection ID shared with a rtpsend element for implementing both sending and receiving using the same RTP context")
                    .default_value("rtp-id")
                    .build(),
                glib::ParamSpecUInt::builder("latency")
                    .nick("Buffer latency in ms")
                    .blurb("Amount of ms to buffer")
                    .default_value(DEFAULT_LATENCY.mseconds() as u32)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("stats")
                    .nick("Statistics")
                    .blurb("Statistics about the session")
                    .read_only()
                    .build(),
                glib::ParamSpecEnum::builder::<sync::TimestampingMode>("timestamping-mode")
                    .nick("Timestamping Mode")
                    .blurb("Govern how to pick presentation timestamps for packets")
                    .default_value(sync::TimestampingMode::default())
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "rtp-id" => {
                let mut settings = self.settings.lock().unwrap();
                settings.rtp_id = value.get::<String>().expect("type checked upstream");
            }
            "latency" => {
                let _latency = {
                    let mut settings = self.settings.lock().unwrap();
                    settings.latency = gst::ClockTime::from_mseconds(
                        value.get::<u32>().expect("type checked upstream").into(),
                    );
                    settings.latency
                };

                let _ = self
                    .obj()
                    .post_message(gst::message::Latency::builder().src(&*self.obj()).build());
            }
            "timestamping-mode" => {
                let mut settings = self.settings.lock().unwrap();
                settings.timestamping_mode = value
                    .get::<sync::TimestampingMode>()
                    .expect("Type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "rtp-id" => {
                let settings = self.settings.lock().unwrap();
                settings.rtp_id.to_value()
            }
            "latency" => {
                let settings = self.settings.lock().unwrap();
                (settings.latency.mseconds() as u32).to_value()
            }
            "stats" => {
                let state = self.state.lock().unwrap();
                state.stats().to_value()
            }
            "timestamping-mode" => {
                let settings = self.settings.lock().unwrap();
                settings.timestamping_mode.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
            vec![glib::subclass::Signal::builder("get-session")
                .param_types([u32::static_type()])
                .return_type::<crate::rtpbin2::config::Rtp2Session>()
                .action()
                .class_handler(|args| {
                    let element = args[0].get::<super::RtpRecv>().expect("signal arg");
                    let id = args[1].get::<u32>().expect("signal arg");
                    let bin = element.imp();
                    let state = bin.state.lock().unwrap();
                    state
                        .session_by_id(id as usize)
                        .map(|sess| sess.internal_session.config.to_value())
                })
                .build()]
        });

        SIGNALS.as_ref()
    }
}

impl GstObjectImpl for RtpRecv {}

impl ElementImpl for RtpRecv {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP Session receiver",
                "Network/RTP/Filter",
                "RTP sessions management (receiver)",
                "Matthew Waters <matthew@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let rtp_caps = gst::Caps::builder_full()
                .structure(gst::Structure::builder("application/x-rtp").build())
                .build();
            let rtcp_caps = gst::Caps::builder_full()
                .structure(gst::Structure::builder("application/x-rtcp").build())
                .build();

            vec![
                gst::PadTemplate::new(
                    "rtp_sink_%u",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Request,
                    &rtp_caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "rtcp_sink_%u",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Request,
                    &rtcp_caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "rtp_src_%u_%u_%u",
                    gst::PadDirection::Src,
                    gst::PadPresence::Sometimes,
                    &rtp_caps,
                )
                .unwrap(),
            ]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        _caps: Option<&gst::Caps>, // XXX: do something with caps?
    ) -> Option<gst::Pad> {
        let settings = self.settings.lock().unwrap().clone();
        let rtp_id = settings.rtp_id.clone();
        let mut state = self.state.lock().unwrap();
        let max_session_id = state.max_session_id;

        // parse the possibly provided name into a session id or use the default
        let sess_parse = move |name: Option<&str>, prefix, default_id| -> Option<usize> {
            if let Some(name) = name {
                name.strip_prefix(prefix).and_then(|suffix| {
                    if suffix.starts_with("%u") {
                        Some(default_id)
                    } else {
                        suffix.parse::<usize>().ok()
                    }
                })
            } else {
                Some(default_id)
            }
        };

        match templ.name_template() {
            "rtp_sink_%u" => sess_parse(name, "rtp_sink_", max_session_id).and_then(|id| {
                let new_pad = move |session: &mut RecvSession| -> Option<(
                    gst::Pad,
                    Option<gst::Pad>,
                    usize,
                    Vec<gst::Event>,
                )> {
                    let sinkpad = gst::Pad::builder_from_template(templ)
                        .chain_list_function(move |pad, parent, buffer_list| {
                            RtpRecv::catch_panic_pad_function(
                                parent,
                                || Err(gst::FlowError::Error),
                                |this| this.rtp_sink_chain_list(pad, id, buffer_list),
                            )
                        })
                        .chain_function(move |pad, parent, buffer| {
                            RtpRecv::catch_panic_pad_function(
                                parent,
                                || Err(gst::FlowError::Error),
                                |this| this.rtp_sink_chain(pad, id, buffer),
                            )
                        })
                        .iterate_internal_links_function(|pad, parent| {
                            RtpRecv::catch_panic_pad_function(
                                parent,
                                || gst::Iterator::from_vec(vec![]),
                                |this| this.iterate_internal_links(pad),
                            )
                        })
                        .event_function(move |pad, parent, event| {
                            RtpRecv::catch_panic_pad_function(
                                parent,
                                || false,
                                |this| this.rtp_sink_event(pad, event, id),
                            )
                        })
                        .query_function(move |pad, parent, query| {
                            RtpRecv::catch_panic_pad_function(
                                parent,
                                || false,
                                |this| this.rtp_sink_query(pad, query, id),
                            )
                        })
                        .name(format!("rtp_sink_{}", id))
                        .build();
                    session.rtp_recv_sinkpad = Some(sinkpad.clone());
                    Some((sinkpad, None, id, vec![]))
                };

                let session = state.mut_session_by_id(id);
                if let Some(session) = session {
                    if session.rtp_recv_sinkpad.is_some() {
                        None
                    } else {
                        new_pad(session)
                    }
                } else {
                    let shared_state = state
                        .shared_state
                        .get_or_insert_with(|| SharedRtpState::recv_get_or_init(rtp_id));
                    let mut session = RecvSession::new(shared_state, id);
                    let ret = new_pad(&mut session);
                    state.sessions.push(session);
                    ret
                }
            }),
            "rtcp_sink_%u" => sess_parse(name, "rtcp_sink_", max_session_id).and_then(|id| {
                let new_pad = move |session: &mut RecvSession| -> Option<(
                    gst::Pad,
                    Option<gst::Pad>,
                    usize,
                    Vec<gst::Event>,
                )> {
                    let sinkpad = gst::Pad::builder_from_template(templ)
                        .chain_function(move |_pad, parent, buffer| {
                            RtpRecv::catch_panic_pad_function(
                                parent,
                                || Err(gst::FlowError::Error),
                                |this| this.rtcp_sink_chain(id, buffer),
                            )
                        })
                        .iterate_internal_links_function(|pad, parent| {
                            RtpRecv::catch_panic_pad_function(
                                parent,
                                || gst::Iterator::from_vec(vec![]),
                                |this| this.iterate_internal_links(pad),
                            )
                        })
                        .name(format!("rtcp_sink_{}", id))
                        .build();
                    session.rtcp_recv_sinkpad = Some(sinkpad.clone());
                    Some((sinkpad, None, id, vec![]))
                };

                let session = state.mut_session_by_id(id);
                if let Some(session) = session {
                    if session.rtcp_recv_sinkpad.is_some() {
                        None
                    } else {
                        new_pad(session)
                    }
                } else {
                    let shared_state = state
                        .shared_state
                        .get_or_insert_with(|| SharedRtpState::recv_get_or_init(rtp_id));
                    let mut session = RecvSession::new(shared_state, id);
                    let ret = new_pad(&mut session);
                    state.sessions.push(session);
                    ret
                }
            }),
            _ => None,
        }
        .map(|(pad, otherpad, id, sticky_events)| {
            state.max_session_id = (id + 1).max(state.max_session_id);
            state.pads_session_id_map.insert(pad.clone(), id);
            if let Some(ref pad) = otherpad {
                state.pads_session_id_map.insert(pad.clone(), id);
            }

            drop(state);

            pad.set_active(true).unwrap();
            for event in sticky_events {
                let _ = pad.store_sticky_event(&event);
            }
            self.obj().add_pad(&pad).unwrap();

            if let Some(pad) = otherpad {
                pad.set_active(true).unwrap();
                self.obj().add_pad(&pad).unwrap();
            }

            pad
        })
    }

    fn release_pad(&self, pad: &gst::Pad) {
        let mut state = self.state.lock().unwrap();
        let mut removed_pads = vec![];
        let mut removed_session_ids = vec![];
        let mut removed_srcpads_session_ids = vec![];
        if let Some(&id) = state.pads_session_id_map.get(pad) {
            removed_pads.push(pad.clone());
            if let Some(session) = state.mut_session_by_id(id) {
                if Some(pad) == session.rtp_recv_sinkpad.as_ref() {
                    session.rtp_recv_sinkpad = None;
                    removed_pads.extend(session.rtp_recv_srcpads.iter().map(|r| r.pad.clone()));
                    session.recv_flow_combiner.lock().unwrap().clear();
                    removed_srcpads_session_ids.push(id);
                    session.recv_store.clear();
                }

                if Some(pad) == session.rtcp_recv_sinkpad.as_ref() {
                    session.rtcp_recv_sinkpad = None;
                }

                if session.rtp_recv_sinkpad.is_none() && session.rtcp_recv_sinkpad.is_none() {
                    removed_session_ids.push(session.internal_session.id);
                }
            }
        }

        for pad in removed_pads.iter() {
            state.pads_session_id_map.remove(pad);
        }
        drop(state);

        for pad in removed_pads.iter() {
            let _ = pad.set_active(false);
            // Pad might not have been added yet if it's a RTP recv srcpad
            if pad.has_as_parent(&*self.obj()) {
                let _ = self.obj().remove_pad(pad);
            }
        }

        {
            let mut state = self.state.lock().unwrap();
            for id in removed_srcpads_session_ids {
                if let Some(session) = state.mut_session_by_id(id) {
                    session.rtp_recv_srcpads.clear();
                }
            }
            for id in removed_session_ids {
                if let Some(session) = state.mut_session_by_id(id) {
                    if session.rtp_recv_sinkpad.is_none() && session.rtcp_recv_sinkpad.is_none() {
                        let id = session.internal_session.id;
                        state.sessions.retain(|s| s.internal_session.id != id);
                    }
                }
            }
        }

        self.parent_release_pad(pad)
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                let settings = self.settings.lock().unwrap();
                let mut state = self.state.lock().unwrap();
                let rtp_id = settings.rtp_id.clone();
                let empty_sessions = state.sessions.is_empty();
                match state.shared_state.as_mut() {
                    Some(shared) => {
                        if !empty_sessions && shared.name() != rtp_id {
                            let other_name = shared.name().to_owned();
                            drop(state);
                            self.post_error_message(gst::error_msg!(gst::LibraryError::Settings, ["rtp-id {rtp_id} does not match the currently set value {other_name}"]));
                            return Err(gst::StateChangeError);
                        }
                    }
                    None => {
                        state.shared_state = Some(SharedRtpState::send_get_or_init(rtp_id.clone()));
                    }
                }
            }
            gst::StateChange::ReadyToPaused => {
                let settings = self.settings.lock().unwrap();
                let mut sync_context = self.sync_context.lock().unwrap();

                *sync_context = Some(sync::Context::new(settings.timestamping_mode));
            }
            _ => (),
        }

        let mut success = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::ReadyToPaused | gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                let mut removed_pads = vec![];
                for session in &mut state.sessions {
                    removed_pads.extend(session.rtp_recv_srcpads.iter().map(|r| r.pad.clone()));

                    session.recv_flow_combiner.lock().unwrap().clear();
                    session.rtp_recv_srcpads.clear();
                    session.recv_store.clear();

                    session.rtp_recv_sink_caps = None;
                    session.rtp_recv_sink_segment = None;
                    session.rtp_recv_sink_seqnum = None;
                    session.rtp_recv_sink_group_id = None;
                }
                let mut sync_context = self.sync_context.lock().unwrap();
                *sync_context = None;
                drop(sync_context);
                drop(state);

                for pad in removed_pads.iter() {
                    let _ = pad.set_active(false);
                    // Pad might not have been added yet if it's a RTP recv srcpad
                    if pad.has_as_parent(&*self.obj()) {
                        let _ = self.obj().remove_pad(pad);
                    }
                }

                let mut state = self.state.lock().unwrap();
                for pad in removed_pads {
                    state.pads_session_id_map.remove(&pad);
                }
                drop(state);
            }
            _ => (),
        }

        Ok(success)
    }
}

impl Drop for RtpRecv {
    fn drop(&mut self) {
        if let Some(ref shared_state) = self.state.lock().unwrap().shared_state {
            shared_state.unmark_recv_outstanding();
        }
    }
}
