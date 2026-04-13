// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtprecv
 * @see_also: rtpsend, rtpbin, rtpsession, rtpjitterbuffer.
 *
 * RTP session management (receiver).
 *
 * ## Environment variables
 *
 * The underlying Runtime can be fine-tuned process-wide using the following env vars:
 *
 * * 'GST_RTPBIN2_RT_WORKER_THREADS': number of worker threads the Runtime will use (default: 1)
 *   0 => number of cores available on the system.
 * * 'GST_RTPBIN2_RT_MAX_BLOCKING_THREADS': limit for the number of threads in the blocking pool (default: 512).
 *   When they push buffers, downstream events or handle downstream queries, rtpbin2 elements spawn
 *   a thread from the blocking pool. This is to avoid blocking the worker thread which is shared
 *   with other elements running on the same runtime.
 * * 'GST_RTPBIN2_RT_THREAD_KEEP_ALIVE': timeout for a thread in the blocking pool in ms (default: 10s).
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 \
 *  udpsrc port=5004 caps='application/x-rtp, media=audio, clock-rate=48000, encoding-name=OPUS, encoding-params=(string)1, sprop-stereo=(string)0, payload=96' \
 *  ! queue max-size-bytes=0 max-size-buffers=0 max-size-time=200000000 ! recv.rtp_sink_0 \
 *  udpsrc port=5005 caps='application/x-rtcp' \
 *  ! recv.rtcp_sink_0 \
 *  rtprecv name=recv rtp-id=example-rtp-id latency=200 \
 *  ! rtpopusdepay2 ! opusdec ! audioconvert ! audioresample ! queue max-size-bytes=0 max-size-buffers=1 max-size-time=0 ! autoaudiosink \
 *  rtpsend name=send rtp-id=example-rtp-id \
 *  send.rtcp_src_0 ! udpsink port=5007 host=127.0.0.1 async=false
 * ]| This will process incoming RTP & RTCP packets from UDP ports 5004 & 5005,
 * provided the RTP packets contain an Opus encoded audio stream, and will send
 * RTCP back to the sender on UDP port 5007.
 * See #rtpsend for an example of how to produce such packets.
 *
 * Since: plugins-rs-0.13.0
 */
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::SocketAddr;
use std::ops::{ControlFlow, Deref};
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, mpsc as sync_mpsc};
use std::task::{self, Poll, Waker};
use std::time::{Duration, Instant, SystemTime};

use futures::channel::mpsc as async_mpsc;
use futures::{StreamExt, stream};
use gst::{glib, prelude::*, subclass::prelude::*};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::internal::{GstRustLogger, SharedRtpState, SharedSession, pt_clock_rate_from_caps};
use super::jitterbuffer::{self, JitterBuffer};
use super::session::{
    KeyUnitRequestType, RTCP_MIN_REPORT_INTERVAL, RecvReply, RequestRemoteKeyUnitReply,
    RtcpRecvReply, RtpProfile,
};
use super::source::SourceState;
use super::sync;

use crate::rtpbin2;

const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::from_mseconds(200);

/// Initial capacity for `SmallVec`s handling items related to src pads for
/// a given RTP seession. E.g.: `RtpRecvSrcPads`, `JitterBufferStreams`, ...
const SRC_PAD_SMALL_VEC_CAPACITY: usize = 16;

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
    semaphore: tokio_util::sync::PollSemaphore,
    recv_src_pad: RtpRecvSrcPad,
    timer: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl JitterBufferStream {
    fn new(recv_src_pad: &RtpRecvSrcPad) -> Self {
        Self {
            semaphore: tokio_util::sync::PollSemaphore::new(recv_src_pad.semaphore.clone()),
            recv_src_pad: recv_src_pad.clone(),
            timer: None,
        }
    }
}

/// Up to two pending items returned in one go by `JitterBufferStream`
type JitterBufferPendingItems = smallvec::SmallVec<[JitterBufferItem; 2]>;

impl futures::stream::Stream for JitterBufferStream {
    type Item = (
        gst::Pad,
        Option<OwnedSemaphorePermit>,
        JitterBufferPendingItems,
    );

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let Self {
            ref mut semaphore,
            ref recv_src_pad,
            ref mut timer,
            ..
        } = *self.as_mut();

        loop {
            let Poll::Ready(src_pad_permit) = semaphore.poll_acquire(cx) else {
                return Poll::Pending;
            };
            let src_pad_permit = src_pad_permit.unwrap();

            let mut jitterbuffer_store = recv_src_pad.jitter_buffer_store.lock().unwrap();

            if let Some(ref mut jb_waker) = jitterbuffer_store.waker {
                if let Some(cur_timer) = timer.as_mut() {
                    if cur_timer.is_elapsed()
                        || std::future::Future::poll(cur_timer.as_mut(), cx).is_ready()
                    {
                        gst::trace!(
                            CAT,
                            obj = recv_src_pad.pad,
                            "Elapsed deadline {:?}, now {:?}",
                            cur_timer.deadline(),
                            Instant::now(),
                        );
                        *timer = None;
                    } else {
                        gst::trace!(
                            CAT,
                            obj = recv_src_pad.pad,
                            "Pending next deadline {:?}, now {:?}",
                            cur_timer.deadline(),
                            Instant::now()
                        );

                        return Poll::Pending;
                    }
                } else {
                    gst::trace!(
                        CAT,
                        obj = recv_src_pad.pad,
                        "Not awaken by upstream, no current deadline, returning Pending now {:?}",
                        Instant::now(),
                    );

                    // Update waker if necessary
                    jb_waker.clone_from(cx.waker());

                    return Poll::Pending;
                }
            } else {
                // We were woken up due to a new item
                // being pushed to the JB, in which case we need to poll the JB to check
                // if an earlier deadline should be considered.
                gst::trace!(CAT, obj = recv_src_pad.pad, "Woken up by upstream");

                jitterbuffer_store.waker = Some(cx.waker().clone());
            }

            let now = Instant::now();
            let mut pending_items = JitterBufferPendingItems::new();
            loop {
                let ret = jitterbuffer_store
                    .jitterbuffer
                    .poll(&recv_src_pad.pad.name(), now);
                gst::trace!(
                    CAT,
                    obj = recv_src_pad.pad,
                    "JitterBuffer poll ret: {ret:?}",
                );
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
                        if let JitterBufferItem::Packet(ref mut packet) = item
                            && discont
                        {
                            gst::debug!(CAT, obj = recv_src_pad.pad, "Forwarding discont buffer",);
                            let packet_mut = packet.make_mut();
                            packet_mut.set_flags(gst::BufferFlags::DISCONT);
                        }

                        gst::trace!(
                            CAT,
                            obj = recv_src_pad.pad,
                            "Handling item {id}: {item:?}, pending {}",
                            pending_items.len()
                        );

                        match item {
                            // we don't currently push packet lists into the jitterbuffer
                            JitterBufferItem::PacketList(_list) => unreachable!(),
                            // forward events and queries as-is
                            JitterBufferItem::Event(_) | JitterBufferItem::Query(_, _) => {
                                pending_items.push(item);
                                break;
                            }
                            JitterBufferItem::Packet(ref packet) => {
                                // Group consecutive buffers
                                match pending_items.pop() {
                                    Some(
                                        JitterBufferItem::Event(_) | JitterBufferItem::Query(_, _),
                                    ) => unreachable!(),
                                    Some(JitterBufferItem::Packet(pending_buffer)) => {
                                        let mut list = gst::BufferList::new();
                                        let list_mut = list.make_mut();
                                        list_mut.add(pending_buffer);
                                        list_mut.add(packet.clone());
                                        pending_items.push(JitterBufferItem::PacketList(list));
                                    }
                                    Some(JitterBufferItem::PacketList(mut pending_list)) => {
                                        let list_mut = pending_list.make_mut();
                                        list_mut.add(packet.clone());
                                        pending_items
                                            .push(JitterBufferItem::PacketList(pending_list));
                                    }
                                    None => {
                                        pending_items.push(item);
                                    }
                                }
                                continue;
                            }
                        }
                    }
                    jitterbuffer::PollResult::Timeout(timeout) => {
                        // Init the timer for next deadline
                        // If no items are pending, it will be polled immediately,
                        // otherwise, it will be polled on next poll_next
                        // after the pending items are pushed downstream.
                        *timer = Some(Box::pin(tokio::time::sleep_until(timeout.into())));
                        break;
                    }
                    jitterbuffer::PollResult::Empty => {
                        // Will be woken up when necessary
                        break;
                    }
                }
            }

            let store_is_empty = jitterbuffer_store.store.is_empty();

            if !pending_items.is_empty() {
                gst::trace!(
                    CAT,
                    obj = recv_src_pad.pad,
                    "Returning {} items, store is empty: {store_is_empty}",
                    pending_items.len(),
                );

                // No need to hold the src pad semaphore if the JB store is not empty
                return Poll::Ready(Some((
                    recv_src_pad.pad.clone(),
                    if store_is_empty {
                        Some(src_pad_permit)
                    } else {
                        None
                    },
                    pending_items,
                )));
            }

            if let Some(cur_timer) = timer {
                if std::future::Future::poll(cur_timer.as_mut(), cx).is_pending() {
                    gst::trace!(
                        CAT,
                        obj = recv_src_pad.pad,
                        "Returning Pending deadline {:?}, now {:?}",
                        cur_timer.deadline(),
                        Instant::now(),
                    );
                } else {
                    gst::trace!(
                        CAT,
                        obj = recv_src_pad.pad,
                        "Reached deadline {:?}",
                        cur_timer.deadline(),
                    );
                    *timer = None;
                    // check JitterBuffer again now
                    continue;
                }
            } else {
                gst::trace!(CAT, obj = recv_src_pad.pad, "Returning Pending");
            }

            // Update waker if necessary
            jitterbuffer_store
                .waker
                .as_mut()
                .expect("checked / inserted above")
                .clone_from(cx.waker());

            return Poll::Pending;
        }
    }
}

enum JitterBufferItem {
    Packet(gst::Buffer),
    PacketList(gst::BufferList),
    Event(gst::Event),
    Query(
        std::ptr::NonNull<gst::QueryRef>,
        std::sync::mpsc::SyncSender<bool>,
    ),
}

impl fmt::Debug for JitterBufferItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use JitterBufferItem::*;
        match self {
            Packet(buf) => f.debug_tuple("Packet").field(buf).finish(),
            PacketList(buf_list) => f.debug_tuple("PacketList").field(buf_list).finish(),
            Event(event) => f.debug_tuple("Event").field(event).finish(),
            Query(query, _) => f
                .debug_tuple("Query")
                // SAFETY: the `query` `ptr::NonNull` was built above from
                //         the `query` argument with type `&mut gst::QueryRef`.
                .field(unsafe { query.as_ref() })
                .finish(),
        }
    }
}

// SAFETY: Need to be able to pass *mut gst::QueryRef
unsafe impl Send for JitterBufferItem {}

#[derive(Debug)]
struct JitterBufferStore {
    store: BTreeMap<usize, JitterBufferItem>,
    waker: Option<Waker>,
    jitterbuffer: JitterBuffer,
}

type RtpRecvSrcPads = smallvec::SmallVec<[RtpRecvSrcPad; SRC_PAD_SMALL_VEC_CAPACITY]>;

#[derive(Debug)]
struct RtpRecvSrcPadInner {
    pt: u8,
    ssrc: u32,
    pad: gst::Pad,
    semaphore: Arc<Semaphore>,
    jitter_buffer_store: Arc<Mutex<JitterBufferStore>>,
    removed_sender: Arc<Mutex<Option<sync_mpsc::Sender<()>>>>,
}

#[derive(Debug, Clone)]
struct RtpRecvSrcPad(Arc<RtpRecvSrcPadInner>);

impl RtpRecvSrcPad {
    fn new(pt: u8, ssrc: u32, pad: gst::Pad, jb_store: JitterBufferStore) -> RtpRecvSrcPad {
        RtpRecvSrcPad(Arc::new(RtpRecvSrcPadInner {
            pt,
            ssrc,
            pad,
            semaphore: Arc::new(Semaphore::new(1)),
            jitter_buffer_store: Arc::new(Mutex::new(jb_store)),
            removed_sender: Default::default(),
        }))
    }
}

impl Deref for RtpRecvSrcPad {
    type Target = RtpRecvSrcPadInner;

    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
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
    arrival_time: Instant,
    buffer: gst::Buffer,
    recv_src_pad: RtpRecvSrcPad,
}

#[derive(Debug)]
struct HeldRecvBufferList {
    arrival_time: Instant,
    list: gst::BufferList,
    recv_src_pad: RtpRecvSrcPad,
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
enum RecvSessionSrcTaskCommand {
    AddRecvSrcPad(RtpRecvSrcPad),
    RemoveRecvSrcPad(RtpRecvSrcPad),
    Stop,
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

    rtp_task_cmd_tx: async_mpsc::UnboundedSender<RecvSessionSrcTaskCommand>,
    rtp_task_handle: Option<tokio::task::JoinHandle<()>>,

    rtp_recv_srcpads: Vec<RtpRecvSrcPad>,
    recv_flow_combiner: Arc<Mutex<gst_base::UniqueFlowCombiner>>,

    rtcp_recv_sinkpad: Option<gst::Pad>,
}

impl RecvSession {
    fn new(shared_state: &SharedRtpState, id: usize) -> Self {
        let internal_session = shared_state.session_get_or_init(id, || {
            SharedSession::new(id, RtpProfile::Avp, RTCP_MIN_REPORT_INTERVAL, false)
        });

        let recv_flow_combiner = Arc::new(Mutex::new(gst_base::UniqueFlowCombiner::new()));
        let (task, rtp_task_cmd_tx) = RecvSessionSrcTask::new(recv_flow_combiner.clone());

        Self {
            internal_session,
            rtp_recv_sinkpad: None,
            rtp_recv_sink_group_id: None,
            rtp_recv_sink_caps: None,
            rtp_recv_sink_segment: None,
            rtp_recv_sink_seqnum: None,

            recv_store: vec![],
            rtp_task_cmd_tx,
            rtp_task_handle: Some(
                rtpbin2::get_or_init_runtime()
                    .expect("initialized in change_state()")
                    .spawn(task.start()),
            ),

            rtp_recv_srcpads: vec![],
            recv_flow_combiner,

            rtcp_recv_sinkpad: None,
        }
    }

    fn activate_recv_src_pad(&mut self, pad: &gst::Pad) {
        gst::debug!(CAT, obj = pad, "Activating rtp recv src pad");

        self.recv_flow_combiner.lock().unwrap().reset();

        let recv_pad = self
            .rtp_recv_srcpads
            .iter_mut()
            .find(|recv| &recv.pad == pad)
            .unwrap();

        let mut jb_store = recv_pad.jitter_buffer_store.lock().unwrap();
        jb_store.jitterbuffer.set_flushing(&pad.name(), false);
        if let Some(waker) = jb_store.waker.take() {
            waker.wake()
        }
        drop(jb_store);

        self.rtp_task_cmd_tx
            .unbounded_send(RecvSessionSrcTaskCommand::AddRecvSrcPad(recv_pad.clone()))
            .expect("cmd chan valid until RecvSession is dropped");
    }

    fn deactivate_recv_src_pad(&mut self, pad: &gst::Pad) {
        gst::debug!(CAT, obj = pad, "Deactivating rtp recv src pad");

        let recv_pad = self
            .rtp_recv_srcpads
            .iter_mut()
            .find(|recv| &recv.pad == pad)
            .unwrap();

        let mut jb_store = recv_pad.jitter_buffer_store.lock().unwrap();
        jb_store.jitterbuffer.set_flushing(&pad.name(), true);
        if let Some(waker) = jb_store.waker.take() {
            waker.wake()
        }
        drop(jb_store);

        let _ = self
            .recv_flow_combiner
            .lock()
            .unwrap()
            .update_flow(Err(gst::FlowError::Flushing));

        let (removed_tx, removed_rx) = sync_mpsc::channel();
        {
            let mut removed_sender = recv_pad.removed_sender.lock().unwrap();
            if removed_sender.is_some() {
                gst::debug!(
                    CAT,
                    obj = pad,
                    "Deactivating rtp recv src pad already in progress?"
                );
                return;
            }

            *removed_sender = Some(removed_tx);
        }

        self.rtp_task_cmd_tx
            .unbounded_send(RecvSessionSrcTaskCommand::RemoveRecvSrcPad(
                recv_pad.clone(),
            ))
            .expect("cmd chan valid until RecvSession is dropped");

        // Wait for the pad to be effectively removed
        // this is to prevent race conditions, e.g. at rapid FlushStart then FlushStop
        let _ = removed_rx.recv();
    }

    fn get_or_create_rtp_src(
        &mut self,
        rtpbin: &RtpRecv,
        pt: u8,
        ssrc: u32,
    ) -> (RtpRecvSrcPad, bool) {
        let settings = rtpbin.settings.lock().unwrap();
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
                .name(format!("rtp_src_{id}_{pt}_{ssrc}"))
                .build();

            srcpad.use_fixed_caps();

            let recv_pad = RtpRecvSrcPad::new(
                pt,
                ssrc,
                srcpad.clone(),
                JitterBufferStore {
                    waker: None,
                    store: BTreeMap::new(),
                    jitterbuffer: JitterBuffer::new(settings.latency.into()),
                },
            );

            self.recv_flow_combiner
                .lock()
                .unwrap()
                .add_pad(&recv_pad.pad);
            self.rtp_recv_srcpads.push(recv_pad.clone());
            (recv_pad, true)
        }
    }
}

impl Drop for RecvSession {
    fn drop(&mut self) {
        let Some(rtp_task_handle) = self.rtp_task_handle.take() else {
            return;
        };
        if rtp_task_handle.is_finished() {
            return;
        }
        if self
            .rtp_task_cmd_tx
            .unbounded_send(RecvSessionSrcTaskCommand::Stop)
            .is_err()
        {
            return;
        }

        // All pads inactive and task Stop command sent
        // => rtp_task_handle will complete swiftly

        if let Ok(cur_rt_handle) = tokio::runtime::Handle::try_current() {
            // Running within a tokio runtime

            use tokio::runtime::RuntimeFlavor;
            if cur_rt_handle.runtime_flavor() == RuntimeFlavor::CurrentThread {
                if futures::executor::enter().is_ok() {
                    // This should not panic and will not block too long
                    let _ = futures::executor::block_on(rtp_task_handle);
                }
                // else, also running within a `futures` executor and we can't use `block_in_place`
                // ideally we would block here too, but we can't without panicking.
            } else {
                // Inform the runtime this might block a bit so it can take care of other tasks
                let _ =
                    tokio::task::block_in_place(move || cur_rt_handle.block_on(rtp_task_handle));
            }
        } else if futures::executor::enter().is_err() {
            // Already running within a `futures` executor but not within a `tokio` runtime
            // => this will not panic and will not block too long
            let _ = rtpbin2::get_or_init_runtime()
                .expect("initialized in change_state()")
                .block_on(rtp_task_handle);
        } else {
            // Not running within a `tokio` runtime nor a `futures` executor
            let _ = futures::executor::block_on(rtp_task_handle);
        }
    }
}

#[derive(Debug)]
struct RecvSessionSrcTask {
    cmd_rx: async_mpsc::UnboundedReceiver<RecvSessionSrcTaskCommand>,
    recv_flow_combiner: Arc<Mutex<gst_base::UniqueFlowCombiner>>,
}

type JitterBufferStreams = smallvec::SmallVec<[JitterBufferStream; SRC_PAD_SMALL_VEC_CAPACITY]>;

impl RecvSessionSrcTask {
    fn new(
        recv_flow_combiner: Arc<Mutex<gst_base::UniqueFlowCombiner>>,
    ) -> (
        RecvSessionSrcTask,
        async_mpsc::UnboundedSender<RecvSessionSrcTaskCommand>,
    ) {
        // task commands will be sent from a sync context
        // let's not rely on back pressure if the channel is full
        let (cmd_tx, cmd_rx) = async_mpsc::unbounded();

        (
            RecvSessionSrcTask {
                cmd_rx,
                recv_flow_combiner,
            },
            cmd_tx,
        )
    }

    fn combine_jb_streams(
        jb_streams: &mut [JitterBufferStream],
    ) -> impl stream::FusedStream<
        Item = Vec<(
            gst::Pad,
            Option<OwnedSemaphorePermit>,
            JitterBufferPendingItems,
        )>,
    > + use<'_> {
        let len = jb_streams.len();
        // + 1 so we don't end up with a 0 capacity when jb_streams is empty
        stream::select_all(jb_streams.iter_mut()).ready_chunks(len + 1)
    }

    fn handle_cmd(
        jb_streams: &mut JitterBufferStreams,
        cmd: RecvSessionSrcTaskCommand,
    ) -> ControlFlow<()> {
        gst::trace!(CAT, "Handling {cmd:?}");

        use RecvSessionSrcTaskCommand::*;
        match cmd {
            AddRecvSrcPad(recv_src_pad) => {
                let jb_stream = JitterBufferStream::new(&recv_src_pad);
                jb_streams.push(jb_stream);

                gst::debug!(CAT, obj = recv_src_pad.pad, "activated");
            }
            RemoveRecvSrcPad(recv_src_pad) => {
                if let Some(pos) = jb_streams
                    .iter()
                    .position(|jb| jb.recv_src_pad.pad == recv_src_pad.pad)
                {
                    let _ = jb_streams.remove(pos);
                }

                gst::debug!(CAT, obj = recv_src_pad.pad, "deactivated");
                if let Some(removed_tx) = recv_src_pad.removed_sender.lock().unwrap().take() {
                    let _ = removed_tx.send(());
                }
            }
            Stop => {
                return ControlFlow::Break(());
            }
        }

        ControlFlow::Continue(())
    }

    async fn start(mut self) {
        gst::debug!(CAT, "Entering rtp stream task");

        let mut jb_streams = JitterBufferStreams::new();
        let mut combined_jb_stream = Self::combine_jb_streams(&mut jb_streams);

        loop {
            gst::trace!(CAT, "RecvSessionSrcTask iter");

            // In case both the jb stream & command are ready,
            // always poll the jb stream first, so as to make sure
            // items are dropped when flushing.
            let all_pad_items = futures::select_biased! {
                all_pad_items = combined_jb_stream.next() => {
                    let Some(all_pad_items) = all_pad_items else {
                        continue;
                    };
                    if all_pad_items.is_empty() {
                        gst::debug!(CAT, "rtp stream task: all pad items is empty");
                        continue;
                    }
                    all_pad_items
                }
                cmd = self.cmd_rx.next() => {
                    drop(combined_jb_stream);
                    if Self::handle_cmd(
                        &mut jb_streams,
                        cmd.expect("cmd chan valid until RecvSession is dropped")).is_break() {
                        break;
                    }

                    combined_jb_stream = Self::combine_jb_streams(&mut jb_streams);
                    continue;
                }
            };

            drop(combined_jb_stream);
            match rtpbin2::get_or_init_runtime()
                .expect("initialized in change_state()")
                .spawn_blocking(move || {
                    gst::log!(CAT, "Handling items for {} src pads", all_pad_items.len());
                    self.push_all_pad_items_blocking(jb_streams, all_pad_items)
                })
                .await
                .unwrap()
            {
                ControlFlow::Continue((this, src_pad_jb_list_)) => {
                    self = this;
                    jb_streams = src_pad_jb_list_;
                    combined_jb_stream = Self::combine_jb_streams(&mut jb_streams);
                }
                ControlFlow::Break(_) => {
                    break;
                }
            }
        }

        gst::debug!(CAT, "Leaving RecvSessionSrc task");
    }

    fn push_all_pad_items_blocking(
        mut self,
        mut jb_streams: JitterBufferStreams,
        mut all_pad_items: Vec<(
            gst::Pad,
            Option<OwnedSemaphorePermit>,
            JitterBufferPendingItems,
        )>,
    ) -> ControlFlow<Self, (Self, JitterBufferStreams)> {
        loop {
            // If there was a flow error pushing buffers downstream then just discard
            // all future buffers but make sure to still handle any pending events
            // and queries.
            let mut combined_flow = Ok(gst::FlowSuccess::Ok);

            for (pad, _semaphore_permit, items) in all_pad_items.drain(..) {
                for item in items {
                    gst::log!(CAT, obj = pad, "Pushing item {item:?}");

                    match item {
                        JitterBufferItem::PacketList(list) => {
                            if combined_flow.is_ok() {
                                let flow = pad.push_list(list);
                                gst::trace!(
                                    CAT,
                                    obj = pad,
                                    "Pushed buffer list, flow ret {flow:?}"
                                );
                                let mut recv_flow_combiner =
                                    self.recv_flow_combiner.lock().unwrap();
                                combined_flow = recv_flow_combiner.update_pad_flow(&pad, flow);
                            }
                        }
                        JitterBufferItem::Packet(buffer) => {
                            if combined_flow.is_ok() {
                                let flow = pad.push(buffer);
                                gst::trace!(CAT, obj = pad, "Pushed buffer, flow ret {flow:?}");
                                let mut recv_flow_combiner =
                                    self.recv_flow_combiner.lock().unwrap();
                                combined_flow = recv_flow_combiner.update_pad_flow(&pad, flow);
                            }
                        }
                        JitterBufferItem::Event(event) => {
                            let res = pad.push_event(event);
                            gst::trace!(CAT, obj = pad, "Pushed serialized event, result: {res}");
                        }
                        JitterBufferItem::Query(mut query, tx) => {
                            // This is safe because the thread holding the original reference is waiting
                            // for us exclusively
                            let res = pad.peer_query(unsafe { query.as_mut() });
                            let _ = tx.send(res);
                        }
                    }
                }
            }

            // Check whether there's anything else we can do before leaving this blocking task

            if let Ok(cmd) = self.cmd_rx.try_recv()
                && Self::handle_cmd(&mut jb_streams, cmd).is_break()
            {
                return ControlFlow::Break(self);
            }
            // else, let the async task deal with errors

            let mut combined_jb_stream = Self::combine_jb_streams(&mut jb_streams);

            let mut cx = task::Context::from_waker(task::Waker::noop());
            let Poll::Ready(Some(all_pad_items_)) = combined_jb_stream.poll_next_unpin(&mut cx)
            else {
                drop(combined_jb_stream);
                gst::trace!(CAT, "Done processing items");

                return ControlFlow::Continue((self, jb_streams));
            };

            all_pad_items = all_pad_items_;
            gst::log!(CAT, "Got new items for {} src pads", all_pad_items.len());
            // iterate and process the new batch
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
    Forward((gst::Buffer, RtpRecvSrcPad)),
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
                session.activate_recv_src_pad(pad);
            } else {
                session.deactivate_recv_src_pad(pad);
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
        gst::log!(CAT, obj = pad, "Handling {query:?}");

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
        if let Some(&id) = state.pads_session_id_map.get(pad)
            && let Some(session) = state.session_by_id(id)
            && let Some(ref sinkpad) = session.rtp_recv_sinkpad
        {
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
        gst::Iterator::from_vec(vec![])
    }

    #[expect(clippy::too_many_arguments)]
    fn handle_buffer_locked<const H: usize, const P: usize>(
        &self,
        pad: &gst::Pad,
        session: &mut RecvSession,
        mut buffer: gst::Buffer,
        arrival_running_time: gst::ClockTime,
        arrival_time: Instant,
        items_to_pre_push: &mut smallvec::SmallVec<[HeldRecvItem; P]>,
        held_buffers: &mut smallvec::SmallVec<[HeldRecvBuffer; H]>,
    ) -> Result<RecvRtpBuffer, gst::FlowError> {
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
            gst::error!(CAT, obj = pad, "Failed to map input buffer {e:?}");
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
                    obj = pad,
                    "Failed to parse input as valid rtp packet: {e:?}"
                );
                return Ok(RecvRtpBuffer::Drop);
            }
        };

        gst::trace!(CAT, obj = pad, "using arrival time {arrival_time:?}");

        let internal_session = session.internal_session.clone();
        let mut session_inner = internal_session.inner.lock().unwrap();

        let pts = {
            let mut sync_context = self.sync_context.lock().unwrap();
            let sync_context = sync_context.as_mut().unwrap();
            if !sync_context.has_clock_rate(rtp.ssrc()) {
                let clock_rate = match session_inner.session.clock_rate_from_pt(rtp.payload_type())
                {
                    Some(clock_rate) => clock_rate,
                    None => {
                        // See RFC 5761 Section 4: An RTP packet with the marker bit
                        // and a payload type in this range is potentially an RTCP
                        // packet, especially if we don't know about the payload type.
                        //
                        // FIXME: If this turns out to be too weak / strong of a check
                        // we could make use of explicit signalling of rtcp-mux via the
                        // caps based on the a=rtcp-mux SDP attribute or similar.
                        if rtp.marker_bit() && (64..=95).contains(&rtp.payload_type()) {
                            // If this is a valid RTCP packet then it was muxed with the RTP stream and can be
                            // handled just fine.
                            if rtcp_types::Compound::parse(&mapped)
                                .is_ok_and(|mut rtcp| rtcp.next().is_some_and(|rtcp| rtcp.is_ok()))
                            {
                                drop(mapped);
                                return Ok(RecvRtpBuffer::IsRtcp(buffer));
                            }
                        }

                        gst::warning!(
                            CAT,
                            obj = pad,
                            "Have no clock-rate for payload type {}",
                            rtp.payload_type()
                        );
                        return Ok(RecvRtpBuffer::Drop);
                    }
                };
                sync_context.set_clock_rate(rtp.ssrc(), clock_rate);
            }

            // TODO: Put NTP time as `gst::ReferenceTimeStampMeta` on the buffers if selected via property
            let (pts, _ntp_time) = sync_context.calculate_pts(
                rtp.ssrc(),
                rtp.timestamp(),
                arrival_running_time.nseconds(),
            );
            pts
        };

        let segment = session.rtp_recv_sink_segment.as_ref().unwrap();
        let pts = segment
            .position_from_running_time(gst::ClockTime::from_nseconds(pts))
            .unwrap();
        gst::debug!(CAT, obj = pad, "Calculated PTS: {pts}");

        loop {
            let recv_ret = session_inner.session.handle_recv(&rtp, addr, arrival_time);
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
                    let (recv_src_pad, is_new_pad) = session.get_or_create_rtp_src(self, pt, ssrc);
                    if is_new_pad {
                        items_to_pre_push.push(HeldRecvItem::NewPad(recv_src_pad.clone()));
                    }
                    held_buffers.push(HeldRecvBuffer {
                        hold_id: Some(hold_id),
                        arrival_time,
                        buffer,
                        recv_src_pad,
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
                    let (recv_src_pad, is_new_pad) = session.get_or_create_rtp_src(self, pt, ssrc);
                    if is_new_pad {
                        items_to_pre_push.push(HeldRecvItem::NewPad(recv_src_pad.clone()));
                    }
                    return Ok(RecvRtpBuffer::Forward((buffer, recv_src_pad)));
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
                pad.push_event(
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
                HeldRecvItem::Buffer(mut buffer) => {
                    let mapped = buffer.buffer.map_readable().map_err(|e| {
                        gst::error!(
                            CAT,
                            obj = buffer.recv_src_pad.pad,
                            "Failed to map input buffer {e:?}"
                        );
                        gst::FlowError::Error
                    })?;
                    let rtp = match rtp_types::RtpPacket::parse(&mapped) {
                        Ok(rtp) => rtp,
                        Err(e) => {
                            gst::error!(
                                CAT,
                                obj = buffer.recv_src_pad.pad,
                                "Failed to parse input as valid rtp packet: {e:?}"
                            );
                            return Ok(state);
                        }
                    };

                    // FIXME: Should block if too many packets are stored here because the source pad task
                    // is blocked

                    // Make sure not to deadlock, e.g.:
                    // 1. 1st buffer is currently being handled by the src pad's task => holds semaphore.
                    // 2. 1st buffer reaches downstream AudioDecoder => Latency query.
                    // 3. src pad's task still holding semaphore while relaying Latency query.
                    // 4. `src_query()` calls `Pad::default()` which calls `iterate_internal_links()`.
                    // 5. `iterate_internal_links()` tries to acquire the `state` `Mutex`.
                    // => deadlock.
                    drop(state);
                    let _src_pad_permit = rtpbin2::get_or_init_runtime()
                        .expect("initialized in change_state()")
                        .block_on(buffer.recv_src_pad.semaphore.acquire());
                    state = self.state.lock().unwrap();

                    let mut jb_store = buffer.recv_src_pad.jitter_buffer_store.lock().unwrap();

                    let ret = jb_store.jitterbuffer.queue_packet(
                        &buffer.recv_src_pad.pad.name(),
                        &rtp,
                        buffer.buffer.pts().unwrap().nseconds(),
                        buffer.arrival_time,
                    );
                    gst::trace!(
                        CAT,
                        obj = buffer.recv_src_pad.pad,
                        "jb queue buffer pts {} rtp ts {} marker {}: {ret:?}",
                        buffer.buffer.pts().display(),
                        rtp.timestamp(),
                        rtp.marker_bit(),
                    );
                    match ret {
                        jitterbuffer::QueueResult::Flushing => {
                            return Err(gst::FlowError::Flushing);
                        }
                        jitterbuffer::QueueResult::Forward { id: _, discont } => {
                            drop(mapped);

                            if let Some(session) = state.session_by_id(id) {
                                let flow_combiner = session.recv_flow_combiner.clone();
                                drop(state);

                                if discont {
                                    let buffer_mut = buffer.buffer.make_mut();
                                    buffer_mut.set_flags(gst::BufferFlags::DISCONT);
                                }

                                let res = buffer.recv_src_pad.pad.push(buffer.buffer);
                                gst::trace!(
                                    CAT,
                                    obj = buffer.recv_src_pad.pad,
                                    "Pushed buffer, flow ret {res:?}"
                                );
                                flow_combiner
                                    .lock()
                                    .unwrap()
                                    .update_pad_flow(&buffer.recv_src_pad.pad, res)?;

                                state = self.state.lock().unwrap();
                            }
                        }
                        jitterbuffer::QueueResult::Queued(id) => {
                            drop(mapped);

                            jb_store
                                .store
                                .insert(id, JitterBufferItem::Packet(buffer.buffer));
                            if let Some(waker) = jb_store.waker.take() {
                                waker.wake()
                            }
                        }
                        jitterbuffer::QueueResult::Late => {
                            gst::warning!(
                                CAT,
                                obj = buffer.recv_src_pad.pad,
                                "Late buffer was dropped"
                            );
                        }
                        jitterbuffer::QueueResult::Duplicate => {
                            gst::warning!(
                                CAT,
                                obj = buffer.recv_src_pad.pad,
                                "Duplicate buffer was dropped"
                            );
                        }
                    }
                }
                HeldRecvItem::BufferList(mut list) => {
                    // FIXME: Should block if too many packets are stored here because the source pad task
                    // is blocked

                    let _src_pad_permit = rtpbin2::get_or_init_runtime()
                        .expect("initialized in change_state()")
                        .block_on(list.recv_src_pad.semaphore.acquire());

                    let mut jb_store = list.recv_src_pad.jitter_buffer_store.lock().unwrap();

                    // Collect all buffers with the same PTS in a new list to push them
                    // all downstream in one go.
                    let mut new_list = None;

                    // Small internal helper function for pushing a buffer list as a single buffer
                    // if there's only one buffer, or as a full list otherwise, and updating the
                    // flow combiner accordingly.
                    fn push_list(
                        flow_combiner: Arc<Mutex<gst_base::UniqueFlowCombiner>>,
                        recv_src_pad: &RtpRecvSrcPad,
                        buf_list: gst::BufferList,
                    ) -> Result<(), gst::FlowError> {
                        let res = if buf_list.len() == 1 {
                            let buffer = buf_list.get_owned(0).unwrap();
                            drop(buf_list);
                            let res = recv_src_pad.pad.push(buffer);
                            gst::trace!(
                                CAT,
                                obj = recv_src_pad.pad,
                                "Pushed buffer, flow ret {res:?}"
                            );
                            res
                        } else {
                            let res = recv_src_pad.pad.push_list(buf_list);
                            gst::trace!(
                                CAT,
                                obj = recv_src_pad.pad,
                                "Pushed buffer list, flow ret {res:?}"
                            );
                            res
                        };
                        flow_combiner
                            .lock()
                            .unwrap()
                            .update_pad_flow(&recv_src_pad.pad, res)?;

                        Ok(())
                    }

                    let buf_list = list.list.make_mut();
                    for mut buffer in buf_list.drain(..) {
                        let mapped = buffer.map_readable().map_err(|e| {
                            gst::error!(
                                CAT,
                                obj = list.recv_src_pad.pad,
                                "Failed to map input buffer {e:?}"
                            );
                            gst::FlowError::Error
                        })?;
                        let rtp = match rtp_types::RtpPacket::parse(&mapped) {
                            Ok(rtp) => rtp,
                            Err(e) => {
                                gst::error!(
                                    CAT,
                                    obj = list.recv_src_pad.pad,
                                    "Failed to parse input as valid rtp packet: {e:?}"
                                );
                                continue;
                            }
                        };

                        let ret = jb_store.jitterbuffer.queue_packet(
                            &list.recv_src_pad.pad.name(),
                            &rtp,
                            buffer.pts().unwrap().nseconds(),
                            list.arrival_time,
                        );
                        gst::trace!(
                            CAT,
                            obj = list.recv_src_pad.pad,
                            "jb queue buffer in list: {ret:?}",
                        );
                        match ret {
                            jitterbuffer::QueueResult::Flushing => {
                                return Err(gst::FlowError::Flushing);
                            }
                            jitterbuffer::QueueResult::Forward { id: _, discont } => {
                                drop(mapped);

                                if discont {
                                    let buffer_mut = buffer.make_mut();
                                    buffer_mut.set_flags(gst::BufferFlags::DISCONT);
                                }

                                if let Some(session) = state.session_by_id(id) {
                                    match new_list {
                                        None => {
                                            let new_pts = buffer.pts();
                                            let mut list = gst::BufferList::new();
                                            let list_ref = list.get_mut().unwrap();
                                            list_ref.add(buffer);
                                            new_list = Some((new_pts, list));
                                        }
                                        Some((pts, ref mut list)) if pts == buffer.pts() => {
                                            let list_ref = list.get_mut().unwrap();
                                            list_ref.add(buffer);
                                        }
                                        Some((_pts, old_list)) => {
                                            let flow_combiner = session.recv_flow_combiner.clone();
                                            drop(state);

                                            push_list(flow_combiner, &list.recv_src_pad, old_list)?;

                                            state = self.state.lock().unwrap();

                                            let new_pts = buffer.pts();
                                            let mut list = gst::BufferList::new();
                                            let list_ref = list.get_mut().unwrap();
                                            list_ref.add(buffer);
                                            new_list = Some((new_pts, list));
                                        }
                                    }
                                }
                            }
                            jitterbuffer::QueueResult::Queued(id) => {
                                drop(mapped);

                                // Forward any pending buffers now that we need to start queueing
                                if let Some(((_pts, new_list), session)) =
                                    Option::zip(new_list.take(), state.session_by_id(id))
                                {
                                    let flow_combiner = session.recv_flow_combiner.clone();
                                    drop(state);

                                    push_list(flow_combiner, &list.recv_src_pad, new_list)?;

                                    state = self.state.lock().unwrap();
                                }

                                jb_store.store.insert(id, JitterBufferItem::Packet(buffer));

                                if let Some(waker) = jb_store.waker.take() {
                                    waker.wake()
                                }
                            }
                            jitterbuffer::QueueResult::Late => {
                                gst::warning!(
                                    CAT,
                                    obj = list.recv_src_pad.pad,
                                    "Late buffer was dropped"
                                );
                            }
                            jitterbuffer::QueueResult::Duplicate => {
                                gst::warning!(
                                    CAT,
                                    obj = list.recv_src_pad.pad,
                                    "Duplicate buffer was dropped"
                                );
                            }
                        }
                    }

                    // Finally forward any buffers that are still left
                    if let Some(((_pts, new_list), session)) =
                        Option::zip(new_list, state.session_by_id(id))
                    {
                        let flow_combiner = session.recv_flow_combiner.clone();
                        drop(state);

                        push_list(flow_combiner, &list.recv_src_pad, new_list)?;

                        state = self.state.lock().unwrap();
                    }
                }
            }
        }

        Ok(state)
    }

    /// Get the arrival running time & arrival time of the incoming packet
    ///
    /// The arrival time of the packet is the instant when this packet was received
    /// by current branch. The source element is expected to set the dts of the buffer
    /// so it can be converted to the running time when the packet was received.
    ///
    /// Falls back to the (`handling_running_time`, `handling_instant`) which are expected
    /// to be the instant when the buffer was handled by `rtprecv` chain's method.
    fn get_arrival_times(
        &self,
        pad: &gst::Pad,
        session: &RecvSession,
        handling_running_time: gst::ClockTime,
        handling_instant: Instant,
        packet: &gst::BufferRef,
    ) -> Result<(gst::ClockTime, Instant), gst::FlowError> {
        // TODO: this is different from the old C implementation, where we
        // simply used the RTP timestamps as they were instead of doing any
        // sort of skew calculations.
        //
        // Check if this makes sense or if this leads to issue with eg interleaved
        // TCP.
        let (arrival_running_time, arrival_time) = match packet.dts() {
            Some(dts) => {
                let segment = session.rtp_recv_sink_segment.as_ref().unwrap();
                // TODO: use running_time_full if we care to support that
                let arrival_running_time = match segment.to_running_time(dts) {
                    Some(time) => time,
                    None => {
                        gst::error!(
                            CAT,
                            obj = pad,
                            "out of segment DTS are not supported {packet:?}"
                        );
                        return Err(gst::FlowError::Error);
                    }
                };

                let handling_delay = handling_running_time
                    .checked_sub(arrival_running_time)
                    .ok_or_else(|| {
                        gst::error!(
                            CAT,
                            obj = pad,
                            "arrival_running_time {arrival_running_time} > handling_running_time {handling_running_time} {packet:?}"
                        );
                        gst::FlowError::Error
                    })?;

                let arrival_time = handling_instant.checked_sub(Duration::from_nanos(handling_delay.nseconds())).ok_or_else(||
                {
                    gst::error!(
                        CAT,
                        obj = pad,
                        "Failed to compute arrival time, handling_instant {handling_instant:?}, handling_delay: {handling_delay} {packet:?}"
                    );
                    gst::FlowError::Error
                })?;

                (arrival_running_time, arrival_time)
            }
            None => (handling_running_time, handling_instant),
        };

        Ok((arrival_running_time, arrival_time))
    }

    fn rtp_sink_chain_list(
        &self,
        pad: &gst::Pad,
        id: usize,
        mut list: gst::BufferList,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let Some(handling_running_time) = self.obj().current_running_time() else {
            gst::error!(CAT, obj = pad, "Failed to get current time");
            return Err(gst::FlowError::Error);
        };
        let handling_instant = Instant::now();

        let mut state = self.state.lock().unwrap();
        let Some(session) = state.mut_session_by_id(id) else {
            return Err(gst::FlowError::Error);
        };

        // Return last error without further processing
        session
            .recv_flow_combiner
            .lock()
            .unwrap()
            .update_flow(Ok(gst::FlowSuccess::Ok))?;

        let (arrival_running_time, arrival_time) = {
            let Some(first_packet) = list.get(0) else {
                gst::debug!(CAT, obj = pad, "Skipping empty list");
                return Ok(gst::FlowSuccess::Ok);
            };
            self.get_arrival_times(
                pad,
                session,
                handling_running_time,
                handling_instant,
                first_packet,
            )?
        };

        let mut ssrc_collision: smallvec::SmallVec<[u32; 4]> = Default::default();
        let mut items_to_pre_push: smallvec::SmallVec<[HeldRecvItem; 4]> =
            smallvec::SmallVec::with_capacity(list.len() + 2);
        let mut held_buffers: smallvec::SmallVec<[HeldRecvBuffer; 4]> = Default::default();
        let mut split_bufferlist = false;
        let mut previous_recv_src_pad = None;
        let list_mut = list.make_mut();
        let mut ret = Ok(());
        list_mut.foreach_mut(|buffer, _i| {
            match self.handle_buffer_locked(
                pad,
                session,
                buffer,
                arrival_running_time,
                arrival_time,
                &mut items_to_pre_push,
                &mut held_buffers,
            ) {
                Ok(RecvRtpBuffer::SsrcCollision(ssrc)) => {
                    ssrc_collision.push(ssrc);
                    ControlFlow::Continue(None)
                }
                Ok(RecvRtpBuffer::IsRtcp(buffer)) => {
                    match Self::rtcp_sink_chain(self, pad, id, buffer) {
                        Ok(_buf) => ControlFlow::Continue(None),
                        Err(e) => {
                            ret = Err(e);
                            ControlFlow::Break(None)
                        }
                    }
                }
                Ok(RecvRtpBuffer::Drop) => ControlFlow::Continue(None),
                Ok(RecvRtpBuffer::Forward((buffer, recv_src_pad))) => {
                    // if all the buffers do not end up in the same jitterbuffer, then we need to
                    // split
                    if !split_bufferlist
                        && previous_recv_src_pad
                            .as_ref()
                            .is_some_and(|previous: &RtpRecvSrcPad| {
                                !Arc::ptr_eq(&previous.0, &recv_src_pad.0)
                            })
                    {
                        split_bufferlist = true;
                    }
                    previous_recv_src_pad = Some(recv_src_pad);
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
        state = self.handle_push_jitterbuffer(state, id, items_to_pre_push)?;
        if split_bufferlist {
            assert!(!list_mut.is_empty());

            let previous_recv_src_pad = previous_recv_src_pad.unwrap();

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
                        arrival_time,
                        buffer,
                        recv_src_pad: previous_recv_src_pad.clone(),
                    })],
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
        } else if !list.is_empty() {
            state = self.handle_push_jitterbuffer(
                state,
                id,
                [HeldRecvItem::BufferList(HeldRecvBufferList {
                    arrival_time,
                    list,
                    recv_src_pad: previous_recv_src_pad.unwrap(),
                })],
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
        let Some(handling_running_time) = self.obj().current_running_time() else {
            gst::error!(CAT, obj = pad, "Failed to get current time");
            return Err(gst::FlowError::Error);
        };
        let handling_instant = Instant::now();

        let mut state = self.state.lock().unwrap();
        let Some(session) = state.mut_session_by_id(id) else {
            return Err(gst::FlowError::Error);
        };

        // Return last error without further processing
        session
            .recv_flow_combiner
            .lock()
            .unwrap()
            .update_flow(Ok(gst::FlowSuccess::Ok))?;

        let (arrival_running_time, arrival_time) = self.get_arrival_times(
            pad,
            session,
            handling_running_time,
            handling_instant,
            buffer.as_ref(),
        )?;

        let mut items_to_pre_push: smallvec::SmallVec<[HeldRecvItem; 4]> = Default::default();
        let mut held_buffers: smallvec::SmallVec<[HeldRecvBuffer; 4]> = Default::default();
        let forward = match self.handle_buffer_locked(
            pad,
            session,
            buffer,
            arrival_running_time,
            arrival_time,
            &mut items_to_pre_push,
            &mut held_buffers,
        )? {
            RecvRtpBuffer::SsrcCollision(ssrc) => {
                return self.handle_ssrc_collision(session, [ssrc]);
            }
            RecvRtpBuffer::IsRtcp(buffer) => {
                drop(state);
                return Self::rtcp_sink_chain(self, pad, id, buffer);
            }
            RecvRtpBuffer::Drop => None,
            RecvRtpBuffer::Forward((buffer, jb)) => Some((buffer, jb)),
        };
        session
            .recv_store
            .extend(held_buffers.into_iter().map(HeldRecvItem::Buffer));

        state = self.handle_push_jitterbuffer(state, id, items_to_pre_push)?;
        if let Some((buffer, recv_src_pad)) = forward {
            state = self.handle_push_jitterbuffer(
                state,
                id,
                [HeldRecvItem::Buffer(HeldRecvBuffer {
                    hold_id: None,
                    arrival_time,
                    buffer,
                    recv_src_pad,
                })],
            )?;
        }
        drop(state);

        Ok(gst::FlowSuccess::Ok)
    }

    fn rtcp_sink_chain(
        &self,
        pad: &gst::Pad,
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
            gst::error!(CAT, obj = pad, "Failed to map input buffer {e:?}");
            gst::FlowError::Error
        })?;
        let rtcp = match rtcp_types::Compound::parse(&mapped) {
            Ok(rtcp) => rtcp,
            Err(e) => {
                gst::error!(
                    CAT,
                    obj = pad,
                    "Failed to parse input as valid rtcp packet: {e:?}"
                );
                return Ok(gst::FlowSuccess::Ok);
            }
        };

        let internal_session = session.internal_session.clone();
        let mut session_inner = internal_session.inner.lock().unwrap();

        let now = Instant::now();
        let ntp_now = SystemTime::now();
        let replies = session_inner.session.handle_rtcp_recv(
            &pad.name(),
            rtcp,
            mapped.len(),
            addr,
            now,
            ntp_now,
        );
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
                            obj = pad,
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
                            obj = pad,
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
        gst::log!(CAT, obj = pad, "Handling {query:?}");

        if query.is_serialized() {
            let state = self.state.lock().unwrap();
            let mut ret = true;

            if let Some(session) = state.session_by_id(id) {
                let recv_src_pads: RtpRecvSrcPads = session
                    .rtp_recv_srcpads
                    .iter()
                    .filter(|r| state.pads_session_id_map.contains_key(&r.pad))
                    .cloned()
                    .collect();

                drop(state);

                let mut query = std::ptr::NonNull::from(query);

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
                for recv_src_pad in recv_src_pads {
                    let src_pad_permit = rtpbin2::get_or_init_runtime()
                        .expect("initialized in change_state()")
                        .block_on(recv_src_pad.semaphore.acquire());

                    let mut jb_store = recv_src_pad.jitter_buffer_store.lock().unwrap();

                    match jb_store
                        .jitterbuffer
                        .queue_serialized_item(&recv_src_pad.pad.name())
                    {
                        jitterbuffer::QueueResult::Forward { id, discont: _ } => {
                            // SAFETY: the `query` `ptr::NonNull` was built above from
                            //         the `query` argument with type `&mut gst::QueryRef`.
                            let query = unsafe { query.as_mut() };

                            gst::trace!(
                                CAT,
                                obj = recv_src_pad.pad,
                                "querying ({id}) peer: {query:?}",
                            );

                            ret |= recv_src_pad.pad.peer_query(query);
                            if ret {
                                break;
                            }
                        }
                        jitterbuffer::QueueResult::Queued(id) => {
                            gst::trace!(
                                CAT,
                                obj = recv_src_pad.pad,
                                "jb queuing serialized query ({id}): {:?}",
                                // SAFETY: the `query` `ptr::NonNull` was built above from
                                //         the `query` argument with type `&mut gst::QueryRef`.
                                unsafe { query.as_ref() },
                            );

                            let (query_tx, query_rx) = std::sync::mpsc::sync_channel(1);

                            jb_store
                                .store
                                .insert(id, JitterBufferItem::Query(query, query_tx));

                            if let Some(waker) = jb_store.waker.take() {
                                waker.wake();
                            }

                            drop(jb_store);
                            drop(src_pad_permit);

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
                        _ => unreachable!(),
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
        let recv_src_pads: RtpRecvSrcPads = {
            let state = self.state.lock().unwrap();

            let Some(session) = state.session_by_id(id) else {
                return true;
            };

            session
                .rtp_recv_srcpads
                .iter()
                .filter(|r| state.pads_session_id_map.contains_key(&r.pad))
                .cloned()
                .collect()
        };

        for recv_src_pad in recv_src_pads {
            let _src_pad_permit = rtpbin2::get_or_init_runtime()
                .expect("initialized in change_state()")
                .block_on(recv_src_pad.semaphore.acquire());

            let mut jb_store = recv_src_pad.jitter_buffer_store.lock().unwrap();

            match jb_store
                .jitterbuffer
                .queue_serialized_item(&recv_src_pad.pad.name())
            {
                jitterbuffer::QueueResult::Forward { id, discont: _ } => {
                    gst::trace!(CAT, obj = recv_src_pad.pad, "Forwarding {id}: {event:?}");

                    if !recv_src_pad.pad.push_event(event.clone()) {
                        gst::warning!(CAT, obj = recv_src_pad.pad, "Failed to push event");
                    }
                }
                jitterbuffer::QueueResult::Queued(id) => {
                    gst::trace!(CAT, obj = recv_src_pad.pad, "Queuing as {id}: {event:?}");

                    jb_store
                        .store
                        .insert(id, JitterBufferItem::Event(event.clone()));
                    if let Some(waker) = jb_store.waker.take() {
                        waker.wake();
                    }
                }
                _ => {
                    unreachable!();
                }
            }
        }

        true
    }

    fn rtp_sink_event(&self, pad: &gst::Pad, mut event: gst::Event, id: usize) -> bool {
        gst::log!(CAT, obj = pad, "Handling {event:?}");

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
                    gst::debug!(
                        CAT,
                        obj = pad,
                        "input caps are missing payload or clock-rate fields, need to use pt-map"
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
                                && local_send.state() != SourceState::Bye
                                && Some(ssrc) != internal_ssrc
                            {
                                all_remote = false;
                                break;
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
                let mut state = self.state.lock().unwrap();
                if let Some(session) = state.mut_session_by_id(id) {
                    let pads = session
                        .rtp_recv_srcpads
                        .iter()
                        .map(|r| r.pad.clone())
                        .collect::<Vec<_>>();
                    for pad in pads {
                        session.deactivate_recv_src_pad(&pad);
                    }
                }
                drop(state);
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
                        session.activate_recv_src_pad(&pad);
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
        gst::log!(CAT, obj = pad, "Handling {event:?}");

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
                glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
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
            vec![
                glib::subclass::Signal::builder("get-session")
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
                    .build(),
            ]
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
                        .name(format!("rtp_sink_{id}"))
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
                        .get_or_insert_with(|| SharedRtpState::get_or_init(rtp_id));
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
                        .chain_function(move |pad, parent, buffer| {
                            RtpRecv::catch_panic_pad_function(
                                parent,
                                || Err(gst::FlowError::Error),
                                |this| this.rtcp_sink_chain(pad, id, buffer),
                            )
                        })
                        .iterate_internal_links_function(|pad, parent| {
                            RtpRecv::catch_panic_pad_function(
                                parent,
                                || gst::Iterator::from_vec(vec![]),
                                |this| this.iterate_internal_links(pad),
                            )
                        })
                        .name(format!("rtcp_sink_{id}"))
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
                        .get_or_insert_with(|| SharedRtpState::get_or_init(rtp_id));
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
                if let Some(session) = state.mut_session_by_id(id)
                    && session.rtp_recv_sinkpad.is_none()
                    && session.rtcp_recv_sinkpad.is_none()
                {
                    let id = session.internal_session.id;
                    state.sessions.retain(|s| s.internal_session.id != id);
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
                if let Err(err) = rtpbin2::get_or_init_runtime() {
                    self.post_error_message(gst::error_msg!(
                        gst::LibraryError::Settings,
                        ["Error initializing runtime: {err}"]
                    ));
                    return Err(gst::StateChangeError);
                }

                let settings = self.settings.lock().unwrap();
                let mut state = self.state.lock().unwrap();
                let rtp_id = settings.rtp_id.clone();
                let empty_sessions = state.sessions.is_empty();
                match state.shared_state.as_mut() {
                    Some(shared) => {
                        if !empty_sessions && shared.name() != rtp_id {
                            let other_name = shared.name();
                            drop(state);
                            self.post_error_message(gst::error_msg!(gst::LibraryError::Settings, ["rtp-id {rtp_id} does not match the currently set value {other_name}"]));
                            return Err(gst::StateChangeError);
                        }
                    }
                    None => {
                        state.shared_state = Some(SharedRtpState::get_or_init(rtp_id.clone()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtpbin2::{self, jitterbuffer::QueueResult};
    use RecvSessionSrcTaskCommand::*;
    use rtp_types::RtpPacket;
    use std::{sync::mpsc, thread::sleep, time::Duration};

    const LATENCY: Duration = Duration::from_millis(40);
    const PACKET_DURATION: Duration = Duration::from_millis(20);

    #[derive(Debug)]
    enum BufferOrList {
        Buffer(gst::Buffer),
        BufferList(gst::BufferList),
    }

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            gst::init().unwrap();
            rtpbin2::get_or_init_runtime().unwrap();
            let _ = env_logger::try_init();
        });
    }

    fn runtime<'a>() -> &'a tokio::runtime::Runtime {
        rtpbin2::get_or_init_runtime().unwrap()
    }

    fn make_link_recv_src_pad(
        session_id: usize,
        pt: u8,
        ssrc: u32,
    ) -> (RtpRecvSrcPad, gst::Pad, mpsc::Receiver<BufferOrList>) {
        let (buf_tx, buf_rx) = mpsc::sync_channel(0);
        let peer = gst::Pad::builder(gst::PadDirection::Sink)
            .chain_function({
                let buf_tx = buf_tx.clone();
                move |_, _, buf| {
                    if buf_tx.send(BufferOrList::Buffer(buf)).is_err() {
                        return Err(gst::FlowError::Flushing);
                    }
                    Ok(gst::FlowSuccess::Ok)
                }
            })
            .chain_list_function(move |_, _, list| {
                if buf_tx.send(BufferOrList::BufferList(list)).is_err() {
                    return Err(gst::FlowError::Flushing);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build();

        let rspad = RtpRecvSrcPad::new(
            pt,
            ssrc,
            gst::Pad::builder(gst::PadDirection::Src)
                .name(format!("rtp_src_{session_id}_{pt}_{ssrc}"))
                .build(),
            JitterBufferStore {
                waker: None,
                store: BTreeMap::new(),
                jitterbuffer: JitterBuffer::new(LATENCY),
            },
        );

        rspad.pad.link(&peer).unwrap();
        rspad.pad.set_active(true).unwrap();
        peer.set_active(true).unwrap();

        rspad
            .jitter_buffer_store
            .lock()
            .unwrap()
            .jitterbuffer
            .set_flushing(&rspad.pad.name(), false);

        (rspad, peer, buf_rx)
    }

    fn push_initial_events(session_id: usize, rspad: &RtpRecvSrcPad) {
        let mut jb_store = rspad.jitter_buffer_store.lock().unwrap();

        assert_eq!(
            jb_store
                .jitterbuffer
                .queue_serialized_item(&rspad.pad.name()),
            QueueResult::Forward {
                id: 0,
                discont: false
            }
        );
        assert!(
            rspad.pad.push_event(
                gst::event::StreamStart::builder(&format!(
                    "{session_id}_{}_{}",
                    rspad.pt, rspad.ssrc
                ))
                .build()
            )
        );
        assert_eq!(
            jb_store
                .jitterbuffer
                .queue_serialized_item(&rspad.pad.name()),
            QueueResult::Forward {
                id: 1,
                discont: false
            }
        );
        assert!(
            rspad
                .0
                .pad
                .push_event(gst::event::Segment::new(&gst::FormattedSegment::<
                    gst::format::Time,
                >::new()))
        );

        if let Some(waker) = jb_store.waker.take() {
            waker.wake()
        }
    }

    #[track_caller]
    fn queue_packet(rspad: &RtpRecvSrcPad, seq_no: u16, now: Instant) -> Result<u64, QueueResult> {
        let mut jb_store = rspad.jitter_buffer_store.lock().unwrap();

        let pts = PACKET_DURATION.as_nanos() as u64 * seq_no as u64;
        let rtp_ts = pts.mul_div_floor(90_000, *gst::ClockTime::SECOND).unwrap() as u32;
        let rtp_data =
            crate::rtpbin2::session::tests::generate_rtp_packet(rspad.ssrc, seq_no, rtp_ts, 4);
        let packet = RtpPacket::parse(&rtp_data).unwrap();
        let id = match jb_store
            .jitterbuffer
            .queue_packet(&rspad.pad.name(), &packet, pts, now)
        {
            QueueResult::Queued(id) => id,
            other => return Err(other),
        };

        let mut buf = gst::Buffer::from_mut_slice(rtp_data);
        let buf_mut = buf.make_mut();
        buf_mut.set_pts(gst::ClockTime::from_nseconds(pts));

        jb_store.store.insert(id, JitterBufferItem::Packet(buf));
        if let Some(waker) = jb_store.waker.take() {
            waker.wake()
        }

        Ok(pts)
    }

    #[test]
    fn recv_session_src_task_add_push_remove_stop() {
        const SESSION_ID: usize = 0;

        init();

        let (task, cmd_tx) =
            RecvSessionSrcTask::new(Arc::new(Mutex::new(gst_base::UniqueFlowCombiner::new())));

        let task_hdl = runtime().spawn(task.start());

        let (rspad, _peer, buf_rx) = make_link_recv_src_pad(SESSION_ID, 96, 1234);
        assert_eq!(Arc::strong_count(&rspad.0), 1);
        cmd_tx.unbounded_send(AddRecvSrcPad(rspad.clone())).unwrap();

        push_initial_events(SESSION_ID, &rspad);

        let mut now = Instant::now();
        let pts0 = queue_packet(&rspad, 0, now).unwrap();
        now += PACKET_DURATION;
        let pts1 = queue_packet(&rspad, 1, now).unwrap();

        match buf_rx.recv().unwrap() {
            BufferOrList::Buffer(buf) => {
                assert_eq!(buf.pts().unwrap().nseconds(), pts0);
                let BufferOrList::Buffer(buf) = buf_rx.recv().unwrap() else {
                    unreachable!();
                };
                assert_eq!(buf.pts().unwrap().nseconds(), pts1);
            }
            BufferOrList::BufferList(list) => {
                assert_eq!(list.len(), 2);
                assert_eq!(list.get(0).unwrap().pts().unwrap().nseconds(), pts0);
                assert_eq!(list.get(1).unwrap().pts().unwrap().nseconds(), pts1);
            }
        }
        let Err(mpsc::TryRecvError::Empty) = buf_rx.try_recv() else {
            unreachable!();
        };

        assert_eq!(Arc::strong_count(&rspad.0), 2);

        cmd_tx
            .unbounded_send(RemoveRecvSrcPad(rspad.clone()))
            .unwrap();

        while Arc::strong_count(&rspad.0) > 1 {
            sleep(Duration::from_millis(10));
        }

        cmd_tx.unbounded_send(Stop).unwrap();

        futures::executor::block_on(task_hdl).unwrap();
    }

    #[test]
    #[ignore] // See FIXME below
    fn recv_session_src_task_two_pads() {
        const SESSION_ID: usize = 1;

        init();

        let (task, cmd_tx) =
            RecvSessionSrcTask::new(Arc::new(Mutex::new(gst_base::UniqueFlowCombiner::new())));

        let task_hdl = runtime().spawn(task.start());

        let (rspad1, _peer1, buf_rx1) = make_link_recv_src_pad(SESSION_ID, 96, 2345);
        cmd_tx
            .unbounded_send(AddRecvSrcPad(rspad1.clone()))
            .unwrap();
        push_initial_events(SESSION_ID, &rspad1);

        let (rspad2, _peer2, buf_rx2) = make_link_recv_src_pad(SESSION_ID, 97, 3456);
        cmd_tx
            .unbounded_send(AddRecvSrcPad(rspad2.clone()))
            .unwrap();
        push_initial_events(SESSION_ID, &rspad2);

        let mut now = Instant::now();
        let pts10 = queue_packet(&rspad1, 0, now).unwrap();
        let pts20 = queue_packet(&rspad2, 0, now).unwrap();

        now += PACKET_DURATION;
        let pts11 = queue_packet(&rspad1, 1, now).unwrap();
        let pts21 = queue_packet(&rspad2, 1, now).unwrap();

        match buf_rx1.recv().unwrap() {
            BufferOrList::Buffer(buf) => {
                assert_eq!(buf.pts().unwrap().nseconds(), pts10);

                let BufferOrList::Buffer(buf) = buf_rx2.recv().unwrap() else {
                    unreachable!();
                };
                assert_eq!(buf.pts().unwrap().nseconds(), pts20);

                let BufferOrList::Buffer(buf) = buf_rx1.recv().unwrap() else {
                    unreachable!();
                };
                assert_eq!(buf.pts().unwrap().nseconds(), pts11);
            }
            BufferOrList::BufferList(list) => {
                assert_eq!(list.len(), 2);
                assert_eq!(list.get(0).unwrap().pts().unwrap().nseconds(), pts10);
                assert_eq!(list.get(1).unwrap().pts().unwrap().nseconds(), pts11);

                let BufferOrList::Buffer(buf) = buf_rx2.recv().unwrap() else {
                    unreachable!();
                };
                assert_eq!(buf.pts().unwrap().nseconds(), pts20);
            }
        }

        // packets 1 has reach its deadline
        // rspad2 is blocked => the recv session task is stuck in the blocking handler

        // rspad1 jb is empty => skip one packet for packets to be queued again
        // otherwise the jitter buffer will consider packet 2 can be forwarded
        now += 2 * PACKET_DURATION;
        let pts13 = queue_packet(&rspad1, 3, now).unwrap();
        let pts23 = queue_packet(&rspad2, 3, now).unwrap();

        now += PACKET_DURATION;
        let _pts14 = queue_packet(&rspad1, 4, now).unwrap();
        let pts24 = queue_packet(&rspad2, 4, now).unwrap();

        // wait for packets 3 to reach their deadlines
        sleep(LATENCY);

        // Unlock rspad2

        let BufferOrList::Buffer(buf) = buf_rx2.recv().unwrap() else {
            unreachable!();
        };
        assert_eq!(buf.pts().unwrap().nseconds(), pts21);

        // Blocking handler can pull packets 3

        // FIXME with slow hardware with low precision clock (e.g. Windows CI runner)
        //       we can get a BufferList here. Ignoring this test for now.
        let BufferOrList::Buffer(buf) = buf_rx1.recv().unwrap() else {
            unreachable!();
        };
        assert_eq!(buf.pts().unwrap().nseconds(), pts13);

        // Blocking handler is stuck waiting for rspad2 to complete pushing packet 3
        // Remove rspad1 before its packet 3 could be handled
        cmd_tx
            .unbounded_send(RemoveRecvSrcPad(rspad1.clone()))
            .unwrap();

        let BufferOrList::Buffer(buf) = buf_rx2.recv().unwrap() else {
            unreachable!();
        };
        assert_eq!(buf.pts().unwrap().nseconds(), pts23);

        // Back to the async handler

        let BufferOrList::Buffer(buf) = buf_rx2.recv().unwrap() else {
            unreachable!();
        };
        assert_eq!(buf.pts().unwrap().nseconds(), pts24);

        cmd_tx
            .unbounded_send(RemoveRecvSrcPad(rspad2.clone()))
            .unwrap();

        sleep(LATENCY);
        let Err(mpsc::TryRecvError::Empty) = buf_rx1.try_recv() else {
            unreachable!();
        };
        let Err(mpsc::TryRecvError::Empty) = buf_rx2.try_recv() else {
            unreachable!();
        };

        cmd_tx.unbounded_send(Stop).unwrap();

        futures::executor::block_on(task_hdl).unwrap();
    }

    #[test]
    // Because it involves the JitterBufferStream, this test heavily relies on timers
    // which don't play well on CI
    #[ignore]
    fn jitterbuffer_stream_deadline() {
        const SESSION_ID: usize = 0;

        init();

        let (task, cmd_tx) =
            RecvSessionSrcTask::new(Arc::new(Mutex::new(gst_base::UniqueFlowCombiner::new())));

        let task_hdl = runtime().spawn(task.start());

        let (rspad, _peer, buf_rx) = make_link_recv_src_pad(SESSION_ID, 96, 1234);
        assert_eq!(Arc::strong_count(&rspad.0), 1);
        gst::info!(super::CAT, "Add Pad command => init task + poll_next");
        cmd_tx.unbounded_send(AddRecvSrcPad(rspad.clone())).unwrap();

        std::thread::sleep(Duration::from_millis(20));

        gst::info!(super::CAT, "Pushing initial events");
        push_initial_events(SESSION_ID, &rspad);
        match buf_rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => (),
            other => unreachable!("{other:?}"),
        };

        let instant_packet0 = Instant::now();

        gst::info!(super::CAT, "Queuing seqnum 0 and waiting for deadline");
        let pts0 = queue_packet(&rspad, 0, instant_packet0).unwrap();

        let buf = match buf_rx.recv().unwrap() {
            BufferOrList::Buffer(buf) => buf,
            other => unreachable!("{other:?}"),
        };
        assert!((LATENCY..(LATENCY + PACKET_DURATION)).contains(&instant_packet0.elapsed()));
        assert_eq!(buf.pts().unwrap().nseconds(), pts0);
        match buf_rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => (),
            other => unreachable!("{other:?}"),
        };
        // note: didn't push seqnum 1 & 2 while waiting for seqnum 0 to be pushed out
        gst::info!(super::CAT, "=> got one packet with expected delay");

        std::thread::sleep(2 * PACKET_DURATION + PACKET_DURATION / 4);
        // note: didn't push seqnum 3 while sleeping for 3 * PACKET_DURATION
        gst::info!(
            super::CAT,
            "Queuing early seqnum 4 => jb stream should wake up and set its deadline"
        );
        let instant_packet4 = Instant::now();
        let pts4 = queue_packet(&rspad, 4, instant_packet4).unwrap();
        std::thread::sleep(Duration::from_millis(1));
        gst::info!(
            super::CAT,
            "Queuing earlier seqnum 3 => jb stream should wake up and update its deadline"
        );
        match buf_rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => (),
            other => unreachable!("{other:?}"),
        };
        let instant_packet3 = Instant::now();
        let pts3 = queue_packet(&rspad, 3, instant_packet3).unwrap();

        std::thread::sleep(PACKET_DURATION / 8);
        gst::info!(
            super::CAT,
            "Pushing earlier packets still on time to get accepted and pushed out immediately"
        );
        let pts1 = queue_packet(&rspad, 1, Instant::now()).unwrap();
        let pts2 = queue_packet(&rspad, 2, Instant::now()).unwrap();

        let buf_list = match buf_rx.recv().unwrap() {
            BufferOrList::BufferList(buf_list) => buf_list,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(buf_list.len(), 2);
        assert_eq!(buf_list.get(0).unwrap().pts().unwrap().nseconds(), pts1);
        assert_eq!(buf_list.get(1).unwrap().pts().unwrap().nseconds(), pts2);

        std::thread::sleep(PACKET_DURATION / 8);
        let buf = match buf_rx.recv().unwrap() {
            BufferOrList::Buffer(buf) => buf,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(buf.pts().unwrap().nseconds(), pts3);
        assert!(
            ((3 * PACKET_DURATION + LATENCY)..(4 * PACKET_DURATION + LATENCY))
                .contains(&instant_packet0.elapsed()),
            "{:?}",
            instant_packet0.elapsed()
        );

        let buf = match buf_rx.recv().unwrap() {
            BufferOrList::Buffer(buf) => buf,
            other => unreachable!("{other:?}"),
        };
        assert_eq!(buf.pts().unwrap().nseconds(), pts4);
        assert!(
            ((4 * PACKET_DURATION + LATENCY)..(5 * PACKET_DURATION + LATENCY))
                .contains(&instant_packet0.elapsed()),
            "{:?}",
            instant_packet0.elapsed()
        );
        gst::info!(super::CAT, "=> got packets in expected order");

        match buf_rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => (),
            other => unreachable!("{other:?}"),
        };

        cmd_tx
            .unbounded_send(RemoveRecvSrcPad(rspad.clone()))
            .unwrap();
        cmd_tx.unbounded_send(Stop).unwrap();

        futures::executor::block_on(task_hdl).unwrap();
    }
}
