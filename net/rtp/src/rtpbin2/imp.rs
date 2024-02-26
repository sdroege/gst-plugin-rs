// SPDX-License-Identifier: MPL-2.0

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant, SystemTime};

use futures::future::{AbortHandle, Abortable};
use futures::StreamExt;
use gst::{glib, prelude::*, subclass::prelude::*};
use once_cell::sync::Lazy;

use super::jitterbuffer::{self, JitterBuffer};
use super::session::{
    KeyUnitRequestType, RecvReply, RequestRemoteKeyUnitReply, RtcpRecvReply, RtcpSendReply,
    RtpProfile, SendReply, Session, RTCP_MIN_REPORT_INTERVAL,
};
use super::source::{ReceivedRb, SourceState};
use super::sync;

use crate::rtpbin2::config::RtpBin2Session;
use crate::rtpbin2::RUNTIME;

const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::from_mseconds(200);
const DEFAULT_MIN_RTCP_INTERVAL: Duration = RTCP_MIN_REPORT_INTERVAL;
const DEFAULT_REDUCED_SIZE_RTCP: bool = false;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rtpbin2",
        gst::DebugColorFlags::empty(),
        Some("RTP management bin"),
    )
});

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstRtpBin2Profile")]
enum Profile {
    #[default]
    #[enum_value(name = "AVP profile as specified in RFC 3550", nick = "avp")]
    Avp,
    #[enum_value(name = "AVPF profile as specified in RFC 4585", nick = "avpf")]
    Avpf,
}

impl From<RtpProfile> for Profile {
    fn from(value: RtpProfile) -> Self {
        match value {
            RtpProfile::Avp => Self::Avp,
            RtpProfile::Avpf => Self::Avpf,
        }
    }
}

impl From<Profile> for RtpProfile {
    fn from(value: Profile) -> Self {
        match value {
            Profile::Avp => Self::Avp,
            Profile::Avpf => Self::Avpf,
        }
    }
}

#[derive(Debug, Clone)]
struct Settings {
    latency: gst::ClockTime,
    min_rtcp_interval: Duration,
    profile: Profile,
    reduced_size_rtcp: bool,
    timestamping_mode: sync::TimestampingMode,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            latency: DEFAULT_LATENCY,
            min_rtcp_interval: DEFAULT_MIN_RTCP_INTERVAL,
            profile: Profile::default(),
            reduced_size_rtcp: DEFAULT_REDUCED_SIZE_RTCP,
            timestamping_mode: sync::TimestampingMode::default(),
        }
    }
}

#[derive(Debug)]
#[must_use = "futures/streams/sinks do nothing unless you `.await` or poll them"]
struct RtcpSendStream {
    state: Arc<Mutex<State>>,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

impl RtcpSendStream {
    fn new(state: Arc<Mutex<State>>) -> Self {
        Self {
            state,
            sleep: Box::pin(tokio::time::sleep(Duration::from_secs(1))),
        }
    }
}

impl futures::stream::Stream for RtcpSendStream {
    type Item = (usize, RtcpSendReply);

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let ntp_now = SystemTime::now();
        let mut lowest_wait = None;
        for session in state.sessions.iter_mut() {
            let mut session = session.inner.lock().unwrap();
            if let Some(reply) = session.session.poll_rtcp_send(now, ntp_now) {
                return Poll::Ready(Some((session.id, reply)));
            }
            if let Some(wait) = session.session.poll_rtcp_send_timeout(now) {
                if lowest_wait.map_or(true, |lowest_wait| wait < lowest_wait) {
                    lowest_wait = Some(wait);
                }
            }
        }
        state.rtcp_waker = Some(cx.waker().clone());
        drop(state);

        // default to the minimum initial rtcp delay so we don't busy loop if there are no sessions or no
        // timeouts available
        let lowest_wait =
            lowest_wait.unwrap_or(now + crate::rtpbin2::session::RTCP_MIN_REPORT_INTERVAL / 2);
        let this = self.get_mut();
        this.sleep.as_mut().reset(lowest_wait.into());
        if !std::future::Future::poll(this.sleep.as_mut(), cx).is_pending() {
            // wake us again if the delay is not pending for another go at finding the next timeout
            // value
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }
}

#[derive(Debug)]
#[must_use = "futures/streams/sinks do nothing unless you `.await` or poll them"]
struct JitterBufferStream {
    store: Arc<Mutex<JitterBufferStore>>,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

impl JitterBufferStream {
    fn new(store: Arc<Mutex<JitterBufferStore>>) -> Self {
        Self {
            store,
            sleep: Box::pin(tokio::time::sleep(Duration::from_secs(1))),
        }
    }
}

impl futures::stream::Stream for JitterBufferStream {
    type Item = JitterBufferItem;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let now = Instant::now();
        let mut lowest_wait = None;

        let mut jitterbuffer_store = self.store.lock().unwrap();
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
                cx.waker().wake_by_ref();
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
                return Poll::Ready(Some(item));
            }
            jitterbuffer::PollResult::Timeout(timeout) => {
                if lowest_wait.map_or(true, |lowest_wait| timeout < lowest_wait) {
                    lowest_wait = Some(timeout);
                }
            }
            // Will be woken up when necessary
            jitterbuffer::PollResult::Empty => (),
        }

        jitterbuffer_store.waker = Some(cx.waker().clone());
        drop(jitterbuffer_store);

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
    fn activate(&mut self, session: &BinSession) {
        let session_inner = session.inner.lock().unwrap();
        let seqnum = session_inner.rtp_recv_sink_seqnum.unwrap();
        let stream_id = format!("{}/{}", self.pt, self.ssrc);
        let stream_start = gst::event::StreamStart::builder(&stream_id)
            .group_id(session_inner.rtp_recv_sink_group_id.unwrap())
            .seqnum(seqnum)
            .build();

        let caps = session_inner.caps_from_pt(self.pt);
        let caps = gst::event::Caps::builder(&caps).seqnum(seqnum).build();

        let segment =
            gst::event::Segment::builder(session_inner.rtp_recv_sink_segment.as_ref().unwrap())
                .seqnum(seqnum)
                .build();

        drop(session_inner);

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
    pad: RtpRecvSrcPad,
    new_pad: bool,
}

#[derive(Debug, Clone)]
pub struct BinSession {
    id: usize,
    inner: Arc<Mutex<BinSessionInner>>,
    config: RtpBin2Session,
}

impl BinSession {
    fn new(id: usize, settings: &Settings) -> Self {
        let mut inner = BinSessionInner::new(id);
        inner
            .session
            .set_min_rtcp_interval(settings.min_rtcp_interval);
        inner.session.set_profile(settings.profile.into());
        inner
            .session
            .set_reduced_size_rtcp(settings.reduced_size_rtcp);
        let inner = Arc::new(Mutex::new(inner));
        let weak_inner = Arc::downgrade(&inner);
        Self {
            id,
            inner,
            config: RtpBin2Session::new(weak_inner),
        }
    }
}

#[derive(Debug)]
pub(crate) struct BinSessionInner {
    id: usize,

    session: Session,

    // State for received RTP streams
    rtp_recv_sinkpad: Option<gst::Pad>,
    rtp_recv_sink_group_id: Option<gst::GroupId>,
    rtp_recv_sink_caps: Option<gst::Caps>,
    rtp_recv_sink_segment: Option<gst::FormattedSegment<gst::ClockTime>>,
    rtp_recv_sink_seqnum: Option<gst::Seqnum>,

    pt_map: HashMap<u8, gst::Caps>,
    recv_store: Vec<HeldRecvBuffer>,

    rtp_recv_srcpads: Vec<RtpRecvSrcPad>,
    recv_flow_combiner: Arc<Mutex<gst_base::UniqueFlowCombiner>>,

    // State for sending RTP streams
    rtp_send_sinkpad: Option<gst::Pad>,
    rtp_send_srcpad: Option<gst::Pad>,

    rtcp_recv_sinkpad: Option<gst::Pad>,
    rtcp_send_srcpad: Option<gst::Pad>,
}

impl BinSessionInner {
    fn new(id: usize) -> Self {
        Self {
            id,

            session: Session::new(),

            rtp_recv_sinkpad: None,
            rtp_recv_sink_group_id: None,
            rtp_recv_sink_caps: None,
            rtp_recv_sink_segment: None,
            rtp_recv_sink_seqnum: None,

            pt_map: HashMap::default(),
            recv_store: vec![],

            rtp_recv_srcpads: vec![],
            recv_flow_combiner: Arc::new(Mutex::new(gst_base::UniqueFlowCombiner::new())),

            rtp_send_sinkpad: None,
            rtp_send_srcpad: None,

            rtcp_recv_sinkpad: None,
            rtcp_send_srcpad: None,
        }
    }

    pub fn clear_pt_map(&mut self) {
        self.pt_map.clear();
    }

    pub fn add_caps(&mut self, caps: gst::Caps) {
        let Some((pt, clock_rate)) = pt_clock_rate_from_caps(&caps) else {
            return;
        };
        let caps_clone = caps.clone();
        self.pt_map
            .entry(pt)
            .and_modify(move |entry| *entry = caps)
            .or_insert_with(move || caps_clone);
        self.session.set_pt_clock_rate(pt, clock_rate);
    }

    fn caps_from_pt(&self, pt: u8) -> gst::Caps {
        self.pt_map.get(&pt).cloned().unwrap_or(
            gst::Caps::builder("application/x-rtp")
                .field("payload", pt as i32)
                .build(),
        )
    }

    pub fn pt_map(&self) -> impl Iterator<Item = (u8, &gst::Caps)> + '_ {
        self.pt_map.iter().map(|(&k, v)| (k, v))
    }

    pub fn stats(&self) -> gst::Structure {
        let mut session_stats = gst::Structure::builder("application/x-rtpbin2-session-stats")
            .field("id", self.id as u64);
        for ssrc in self.session.ssrcs() {
            if let Some(ls) = self.session.local_send_source_by_ssrc(ssrc) {
                let mut source_stats =
                    gst::Structure::builder("application/x-rtpbin2-source-stats")
                        .field("ssrc", ls.ssrc())
                        .field("sender", true)
                        .field("local", true)
                        .field("packets-sent", ls.packet_count())
                        .field("octets-sent", ls.octet_count())
                        .field("bitrate", ls.bitrate() as u64);
                if let Some(pt) = ls.payload_type() {
                    if let Some(clock_rate) = self.session.clock_rate_from_pt(pt) {
                        source_stats = source_stats.field("clock-rate", clock_rate);
                    }
                }
                if let Some(sr) = ls.last_sent_sr() {
                    source_stats = source_stats
                        .field("sr-ntptime", sr.ntp_timestamp().as_u64())
                        .field("sr-rtptime", sr.rtp_timestamp())
                        .field("sr-octet-count", sr.octet_count())
                        .field("sr-packet-count", sr.packet_count());
                }
                let rbs = gst::List::new(ls.received_report_blocks().map(
                    |(sender_ssrc, ReceivedRb { rb, .. })| {
                        gst::Structure::builder("application/x-rtcp-report-block")
                            .field("sender-ssrc", sender_ssrc)
                            .field("rb-fraction-lost", rb.fraction_lost())
                            .field("rb-packets-lost", rb.cumulative_lost())
                            .field("rb-extended_sequence_number", rb.extended_sequence_number())
                            .field("rb-jitter", rb.jitter())
                            .field("rb-last-sr-ntp-time", rb.last_sr_ntp_time())
                            .field("rb-delay_since_last-sr-ntp-time", rb.delay_since_last_sr())
                            .build()
                    },
                ));
                match rbs.len() {
                    0 => (),
                    1 => {
                        source_stats =
                            source_stats.field("report-blocks", rbs.first().unwrap().clone());
                    }
                    _ => {
                        source_stats = source_stats.field("report-blocks", rbs);
                    }
                }

                // TODO: add jitter, packets-lost
                session_stats = session_stats.field(ls.ssrc().to_string(), source_stats.build());
            } else if let Some(lr) = self.session.local_receive_source_by_ssrc(ssrc) {
                let mut source_stats =
                    gst::Structure::builder("application/x-rtpbin2-source-stats")
                        .field("ssrc", lr.ssrc())
                        .field("sender", false)
                        .field("local", true);
                if let Some(pt) = lr.payload_type() {
                    if let Some(clock_rate) = self.session.clock_rate_from_pt(pt) {
                        source_stats = source_stats.field("clock-rate", clock_rate);
                    }
                }
                // TODO: add rb stats
                session_stats = session_stats.field(lr.ssrc().to_string(), source_stats.build());
            } else if let Some(rs) = self.session.remote_send_source_by_ssrc(ssrc) {
                let mut source_stats =
                    gst::Structure::builder("application/x-rtpbin2-source-stats")
                        .field("ssrc", rs.ssrc())
                        .field("sender", true)
                        .field("local", false)
                        .field("octets-received", rs.octet_count())
                        .field("packets-received", rs.packet_count())
                        .field("bitrate", rs.bitrate() as u64)
                        .field("jitter", rs.jitter())
                        .field("packets-lost", rs.packets_lost());
                if let Some(pt) = rs.payload_type() {
                    if let Some(clock_rate) = self.session.clock_rate_from_pt(pt) {
                        source_stats = source_stats.field("clock-rate", clock_rate);
                    }
                }
                if let Some(rtp_from) = rs.rtp_from() {
                    source_stats = source_stats.field("rtp-from", rtp_from.to_string());
                }
                if let Some(rtcp_from) = rs.rtcp_from() {
                    source_stats = source_stats.field("rtcp-from", rtcp_from.to_string());
                }
                if let Some(sr) = rs.last_received_sr() {
                    source_stats = source_stats
                        .field("sr-ntptime", sr.ntp_timestamp().as_u64())
                        .field("sr-rtptime", sr.rtp_timestamp())
                        .field("sr-octet-count", sr.octet_count())
                        .field("sr-packet-count", sr.packet_count());
                }
                if let Some(rb) = rs.last_sent_rb() {
                    source_stats = source_stats
                        .field("sent-rb-fraction-lost", rb.fraction_lost())
                        .field("sent-rb-packets-lost", rb.cumulative_lost())
                        .field(
                            "sent-rb-extended-sequence-number",
                            rb.extended_sequence_number(),
                        )
                        .field("sent-rb-jitter", rb.jitter())
                        .field("sent-rb-last-sr-ntp-time", rb.last_sr_ntp_time())
                        .field(
                            "sent-rb-delay-since-last-sr-ntp-time",
                            rb.delay_since_last_sr(),
                        );
                }
                let rbs = gst::List::new(rs.received_report_blocks().map(
                    |(sender_ssrc, ReceivedRb { rb, .. })| {
                        gst::Structure::builder("application/x-rtcp-report-block")
                            .field("sender-ssrc", sender_ssrc)
                            .field("rb-fraction-lost", rb.fraction_lost())
                            .field("rb-packets-lost", rb.cumulative_lost())
                            .field("rb-extended_sequence_number", rb.extended_sequence_number())
                            .field("rb-jitter", rb.jitter())
                            .field("rb-last-sr-ntp-time", rb.last_sr_ntp_time())
                            .field("rb-delay_since_last-sr-ntp-time", rb.delay_since_last_sr())
                            .build()
                    },
                ));
                match rbs.len() {
                    0 => (),
                    1 => {
                        source_stats =
                            source_stats.field("report-blocks", rbs.first().unwrap().clone());
                    }
                    _ => {
                        source_stats = source_stats.field("report-blocks", rbs);
                    }
                }
                session_stats = session_stats.field(rs.ssrc().to_string(), source_stats.build());
            } else if let Some(rr) = self.session.remote_receive_source_by_ssrc(ssrc) {
                let source_stats = gst::Structure::builder("application/x-rtpbin2-source-stats")
                    .field("ssrc", rr.ssrc())
                    .field("sender", false)
                    .field("local", false)
                    .build();
                session_stats = session_stats.field(rr.ssrc().to_string(), source_stats);
            }
        }

        let jb_stats = gst::List::new(self.rtp_recv_srcpads.iter().map(|pad| {
            let mut jb_stats = pad.jitter_buffer_store.lock().unwrap().jitterbuffer.stats();
            jb_stats.set_value("ssrc", (pad.ssrc as i32).to_send_value());
            jb_stats.set_value("pt", (pad.pt as i32).to_send_value());
            jb_stats
        }));

        session_stats = session_stats.field("jitterbuffer-stats", jb_stats);

        session_stats.build()
    }

    fn start_rtp_recv_task(&mut self, pad: &gst::Pad) -> Result<(), glib::BoolError> {
        gst::debug!(CAT, obj: pad, "Starting rtp recv src task");

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
                        JitterBufferItem::Packet(buffer) => {
                            let flow = pad.push(buffer);
                            gst::trace!(CAT, obj: pad, "Pushed buffer, flow ret {:?}", flow);
                            let mut recv_flow_combiner = recv_flow_combiner.lock().unwrap();
                            let _combined_flow = recv_flow_combiner.update_pad_flow(&pad, flow);
                            // TODO: store flow, return only on session pads?
                        }
                        JitterBufferItem::Event(event) => {
                            let res = pad.push_event(event);
                            gst::trace!(CAT, obj: pad, "Pushed serialized event, result: {}", res);
                        }
                        JitterBufferItem::Query(mut query, tx) => {
                            // This is safe because the thread holding the original reference is waiting
                            // for us exclusively
                            let res = pad.query(unsafe { query.as_mut() });
                            let _ = tx.send(res);
                        }
                    }
                }
            })
        })?;

        gst::debug!(CAT, obj: pad, "Task started");

        Ok(())
    }

    fn stop_rtp_recv_task(&mut self, pad: &gst::Pad) -> Result<(), glib::BoolError> {
        gst::debug!(CAT, obj: pad, "Stopping rtp recv src task");
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

    fn get_or_create_rtp_recv_src(
        &mut self,
        rtpbin: &RtpBin2,
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
            let src_templ = rtpbin.obj().pad_template("rtp_recv_src_%u_%u_%u").unwrap();
            let id = self.id;
            let srcpad = gst::Pad::builder_from_template(&src_templ)
                .iterate_internal_links_function(|pad, parent| {
                    RtpBin2::catch_panic_pad_function(
                        parent,
                        || gst::Iterator::from_vec(vec![]),
                        |this| this.iterate_internal_links(pad),
                    )
                })
                .query_function(|pad, parent, query| {
                    RtpBin2::catch_panic_pad_function(
                        parent,
                        || false,
                        |this| this.src_query(pad, query),
                    )
                })
                .event_function(move |pad, parent, event| {
                    RtpBin2::catch_panic_pad_function(
                        parent,
                        || false,
                        |this| this.rtp_recv_src_event(pad, event, id, pt, ssrc),
                    )
                })
                .activatemode_function({
                    let this = rtpbin.downgrade();
                    move |pad, _parent, mode, active| {
                        let Some(this) = this.upgrade() else {
                            return Err(gst::LoggableError::new(
                                *CAT,
                                glib::bool_error!("rtpbin does not exist anymore"),
                            ));
                        };
                        this.rtp_recv_src_activatemode(pad, mode, active, id)
                    }
                })
                .name(format!("rtp_recv_src_{}_{}_{}", self.id, pt, ssrc))
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
    sessions: Vec<BinSession>,
    rtcp_waker: Option<Waker>,
    max_session_id: usize,
    pads_session_id_map: HashMap<gst::Pad, usize>,
    sync_context: Option<sync::Context>,
}

impl State {
    fn session_by_id(&self, id: usize) -> Option<&BinSession> {
        self.sessions.iter().find(|session| session.id == id)
    }

    fn stats(&self) -> gst::Structure {
        let mut ret = gst::Structure::builder("application/x-rtpbin2-stats");
        for session in self.sessions.iter() {
            let sess_id = session.id;
            let session = session.inner.lock().unwrap();

            ret = ret.field(sess_id.to_string(), session.stats());
        }
        ret.build()
    }
}

pub struct RtpBin2 {
    settings: Mutex<Settings>,
    state: Arc<Mutex<State>>,
    rtcp_task: Mutex<Option<RtcpTask>>,
}

struct RtcpTask {
    abort_handle: AbortHandle,
}

impl RtpBin2 {
    fn rtp_recv_src_activatemode(
        &self,
        pad: &gst::Pad,
        mode: gst::PadMode,
        active: bool,
        id: usize,
    ) -> Result<(), gst::LoggableError> {
        if let gst::PadMode::Push = mode {
            let state = self.state.lock().unwrap();
            let Some(session) = state.session_by_id(id) else {
                if active {
                    return Err(gst::LoggableError::new(
                        *CAT,
                        glib::bool_error!("Can't activate pad of unknown session {id}"),
                    ));
                } else {
                    return Ok(());
                }
            };

            let mut session = session.inner.lock().unwrap();
            if active {
                session.start_rtp_recv_task(pad)?;
            } else {
                session.stop_rtp_recv_task(pad)?;
                drop(session);

                gst::debug!(CAT, obj: pad, "Stopping task");

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

    fn start_rtcp_task(&self) {
        let mut rtcp_task = self.rtcp_task.lock().unwrap();

        if rtcp_task.is_some() {
            return;
        }

        // run the runtime from another task to prevent the "start a runtime from within a runtime" panic
        // when the plugin is statically linked.
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        let state = self.state.clone();
        RUNTIME.spawn(async move {
            let future = Abortable::new(Self::rtcp_task(state), abort_registration);
            future.await
        });

        rtcp_task.replace(RtcpTask { abort_handle });
    }

    async fn rtcp_task(state: Arc<Mutex<State>>) {
        let mut stream = RtcpSendStream::new(state.clone());
        while let Some((session_id, reply)) = stream.next().await {
            let state = state.lock().unwrap();
            let Some(session) = state.session_by_id(session_id) else {
                continue;
            };
            match reply {
                RtcpSendReply::Data(data) => {
                    let Some(rtcp_srcpad) = session.inner.lock().unwrap().rtcp_send_srcpad.clone()
                    else {
                        continue;
                    };
                    RUNTIME.spawn_blocking(move || {
                        let buffer = gst::Buffer::from_mut_slice(data);
                        if let Err(e) = rtcp_srcpad.push(buffer) {
                            gst::warning!(CAT, obj: rtcp_srcpad, "Failed to send rtcp data: flow return {e:?}");
                        }
                    });
                }
                RtcpSendReply::SsrcBye(ssrc) => {
                    session.config.emit_by_name::<()>("bye-ssrc", &[&ssrc])
                }
            }
        }
    }

    fn stop_rtcp_task(&self) {
        let mut rtcp_task = self.rtcp_task.lock().unwrap();

        if let Some(rtcp) = rtcp_task.take() {
            rtcp.abort_handle.abort();
        }
    }

    pub fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj: pad, "Handling query {query:?}");

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

                gst::info!(CAT, obj: pad, "Handled latency query, our latency {our_latency}, minimum latency: {min}");
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
                let session = session.inner.lock().unwrap();
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
                if let Some(ref sinkpad) = session.rtp_send_sinkpad {
                    if let Some(ref srcpad) = session.rtp_send_srcpad {
                        if sinkpad == pad {
                            return gst::Iterator::from_vec(vec![srcpad.clone()]);
                        } else if srcpad == pad {
                            return gst::Iterator::from_vec(vec![sinkpad.clone()]);
                        }
                    }
                }
                // nothing to do for rtcp pads
            }
        }
        gst::Iterator::from_vec(vec![])
    }

    fn rtp_recv_sink_chain(
        &self,
        pad: &gst::Pad,
        id: usize,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let Some(session) = state.session_by_id(id) else {
            return Err(gst::FlowError::Error);
        };

        // TODO: this is different from the old C implementation, where we
        // simply used the RTP timestamps as they were instead of doing any
        // sort of skew calculations.
        //
        // Check if this makes sense or if this leads to issue with eg interleaved
        // TCP.
        let arrival_time = match buffer.dts() {
            Some(dts) => {
                let session_inner = session.inner.lock().unwrap();
                let segment = session_inner.rtp_recv_sink_segment.as_ref().unwrap();
                // TODO: use running_time_full if we care to support that
                match segment.to_running_time(dts) {
                    Some(time) => time,
                    None => {
                        gst::error!(CAT, obj: pad, "out of segment DTS are not supported");
                        return Err(gst::FlowError::Error);
                    }
                }
            }
            None => match self.obj().current_running_time() {
                Some(time) => time,
                None => {
                    gst::error!(CAT, obj: pad, "Failed to get current time");
                    return Err(gst::FlowError::Error);
                }
            },
        };

        gst::trace!(CAT, obj: pad, "using arrival time {}", arrival_time);

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
            gst::error!(CAT, imp: self, "Failed to map input buffer {e:?}");
            gst::FlowError::Error
        })?;
        let rtp = match rtp_types::RtpPacket::parse(&mapped) {
            Ok(rtp) => rtp,
            Err(e) => {
                // If this is a valid RTCP packet then it was muxed with the RTP stream and can be
                // handled just fine.
                if rtcp_types::Compound::parse(&mapped).map_or(false, |mut rtcp| {
                    rtcp.next().map_or(false, |rtcp| rtcp.is_ok())
                }) {
                    drop(mapped);
                    return Self::rtcp_recv_sink_chain(self, id, buffer);
                }

                gst::error!(CAT, imp: self, "Failed to parse input as valid rtp packet: {e:?}");
                return Ok(gst::FlowSuccess::Ok);
            }
        };

        let session = session.clone();

        let mut session_inner = session.inner.lock().unwrap();

        let current_caps = session_inner.rtp_recv_sink_caps.clone();
        if let std::collections::hash_map::Entry::Vacant(e) =
            session_inner.pt_map.entry(rtp.payload_type())
        {
            if let Some(mut caps) = current_caps.filter(|caps| clock_rate_from_caps(caps).is_some())
            {
                state
                    .sync_context
                    .as_mut()
                    .unwrap()
                    .set_clock_rate(rtp.ssrc(), clock_rate_from_caps(&caps).unwrap());
                {
                    // Ensure the caps we send out hold a payload field
                    let caps = caps.make_mut();
                    let s = caps.structure_mut(0).unwrap();
                    s.set("payload", rtp.payload_type() as i32);
                }
                e.insert(caps);
            }
        }

        // TODO: Put NTP time as `gst::ReferenceTimeStampMeta` on the buffers if selected via property
        let (pts, _ntp_time) = state.sync_context.as_mut().unwrap().calculate_pts(
            rtp.ssrc(),
            rtp.timestamp(),
            arrival_time.nseconds(),
        );
        let segment = session_inner.rtp_recv_sink_segment.as_ref().unwrap();
        let pts = segment
            .position_from_running_time(gst::ClockTime::from_nseconds(pts))
            .unwrap();
        gst::debug!(CAT, "Calculated PTS: {}", pts);

        drop(state);

        let now = Instant::now();
        let mut buffers_to_push = vec![];
        loop {
            match session_inner.session.handle_recv(&rtp, addr, now) {
                RecvReply::SsrcCollision(_ssrc) => (), // TODO: handle ssrc collision
                RecvReply::NewSsrc(ssrc, _pt) => {
                    drop(session_inner);
                    session.config.emit_by_name::<()>("new-ssrc", &[&ssrc]);
                    session_inner = session.inner.lock().unwrap();
                }
                RecvReply::Hold(hold_id) => {
                    let pt = rtp.payload_type();
                    let ssrc = rtp.ssrc();
                    drop(mapped);
                    {
                        let buf_mut = buffer.make_mut();
                        buf_mut.set_pts(pts);
                    }
                    let (pad, new_pad) = session_inner.get_or_create_rtp_recv_src(self, pt, ssrc);
                    session_inner.recv_store.push(HeldRecvBuffer {
                        hold_id: Some(hold_id),
                        buffer,
                        pad,
                        new_pad,
                    });
                    break;
                }
                RecvReply::Drop(hold_id) => {
                    if let Some(pos) = session_inner
                        .recv_store
                        .iter()
                        .position(|b| b.hold_id.unwrap() == hold_id)
                    {
                        session_inner.recv_store.remove(pos);
                    }
                }
                RecvReply::Forward(hold_id) => {
                    if let Some(pos) = session_inner
                        .recv_store
                        .iter()
                        .position(|b| b.hold_id.unwrap() == hold_id)
                    {
                        buffers_to_push.push(session_inner.recv_store.remove(pos));
                    } else {
                        unreachable!();
                    }
                }
                RecvReply::Ignore => break,
                RecvReply::Passthrough => {
                    let pt = rtp.payload_type();
                    let ssrc = rtp.ssrc();
                    drop(mapped);
                    {
                        let buf_mut = buffer.make_mut();
                        buf_mut.set_pts(pts);
                    }
                    let (pad, new_pad) = session_inner.get_or_create_rtp_recv_src(self, pt, ssrc);
                    buffers_to_push.push(HeldRecvBuffer {
                        hold_id: None,
                        buffer,
                        pad,
                        new_pad,
                    });
                    break;
                }
            }
        }

        drop(session_inner);

        for mut held in buffers_to_push {
            // TODO: handle other processing
            if held.new_pad {
                held.pad.activate(&session);
                self.obj().add_pad(&held.pad.pad).unwrap();
                let mut state = self.state.lock().unwrap();
                state.pads_session_id_map.insert(held.pad.pad.clone(), id);
                drop(state);
            }

            let mapped = held.buffer.map_readable().map_err(|e| {
                gst::error!(CAT, imp: self, "Failed to map input buffer {e:?}");
                gst::FlowError::Error
            })?;
            let rtp = match rtp_types::RtpPacket::parse(&mapped) {
                Ok(rtp) => rtp,
                Err(e) => {
                    gst::error!(CAT, imp: self, "Failed to parse input as valid rtp packet: {e:?}");
                    return Ok(gst::FlowSuccess::Ok);
                }
            };

            // FIXME: Should block if too many packets are stored here because the source pad task
            // is blocked
            let mut jitterbuffer_store = held.pad.jitter_buffer_store.lock().unwrap();

            match jitterbuffer_store.jitterbuffer.queue_packet(
                &rtp,
                held.buffer.pts().unwrap().nseconds(),
                now,
            ) {
                jitterbuffer::QueueResult::Flushing => {
                    // TODO: return flushing result upstream
                }
                jitterbuffer::QueueResult::Queued(id) => {
                    drop(mapped);

                    jitterbuffer_store
                        .store
                        .insert(id, JitterBufferItem::Packet(held.buffer));
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

        Ok(gst::FlowSuccess::Ok)
    }

    fn rtp_send_sink_chain(
        &self,
        id: usize,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let state = self.state.lock().unwrap();
        let Some(session) = state.session_by_id(id) else {
            gst::error!(CAT, "No session?");
            return Err(gst::FlowError::Error);
        };

        let mapped = buffer.map_readable().map_err(|e| {
            gst::error!(CAT, imp: self, "Failed to map input buffer {e:?}");
            gst::FlowError::Error
        })?;
        let rtp = match rtp_types::RtpPacket::parse(&mapped) {
            Ok(rtp) => rtp,
            Err(e) => {
                gst::error!(CAT, imp: self, "Failed to parse input as valid rtp packet: {e:?}");
                return Ok(gst::FlowSuccess::Ok);
            }
        };

        let session = session.clone();
        let mut session_inner = session.inner.lock().unwrap();
        drop(state);

        let now = Instant::now();
        loop {
            match session_inner.session.handle_send(&rtp, now) {
                SendReply::SsrcCollision(_ssrc) => (), // TODO: handle ssrc collision
                SendReply::NewSsrc(ssrc, _pt) => {
                    drop(session_inner);
                    session.config.emit_by_name::<()>("new-ssrc", &[&ssrc]);
                    session_inner = session.inner.lock().unwrap();
                }
                SendReply::Passthrough => break,
                SendReply::Drop => return Ok(gst::FlowSuccess::Ok),
            }
        }
        // TODO: handle other processing
        drop(mapped);
        let srcpad = session_inner.rtp_send_srcpad.clone().unwrap();
        drop(session_inner);
        srcpad.push(buffer)
    }

    fn rtcp_recv_sink_chain(
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
            gst::error!(CAT, imp: self, "Failed to map input buffer {e:?}");
            gst::FlowError::Error
        })?;
        let rtcp = match rtcp_types::Compound::parse(&mapped) {
            Ok(rtcp) => rtcp,
            Err(e) => {
                gst::error!(CAT, imp: self, "Failed to parse input as valid rtcp packet: {e:?}");
                return Ok(gst::FlowSuccess::Ok);
            }
        };

        let session = session.clone();
        let mut session_inner = session.inner.lock().unwrap();
        let waker = state.rtcp_waker.clone();
        drop(state);

        let now = Instant::now();
        let ntp_now = SystemTime::now();
        let replies =
            session_inner
                .session
                .handle_rtcp_recv(rtcp, mapped.len(), addr, now, ntp_now);
        let rtp_send_sinkpad = session_inner.rtp_send_sinkpad.clone();
        drop(session_inner);

        for reply in replies {
            match reply {
                RtcpRecvReply::NewSsrc(ssrc) => {
                    session.config.emit_by_name::<()>("new-ssrc", &[&ssrc]);
                }
                RtcpRecvReply::SsrcCollision(_ssrc) => (), // TODO: handle ssrc collision
                RtcpRecvReply::TimerReconsideration => {
                    if let Some(ref waker) = waker {
                        // reconsider timers means that we wake the rtcp task to get a new timeout
                        waker.wake_by_ref();
                    }
                }
                RtcpRecvReply::RequestKeyUnit { ssrcs, fir } => {
                    if let Some(ref rtp_send_sinkpad) = rtp_send_sinkpad {
                        gst::debug!(CAT, imp: self, "Sending force-keyunit event for ssrcs {ssrcs:?} (all headers: {fir})");
                        // TODO what to do with the ssrc?
                        let event = gst_video::UpstreamForceKeyUnitEvent::builder()
                            .all_headers(fir)
                            .other_field("ssrcs", &gst::Array::new(ssrcs))
                            .build();

                        let _ = rtp_send_sinkpad.push_event(event);
                    } else {
                        gst::debug!(CAT, imp: self, "Can't send force-keyunit event because of missing sinkpad");
                    }
                }
                RtcpRecvReply::NewCName((cname, ssrc)) => {
                    let mut state = self.state.lock().unwrap();

                    state.sync_context.as_mut().unwrap().associate(ssrc, &cname);
                }
                RtcpRecvReply::NewRtpNtp((ssrc, rtp, ntp)) => {
                    let mut state = self.state.lock().unwrap();

                    state
                        .sync_context
                        .as_mut()
                        .unwrap()
                        .add_sender_report(ssrc, rtp, ntp);
                }
                RtcpRecvReply::SsrcBye(ssrc) => {
                    session.config.emit_by_name::<()>("bye-ssrc", &[&ssrc])
                }
            }
        }
        drop(mapped);

        Ok(gst::FlowSuccess::Ok)
    }

    fn rtp_send_sink_event(&self, pad: &gst::Pad, event: gst::Event, id: usize) -> bool {
        match event.view() {
            gst::EventView::Caps(caps) => {
                if let Some((pt, clock_rate)) = pt_clock_rate_from_caps(caps.caps()) {
                    let state = self.state.lock().unwrap();
                    if let Some(session) = state.session_by_id(id) {
                        let mut session = session.inner.lock().unwrap();
                        session.session.set_pt_clock_rate(pt, clock_rate);
                    }
                } else {
                    gst::warning!(CAT, obj: pad, "input caps are missing payload or clock-rate fields");
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            gst::EventView::Eos(_eos) => {
                let now = Instant::now();
                let mut state = self.state.lock().unwrap();
                if let Some(session) = state.session_by_id(id) {
                    let mut session = session.inner.lock().unwrap();
                    let ssrcs = session.session.ssrcs().collect::<Vec<_>>();
                    // We want to bye all relevant ssrc's here.
                    // Relevant means they will not be used by something else which means that any
                    // local send ssrc that is not being used for Sr/Rr reports (internal_ssrc) can
                    // have the Bye state applied.
                    let mut all_local = true;
                    let internal_ssrc = session.session.internal_ssrc();
                    for ssrc in ssrcs {
                        let Some(local_send) = session.session.mut_local_send_source_by_ssrc(ssrc)
                        else {
                            if let Some(local_recv) =
                                session.session.local_receive_source_by_ssrc(ssrc)
                            {
                                if local_recv.state() != SourceState::Bye
                                    && Some(ssrc) != internal_ssrc
                                {
                                    all_local = false;
                                }
                            }
                            continue;
                        };
                        if Some(ssrc) != internal_ssrc {
                            local_send.mark_bye("End of Stream")
                        }
                    }
                    if all_local {
                        // if there are no non-local send ssrc's, then we can Bye the entire
                        // session.
                        session.session.schedule_bye("End of Stream", now);
                    }
                    drop(session);
                    if let Some(waker) = state.rtcp_waker.take() {
                        waker.wake();
                    }
                }
                drop(state);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    pub fn rtp_recv_sink_query(
        &self,
        pad: &gst::Pad,
        query: &mut gst::QueryRef,
        id: usize,
    ) -> bool {
        gst::log!(CAT, obj: pad, "Handling query {query:?}");

        if query.is_serialized() {
            let state = self.state.lock().unwrap();
            let mut ret = true;

            if let Some(session) = state.session_by_id(id) {
                let session = session.inner.lock().unwrap();

                let jb_stores: Vec<Arc<Mutex<JitterBufferStore>>> = session
                    .rtp_recv_srcpads
                    .iter()
                    .filter(|r| state.pads_session_id_map.contains_key(&r.pad))
                    .map(|p| p.jitter_buffer_store.clone())
                    .collect();

                drop(session);

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
    fn rtp_recv_sink_queue_serialized_event(&self, id: usize, event: gst::Event) -> bool {
        let state = self.state.lock().unwrap();
        if let Some(session) = state.session_by_id(id) {
            let session = session.inner.lock().unwrap();
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

    fn rtp_recv_sink_event(&self, pad: &gst::Pad, mut event: gst::Event, id: usize) -> bool {
        match event.view() {
            gst::EventView::StreamStart(stream_start) => {
                let state = self.state.lock().unwrap();

                if let Some(session) = state.session_by_id(id) {
                    let mut session = session.inner.lock().unwrap();

                    let group_id = stream_start.group_id();
                    session.rtp_recv_sink_group_id =
                        Some(group_id.unwrap_or_else(gst::GroupId::next));
                }

                true
            }
            gst::EventView::Caps(caps) => {
                let state = self.state.lock().unwrap();

                if let Some(session) = state.session_by_id(id) {
                    let mut session = session.inner.lock().unwrap();
                    let caps = caps.caps_owned();

                    if let Some((pt, clock_rate)) = pt_clock_rate_from_caps(&caps) {
                        session.session.set_pt_clock_rate(pt, clock_rate);
                    } else {
                        gst::warning!(CAT, obj: pad, "input caps are missing payload or clock-rate fields");
                    }

                    session.rtp_recv_sink_caps = Some(caps);
                }
                true
            }
            gst::EventView::Segment(segment) => {
                let state = self.state.lock().unwrap();

                if let Some(session) = state.session_by_id(id) {
                    let mut session = session.inner.lock().unwrap();

                    let segment = segment.segment();
                    let segment = match segment.downcast_ref::<gst::ClockTime>() {
                        Some(segment) => segment.clone(),
                        None => {
                            gst::warning!(CAT, obj: pad, "Only TIME segments are supported");

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

                self.rtp_recv_sink_queue_serialized_event(id, event)
            }
            gst::EventView::Eos(_eos) => {
                let now = Instant::now();
                let mut state = self.state.lock().unwrap();
                if let Some(session) = state.session_by_id(id) {
                    let mut session = session.inner.lock().unwrap();
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
                    if let Some(waker) = state.rtcp_waker.take() {
                        waker.wake();
                    }
                }
                drop(state);
                // FIXME: may need to delay sending eos under some circumstances
                self.rtp_recv_sink_queue_serialized_event(id, event);
                true
            }
            gst::EventView::FlushStart(_fs) => {
                let state = self.state.lock().unwrap();
                let mut pause_tasks = vec![];
                if let Some(session) = state.session_by_id(id) {
                    let session = session.inner.lock().unwrap();
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
                let state = self.state.lock().unwrap();
                if let Some(session) = state.session_by_id(id) {
                    let mut session = session.inner.lock().unwrap();
                    let pads = session
                        .rtp_recv_srcpads
                        .iter()
                        .map(|r| r.pad.clone())
                        .collect::<Vec<_>>();
                    for pad in pads {
                        // Will reset flushing to false and ensure task is woken up
                        let _ = session.start_rtp_recv_task(&pad);
                    }
                }
                drop(state);
                self.rtp_recv_sink_queue_serialized_event(id, event)
            }
            _ => {
                if event.is_serialized() {
                    self.rtp_recv_sink_queue_serialized_event(id, event)
                } else {
                    gst::Pad::event_default(pad, Some(&*self.obj()), event)
                }
            }
        }
    }

    fn rtp_recv_src_event(
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
                        let mut session = session.inner.lock().unwrap();
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

                            let waker = state.rtcp_waker.clone();
                            drop(session);
                            drop(state);

                            for reply in replies {
                                match reply {
                                    RequestRemoteKeyUnitReply::TimerReconsideration => {
                                        if let Some(ref waker) = waker {
                                            // reconsider timers means that we wake the rtcp task to get a new timeout
                                            waker.wake_by_ref();
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
impl ObjectSubclass for RtpBin2 {
    const NAME: &'static str = "GstRtpBin2";
    type Type = super::RtpBin2;
    type ParentType = gst::Element;

    fn new() -> Self {
        GstRustLogger::install();
        Self {
            settings: Default::default(),
            state: Default::default(),
            rtcp_task: Mutex::new(None),
        }
    }
}

impl ObjectImpl for RtpBin2 {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt::builder("latency")
                    .nick("Buffer latency in ms")
                    .blurb("Amount of ms to buffer")
                    .default_value(DEFAULT_LATENCY.mseconds() as u32)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("min-rtcp-interval")
                    .nick("Minimum RTCP interval in ms")
                    .blurb("Minimum time (in ms) between RTCP reports")
                    .default_value(DEFAULT_MIN_RTCP_INTERVAL.as_millis() as u32)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("stats")
                    .nick("Statistics")
                    .blurb("Statistics about the session")
                    .read_only()
                    .build(),
                glib::ParamSpecEnum::builder::<Profile>("rtp-profile")
                    .nick("RTP Profile")
                    .blurb("RTP Profile to use")
                    .default_value(Profile::default())
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("reduced-size-rtcp")
                    .nick("Reduced Size RTCP")
                    .blurb("Use reduced size RTCP. Only has an effect if rtp-profile=avpf")
                    .default_value(DEFAULT_REDUCED_SIZE_RTCP)
                    .mutable_ready()
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
            "min-rtcp-interval" => {
                let mut settings = self.settings.lock().unwrap();
                settings.min_rtcp_interval = Duration::from_millis(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            "rtp-profile" => {
                let mut settings = self.settings.lock().unwrap();
                settings.profile = value.get::<Profile>().expect("Type checked upstream");
            }
            "reduced-size-rtcp" => {
                let mut settings = self.settings.lock().unwrap();
                settings.reduced_size_rtcp = value.get::<bool>().expect("Type checked upstream");
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
            "latency" => {
                let settings = self.settings.lock().unwrap();
                (settings.latency.mseconds() as u32).to_value()
            }
            "min-rtcp-interval" => {
                let settings = self.settings.lock().unwrap();
                (settings.min_rtcp_interval.as_millis() as u32).to_value()
            }
            "stats" => {
                let state = self.state.lock().unwrap();
                state.stats().to_value()
            }
            "rtp-profile" => {
                let settings = self.settings.lock().unwrap();
                settings.profile.to_value()
            }
            "reduced-size-rtcp" => {
                let settings = self.settings.lock().unwrap();
                settings.reduced_size_rtcp.to_value()
            }
            "timestamping-mode" => {
                let settings = self.settings.lock().unwrap();
                settings.timestamping_mode.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![glib::subclass::Signal::builder("get-session")
                .param_types([u32::static_type()])
                .return_type::<crate::rtpbin2::config::RtpBin2Session>()
                .action()
                .class_handler(|_token, args| {
                    let element = args[0].get::<super::RtpBin2>().expect("signal arg");
                    let id = args[1].get::<u32>().expect("signal arg");
                    let bin = element.imp();
                    let state = bin.state.lock().unwrap();
                    state
                        .session_by_id(id as usize)
                        .map(|sess| sess.config.to_value())
                })
                .build()]
        });

        SIGNALS.as_ref()
    }
}

impl GstObjectImpl for RtpBin2 {}

impl ElementImpl for RtpBin2 {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP Bin",
                "Network/RTP/Filter",
                "RTP sessions management",
                "Matthew Waters <matthew@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let rtp_caps = gst::Caps::builder_full()
                .structure(gst::Structure::builder("application/x-rtp").build())
                .build();
            let rtcp_caps = gst::Caps::builder_full()
                .structure(gst::Structure::builder("application/x-rtcp").build())
                .build();

            vec![
                gst::PadTemplate::new(
                    "rtp_recv_sink_%u",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Request,
                    &rtp_caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "rtcp_recv_sink_%u",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Request,
                    &rtcp_caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "rtp_recv_src_%u_%u_%u",
                    gst::PadDirection::Src,
                    gst::PadPresence::Sometimes,
                    &rtp_caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "rtp_send_sink_%u",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Request,
                    &rtp_caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "rtp_send_src_%u",
                    gst::PadDirection::Src,
                    gst::PadPresence::Sometimes,
                    &rtp_caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "rtcp_send_src_%u",
                    gst::PadDirection::Src,
                    gst::PadPresence::Request,
                    &rtcp_caps,
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
            "rtp_send_sink_%u" => {
                sess_parse(name, "rtp_send_sink_", max_session_id).and_then(|id| {
                    let new_pad = move |session: &mut BinSessionInner| -> Option<(
                        gst::Pad,
                        Option<gst::Pad>,
                        usize,
                        Vec<gst::Event>,
                    )> {
                        let sinkpad = gst::Pad::builder_from_template(templ)
                            .chain_function(move |_pad, parent, buffer| {
                                RtpBin2::catch_panic_pad_function(
                                    parent,
                                    || Err(gst::FlowError::Error),
                                    |this| this.rtp_send_sink_chain(id, buffer),
                                )
                            })
                            .iterate_internal_links_function(|pad, parent| {
                                RtpBin2::catch_panic_pad_function(
                                    parent,
                                    || gst::Iterator::from_vec(vec![]),
                                    |this| this.iterate_internal_links(pad),
                                )
                            })
                            .event_function(move |pad, parent, event| {
                                RtpBin2::catch_panic_pad_function(
                                    parent,
                                    || false,
                                    |this| this.rtp_send_sink_event(pad, event, id),
                                )
                            })
                            .flags(gst::PadFlags::PROXY_CAPS)
                            .name(format!("rtp_send_sink_{}", id))
                            .build();
                        let src_templ = self.obj().pad_template("rtp_send_src_%u").unwrap();
                        let srcpad = gst::Pad::builder_from_template(&src_templ)
                            .iterate_internal_links_function(|pad, parent| {
                                RtpBin2::catch_panic_pad_function(
                                    parent,
                                    || gst::Iterator::from_vec(vec![]),
                                    |this| this.iterate_internal_links(pad),
                                )
                            })
                            .name(format!("rtp_send_src_{}", id))
                            .build();
                        session.rtp_send_sinkpad = Some(sinkpad.clone());
                        session.rtp_send_srcpad = Some(srcpad.clone());
                        Some((sinkpad, Some(srcpad), id, vec![]))
                    };

                    let session = state.session_by_id(id);
                    if let Some(session) = session {
                        let mut session = session.inner.lock().unwrap();
                        if session.rtp_send_sinkpad.is_some() {
                            None
                        } else {
                            new_pad(&mut session)
                        }
                    } else {
                        let session = BinSession::new(id, &settings);
                        let mut inner = session.inner.lock().unwrap();
                        let ret = new_pad(&mut inner);
                        drop(inner);
                        state.sessions.push(session);
                        ret
                    }
                })
            }
            "rtp_recv_sink_%u" => {
                sess_parse(name, "rtp_recv_sink_", max_session_id).and_then(|id| {
                    let new_pad = move |session: &mut BinSessionInner| -> Option<(
                        gst::Pad,
                        Option<gst::Pad>,
                        usize,
                        Vec<gst::Event>,
                    )> {
                        let sinkpad = gst::Pad::builder_from_template(templ)
                            .chain_function(move |pad, parent, buffer| {
                                RtpBin2::catch_panic_pad_function(
                                    parent,
                                    || Err(gst::FlowError::Error),
                                    |this| this.rtp_recv_sink_chain(pad, id, buffer),
                                )
                            })
                            .iterate_internal_links_function(|pad, parent| {
                                RtpBin2::catch_panic_pad_function(
                                    parent,
                                    || gst::Iterator::from_vec(vec![]),
                                    |this| this.iterate_internal_links(pad),
                                )
                            })
                            .event_function(move |pad, parent, event| {
                                RtpBin2::catch_panic_pad_function(
                                    parent,
                                    || false,
                                    |this| this.rtp_recv_sink_event(pad, event, id),
                                )
                            })
                            .query_function(move |pad, parent, query| {
                                RtpBin2::catch_panic_pad_function(
                                    parent,
                                    || false,
                                    |this| this.rtp_recv_sink_query(pad, query, id),
                                )
                            })
                            .name(format!("rtp_recv_sink_{}", id))
                            .build();
                        session.rtp_recv_sinkpad = Some(sinkpad.clone());
                        Some((sinkpad, None, id, vec![]))
                    };

                    let session = state.session_by_id(id);
                    if let Some(session) = session {
                        let mut session = session.inner.lock().unwrap();
                        if session.rtp_send_sinkpad.is_some() {
                            None
                        } else {
                            new_pad(&mut session)
                        }
                    } else {
                        let session = BinSession::new(id, &settings);
                        let mut inner = session.inner.lock().unwrap();
                        let ret = new_pad(&mut inner);
                        drop(inner);
                        state.sessions.push(session);
                        ret
                    }
                })
            }
            "rtcp_recv_sink_%u" => {
                sess_parse(name, "rtcp_recv_sink_", max_session_id).and_then(|id| {
                    state.session_by_id(id).and_then(|session| {
                        let mut session = session.inner.lock().unwrap();
                        if session.rtcp_recv_sinkpad.is_some() {
                            None
                        } else {
                            let sinkpad = gst::Pad::builder_from_template(templ)
                                .chain_function(move |_pad, parent, buffer| {
                                    RtpBin2::catch_panic_pad_function(
                                        parent,
                                        || Err(gst::FlowError::Error),
                                        |this| this.rtcp_recv_sink_chain(id, buffer),
                                    )
                                })
                                .iterate_internal_links_function(|pad, parent| {
                                    RtpBin2::catch_panic_pad_function(
                                        parent,
                                        || gst::Iterator::from_vec(vec![]),
                                        |this| this.iterate_internal_links(pad),
                                    )
                                })
                                .name(format!("rtcp_recv_sink_{}", id))
                                .build();
                            session.rtcp_recv_sinkpad = Some(sinkpad.clone());
                            Some((sinkpad, None, id, vec![]))
                        }
                    })
                })
            }
            "rtcp_send_src_%u" => {
                self.start_rtcp_task();
                sess_parse(name, "rtcp_send_src_", max_session_id).and_then(|id| {
                    state.session_by_id(id).and_then(|session| {
                        let mut session = session.inner.lock().unwrap();

                        if session.rtcp_send_srcpad.is_some() {
                            None
                        } else {
                            let srcpad = gst::Pad::builder_from_template(templ)
                                .iterate_internal_links_function(|pad, parent| {
                                    RtpBin2::catch_panic_pad_function(
                                        parent,
                                        || gst::Iterator::from_vec(vec![]),
                                        |this| this.iterate_internal_links(pad),
                                    )
                                })
                                .name(format!("rtcp_send_src_{}", id))
                                .build();

                            let stream_id = format!("{}/rtcp", id);
                            let stream_start = gst::event::StreamStart::builder(&stream_id).build();
                            let seqnum = stream_start.seqnum();

                            let caps = gst::Caps::new_empty_simple("application/x-rtcp");
                            let caps = gst::event::Caps::builder(&caps).seqnum(seqnum).build();

                            let segment = gst::FormattedSegment::<gst::ClockTime>::new();
                            let segment = gst::event::Segment::new(&segment);

                            session.rtcp_send_srcpad = Some(srcpad.clone());
                            Some((srcpad, None, id, vec![stream_start, caps, segment]))
                        }
                    })
                })
            }
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
        let state = self.state.lock().unwrap();
        let mut removed_pads = vec![];
        let mut removed_session_ids = vec![];
        if let Some(&id) = state.pads_session_id_map.get(pad) {
            removed_pads.push(pad.clone());
            if let Some(session) = state.session_by_id(id) {
                let mut session = session.inner.lock().unwrap();

                if Some(pad) == session.rtp_recv_sinkpad.as_ref() {
                    session.rtp_recv_sinkpad = None;
                    removed_pads.extend(session.rtp_recv_srcpads.iter().map(|r| r.pad.clone()));
                    session.recv_flow_combiner.lock().unwrap().clear();
                    session.rtp_recv_srcpads.clear();
                    session.recv_store.clear();
                }

                if Some(pad) == session.rtp_send_sinkpad.as_ref() {
                    session.rtp_send_sinkpad = None;
                    if let Some(srcpad) = session.rtp_send_srcpad.take() {
                        removed_pads.push(srcpad);
                    }
                }

                if Some(pad) == session.rtcp_send_srcpad.as_ref() {
                    session.rtcp_send_srcpad = None;
                }

                if Some(pad) == session.rtcp_recv_sinkpad.as_ref() {
                    session.rtcp_recv_sinkpad = None;
                }

                if session.rtp_recv_sinkpad.is_none()
                    && session.rtp_send_sinkpad.is_none()
                    && session.rtcp_recv_sinkpad.is_none()
                    && session.rtcp_send_srcpad.is_none()
                {
                    removed_session_ids.push(session.id);
                }
            }
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

            for pad in removed_pads.iter() {
                state.pads_session_id_map.remove(pad);
            }
            for id in removed_session_ids {
                if let Some(session) = state.session_by_id(id) {
                    let session = session.inner.lock().unwrap();
                    if session.rtp_recv_sinkpad.is_none()
                        && session.rtp_send_sinkpad.is_none()
                        && session.rtcp_recv_sinkpad.is_none()
                        && session.rtcp_send_srcpad.is_none()
                    {
                        let id = session.id;
                        drop(session);
                        state.sessions.retain(|s| s.id != id);
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
            gst::StateChange::ReadyToPaused => {
                let settings = self.settings.lock().unwrap();
                let mut state = self.state.lock().unwrap();

                state.sync_context = Some(sync::Context::new(settings.timestamping_mode));
            }
            _ => (),
        }

        let mut success = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::ReadyToNull => {
                self.stop_rtcp_task();
            }
            gst::StateChange::ReadyToPaused | gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                let mut removed_pads = vec![];
                for session in &state.sessions {
                    let mut session = session.inner.lock().unwrap();
                    removed_pads.extend(session.rtp_recv_srcpads.iter().map(|r| r.pad.clone()));

                    session.recv_flow_combiner.lock().unwrap().clear();
                    session.rtp_recv_srcpads.clear();
                    session.recv_store.clear();

                    session.rtp_recv_sink_caps = None;
                    session.rtp_recv_sink_segment = None;
                    session.rtp_recv_sink_seqnum = None;
                    session.rtp_recv_sink_group_id = None;

                    session.pt_map.clear();
                }
                state.sync_context = None;
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

pub fn pt_clock_rate_from_caps(caps: &gst::CapsRef) -> Option<(u8, u32)> {
    let Some(s) = caps.structure(0) else {
        gst::debug!(CAT, "no structure!");
        return None;
    };
    let Some((clock_rate, pt)) = Option::zip(
        s.get::<i32>("clock-rate").ok(),
        s.get::<i32>("payload").ok(),
    ) else {
        gst::debug!(
            CAT,
            "could not retrieve clock-rate and/or payload from structure"
        );
        return None;
    };
    if (0..=127).contains(&pt) && clock_rate > 0 {
        Some((pt as u8, clock_rate as u32))
    } else {
        gst::debug!(
            CAT,
            "payload value {pt} out of bounds or clock-rate {clock_rate} out of bounds"
        );
        None
    }
}

fn clock_rate_from_caps(caps: &gst::CapsRef) -> Option<u32> {
    let Some(s) = caps.structure(0) else {
        return None;
    };
    let Some(clock_rate) = s.get::<i32>("clock-rate").ok() else {
        return None;
    };
    if clock_rate > 0 {
        Some(clock_rate as u32)
    } else {
        None
    }
}

static RUST_CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rust-log",
        gst::DebugColorFlags::empty(),
        Some("Logs from rust crates"),
    )
});

static GST_RUST_LOGGER_ONCE: once_cell::sync::OnceCell<()> = once_cell::sync::OnceCell::new();
static GST_RUST_LOGGER: GstRustLogger = GstRustLogger {};

pub(crate) struct GstRustLogger {}

impl GstRustLogger {
    pub fn install() {
        GST_RUST_LOGGER_ONCE.get_or_init(|| {
            if log::set_logger(&GST_RUST_LOGGER).is_err() {
                gst::warning!(
                    RUST_CAT,
                    "Cannot install log->gst logger, already installed?"
                );
            } else {
                log::set_max_level(GstRustLogger::debug_level_to_log_level_filter(
                    RUST_CAT.threshold(),
                ));
                gst::info!(RUST_CAT, "installed log->gst logger");
            }
        });
    }

    fn debug_level_to_log_level_filter(level: gst::DebugLevel) -> log::LevelFilter {
        match level {
            gst::DebugLevel::None => log::LevelFilter::Off,
            gst::DebugLevel::Error => log::LevelFilter::Error,
            gst::DebugLevel::Warning => log::LevelFilter::Warn,
            gst::DebugLevel::Fixme | gst::DebugLevel::Info => log::LevelFilter::Info,
            gst::DebugLevel::Debug => log::LevelFilter::Debug,
            gst::DebugLevel::Log | gst::DebugLevel::Trace | gst::DebugLevel::Memdump => {
                log::LevelFilter::Trace
            }
            _ => log::LevelFilter::Trace,
        }
    }

    fn log_level_to_debug_level(level: log::Level) -> gst::DebugLevel {
        match level {
            log::Level::Error => gst::DebugLevel::Error,
            log::Level::Warn => gst::DebugLevel::Warning,
            log::Level::Info => gst::DebugLevel::Info,
            log::Level::Debug => gst::DebugLevel::Debug,
            log::Level::Trace => gst::DebugLevel::Trace,
        }
    }
}

impl log::Log for GstRustLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        RUST_CAT.above_threshold(GstRustLogger::log_level_to_debug_level(metadata.level()))
    }

    fn log(&self, record: &log::Record) {
        let gst_level = GstRustLogger::log_level_to_debug_level(record.metadata().level());
        let file = record
            .file()
            .map(glib::GString::from)
            .unwrap_or_else(|| glib::GString::from("rust-log"));
        let function = record.target();
        let line = record.line().unwrap_or(0);
        RUST_CAT.log(
            None::<&glib::Object>,
            gst_level,
            file.as_gstr(),
            function,
            line,
            *record.args(),
        );
    }

    fn flush(&self) {}
}
