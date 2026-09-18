// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(unused_doc_comments)]

/**
 * SECTION:element-sapsrc
 * @see_also: sapsink, sapdeviceprovider
 *
 * `sapsrc` is a network source that receives RTP sessions announced with the
 * Session Announcement Protocol (SAP). It can be combined with RTP
 * depayloaders to receive multicast RTP sessions without any out-of-band
 * configuration.
 *
 * SAP announcers periodically multicast an SDP session description to port
 * 9875 of the SAP multicast groups. `sapsrc` listens on the global and
 * administratively scoped SAP groups, and on the groups listed in the
 * comma-separated `GST_SAP_ADDRESSES` environment variable. For the first
 * matching session it sets up an RTP receiver for every receivable media
 * line and exposes them as dynamic source pads.
 *
 * Sessions are selected with the #GstSapSrc:session-id and
 * #GstSapSrc:originating-source properties, at least one of which has to be
 * set. #GstSapSrc:originating-source matches the originating source field of
 * the SAP packet: for a session announced by `sapsink` that is its
 * #GstSapSink:source-address or the address derived from it, so usually the
 * LAN address of the sender and not 127.0.0.1. The `sapdeviceprovider`
 * device provider can be used to discover all announced sessions and to
 * create a `sapsrc` for a selected one.
 *
 * The source pads are named `src_%u`, where `%u` is the index of the media
 * line in the SDP, and have `application/x-rtp` caps. They are sometimes
 * pads, so applications have to connect to the `pad-added` signal. The
 * stream ID of each pad is derived from the SDP origin and the media line.
 *
 * The internal `rtprecv` and `rtpsend` elements and the per-stream UDP
 * elements (`udpsrc2-rtp-<id>`, `udpsrc2-rtcp-<id>`, `udpsink2-rtcp-<id>`)
 * are added to the bin with stable names, depending on the RTCP mode, and can
 * be retrieved with gst_bin_get_by_name() or the #GstChildProxy interface.
 *
 * By default RTCP is not received. With #GstSapSrc:rtcp-mode set to
 * `recvonly` or `sendrecv`, RTCP is received on the port from the SDP, or on
 * RTP port + 1 if the SDP has no `a=rtcp` attribute. An RTCP BYE or a
 * source timeout sends EOS on the corresponding pad, but the pad is kept
 * and re-targeted if the stream restarts with a new source. If the
 * announced session is deleted or expires, EOS is sent on all pads, but
 * they are kept until the element is stopped or a session is announced,
 * which then removes them. If the announced session changes
 * incompatibly, EOS is sent on all pads and then all pads are removed
 * and rebuilt.
 *
 * The #GstSapSrc:latency property sets the jitter buffer latency, and
 * #GstSapSrc:timeout makes the element post a warning if no data is received
 * for the given time. Multicast reception can be restricted to specific
 * interfaces with #GstSapSrc:multicast-iface.
 *
 * By default multicast loopback is disabled, so local senders on the same
 * host do not receive the RTCP sent by this element. Set
 * #GstSapSrc:multicast-loopback to true for local testing. SAP announcements
 * are always looped back locally so that multiple SAP listeners on the same
 * host can see each other.
 *
 * ## Examples
 * |[
 * gst-launch-1.0 -v sapsrc multicast-loopback=true session-id=1234 ! rtpopusdepay2 ! opusdec ! autoaudiosink
 * ]| A pipeline to receive an Opus session announced with
 * `sapsink session-id=1234`, e.g. by the `sap-send` example.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use anyhow::{Context, bail};

use sdp_types::Direction;

use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::{Arc, LazyLock, Mutex, mpsc},
};

use crate::common::{
    RtcpMode, SinkSocket, SrcSocket, ensure_required_elements, make_queue, make_udpsink2,
    make_udpsrc2, next_rtp_id, remove_element, set_pt_map, teardown_chain, used_socket,
};
use crate::listener::{ListenerHandle, Session as SapSession, SessionEvent, SessionKey};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new("sapsrc", gst::DebugColorFlags::empty(), Some("SAP Source"))
});

// RTP jitterbuffer latency in ms
const DEFAULT_LATENCY: u32 = 200;

// Factories needed while running, checked up front in change_state() so
// that instantiating elements later can not fail
const REQUIRED_ELEMENTS: &[&str] = &["queue", "rtprecv", "rtpsend", "udpsrc2", "udpsink2"];

#[derive(Clone, Debug)]
struct Settings {
    session_id: Option<String>,
    originating_source: Option<String>,
    multicast_iface: Option<String>,
    multicast_loopback: bool,
    latency: u32,
    rtcp_mode: RtcpMode,
    timeout: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            session_id: None,
            originating_source: None,
            multicast_iface: None,
            multicast_loopback: false,
            latency: DEFAULT_LATENCY,
            rtcp_mode: RtcpMode::default(),
            timeout: 0,
        }
    }
}

/// State of a single media stream of a session
#[derive(Default)]
struct Stream {
    // udpsrc2 / queue / udpsink2 elements, in link order
    elements: Vec<gst::Element>,
    // rtprecv / rtpsend and their pads requested for this stream
    request_pads: Vec<(gst::Element, gst::Pad)>,
    // Caps of the media line from the SDP, stored on the ghost pad
    caps: Option<gst::Caps>,
    // Single src pad of the media line, re-targeted when the RTP source changes
    ghost: Option<gst::GhostPad>,
    // Payload type and SSRC of the current rtp_src target pad
    target: Option<(u32, u32)>,
}

/// State of a single SAP session
struct SessionEntry {
    // SDP of the session, used to detect incompatible session updates
    sdp: sdp_types_gst::Session,
    // Common prefix of the stream IDs of all pads, derived from the SDP
    stream_id_base: String,
    // Group ID shared by all pads of the session, created on first use
    group_id: Option<gst::GroupId>,
    // Whether NO_MORE_PADS was emitted for this session
    no_more_pads: bool,
    // Streams by rtp_sink_N / rtcp_sink_N / rtcp_src_N stream id
    streams: HashMap<u32, Stream>,
    // Session deleted or expired: EOS was sent on all pads, which are kept
    // until the next session announcement or a stop
    dead: bool,
}

// Session events have to be processed asynchronously and in order but
// without a thread or main context owned by the element: events are queued
// and drained in order by a single task on the element call pool. quit()
// waits for the queue to be processed like joining a thread would.
enum Event {
    Session(Box<SapSession>, SessionEvent),
    Quit(mpsc::Sender<()>),
}

#[derive(Default)]
struct QueueState {
    events: VecDeque<Event>,
    draining: bool,
    dead: bool,
}

struct EventQueue {
    state: Mutex<QueueState>,
}

impl EventQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
        }
    }

    fn push(self: &Arc<Self>, obj: &super::SapSrc, event: Event) {
        let mut state = self.state.lock().unwrap();
        if state.dead {
            return;
        }
        // Only a single drain task runs at a time, it picks up events that
        // are queued while it runs
        let drain = !state.draining;
        state.draining = true;
        state.events.push_back(event);
        drop(state);

        if drain {
            // Weak reference so that the task does nothing if the queue was
            // freed by stop() in the meantime, e.g. after a restart
            let queue = Arc::downgrade(self);
            obj.call_async(move |obj| {
                let Some(queue) = queue.upgrade() else {
                    return;
                };
                queue.drain(obj);
            });
        }
    }

    // Wait until queued and running event handlers are done, drop later events
    fn quit(&self) {
        let (ack, ack_rx) = mpsc::channel();

        let mut state = self.state.lock().unwrap();
        if state.dead {
            return;
        }
        // Without a drain task nothing is in flight and the queue is empty
        if !state.draining {
            state.dead = true;
            return;
        }
        state.events.push_back(Event::Quit(ack));
        drop(state);

        let _ = ack_rx.recv();
    }

    fn drain(&self, obj: &super::SapSrc) {
        loop {
            let event = {
                let mut state = self.state.lock().unwrap();
                let Some(event) = state.events.pop_front() else {
                    state.draining = false;
                    return;
                };
                event
            };

            match event {
                Event::Session(session, session_event) => {
                    obj.imp().handle_session_event(&session, session_event);
                }
                Event::Quit(ack) => {
                    let mut state = self.state.lock().unwrap();
                    state.dead = true;
                    state.events.clear();
                    drop(state);
                    let _ = ack.send(());
                    return;
                }
            }
        }
    }
}

#[derive(Default)]
struct State {
    listener: Option<ListenerHandle>,
    // Session events are processed in order by an async task
    events: Option<Arc<EventQueue>>,
    rtp_id: Option<String>,
    rtprecv: Option<gst::Element>,
    rtpsend: Option<gst::Element>,
    // A single element instance handles at most one session at a time
    session: Option<(SessionKey, SessionEntry)>,
    // Aggregates the flow returns of all ghost pads
    flow_combiner: gst_base::UniqueFlowCombiner,
    // Whether a timeout warning was posted, not reset on stop
    warned_timeout: bool,
}

#[derive(Default)]
pub struct SapSrc {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for SapSrc {
    const NAME: &'static str = "GstSapSrc";
    type Type = super::SapSrc;
    type ParentType = gst::Bin;
    type Interfaces = (gst::ChildProxy,);
}

impl ObjectImpl for SapSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("session-id")
                    .nick("Session ID")
                    .blurb("Session ID to filter for, i.e. the session ID of the SDP o= line")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("originating-source")
                    .nick("Originating Source")
                    .blurb("Originating source address to filter for, i.e. the IP address of the SAP announcer")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("multicast-iface")
                    .nick("Multicast Interface")
                    .blurb("Comma-separated list of network interfaces to use for multicast")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("multicast-loopback")
                    .nick("Multicast Loopback")
                    .blurb("Enable multicast loopback so that local senders on the same host receive RTCP; needed for local testing")
                    .default_value(false)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("latency")
                    .nick("Latency")
                    .blurb("RTP jitterbuffer latency in ms")
                    .default_value(DEFAULT_LATENCY)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("rtcp-mode", Settings::default().rtcp_mode)
                    .nick("RTCP Mode")
                    .blurb("Whether to send and/or receive RTCP")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("timeout")
                    .nick("Timeout")
                    .blurb("Post a warning after this many nanoseconds without receiving data (0 = disabled)")
                    .default_value(0)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        self.obj()
            .set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);
        self.obj().set_element_flags(gst::ElementFlags::SOURCE);
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "session-id" => {
                let v = value.get::<Option<&str>>().expect("type checked upstream");
                self.settings.lock().unwrap().session_id =
                    v.filter(|s| !s.is_empty()).map(str::to_string);
            }
            "originating-source" => {
                let v = value.get::<Option<&str>>().expect("type checked upstream");
                self.settings.lock().unwrap().originating_source =
                    v.filter(|s| !s.is_empty()).map(str::to_string);
            }
            "multicast-iface" => {
                let v = value.get::<Option<&str>>().expect("type checked upstream");
                self.settings.lock().unwrap().multicast_iface =
                    v.filter(|s| !s.is_empty()).map(str::to_string);
            }
            "multicast-loopback" => {
                self.settings.lock().unwrap().multicast_loopback =
                    value.get().expect("type checked upstream");
            }
            "latency" => {
                self.settings.lock().unwrap().latency = value.get().expect("type checked upstream");
            }
            "rtcp-mode" => {
                self.settings.lock().unwrap().rtcp_mode =
                    value.get().expect("type checked upstream");
            }
            "timeout" => {
                self.settings.lock().unwrap().timeout = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "session-id" => settings.session_id.as_ref().to_value(),
            "originating-source" => settings.originating_source.as_ref().to_value(),
            "multicast-iface" => settings.multicast_iface.as_ref().to_value(),
            "multicast-loopback" => settings.multicast_loopback.to_value(),
            "latency" => settings.latency.to_value(),
            "rtcp-mode" => settings.rtcp_mode.to_value(),
            "timeout" => settings.timeout.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for SapSrc {}

impl ElementImpl for SapSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "SAP Source",
                "Source/Network",
                "Session Announcement Protocol source",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::new_empty_simple("application/x-rtp");
            let src_pad_template = gst::PadTemplate::new(
                "src_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &caps,
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let mut ret = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::NullToReady => {
                ensure_required_elements(&*self.obj(), REQUIRED_ELEMENTS)?;
            }
            gst::StateChange::ReadyToPaused => {
                if let Err(err) = self.start() {
                    gst::error!(
                        CAT,
                        obj = &self.obj(),
                        "Failed listening for SAP sessions: {err}"
                    );
                    gst::element_error!(
                        self.obj(),
                        gst::CoreError::Failed,
                        ("Failed listening for SAP sessions: {err}")
                    );
                    return Err(gst::StateChangeError);
                }
                ret = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PlayingToPaused => {
                ret = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToReady => {
                self.stop();
            }
            _ => (),
        }

        Ok(ret)
    }
}

impl SapSrc {
    fn start(&self) -> Result<(), anyhow::Error> {
        let res = self.start_inner();
        if res.is_err() {
            self.stop();
        }
        res
    }

    fn start_inner(&self) -> Result<(), anyhow::Error> {
        let obj = self.obj();
        gst::debug!(CAT, obj = &obj, "Preparing SAP session listener");
        let settings = self.settings.lock().unwrap().clone();

        if settings.session_id.is_none() && settings.originating_source.is_none() {
            bail!("Either session-id or originating-source has to be configured for filtering");
        }

        let originating_source = settings
            .originating_source
            .as_deref()
            .map(str::parse::<IpAddr>)
            .transpose()
            .context("Invalid originating-source address")?;

        if settings.rtcp_mode.send() && !settings.rtcp_mode.receive() {
            bail!("rtcp-mode=sendonly is not supported, receiving RTCP is required");
        }

        let rtp_id = next_rtp_id("sapsrc-");
        let rtprecv = gst::ElementFactory::make("rtprecv")
            .name("rtprecv")
            .property("rtp-id", &rtp_id)
            .property("latency", settings.latency)
            .build()
            .unwrap();
        rtprecv.connect_pad_added(glib::clone!(
            #[weak(rename_to = obj)]
            self.obj(),
            move |_rtprecv, pad| {
                obj.imp().handle_rtp_src_pad_added(pad);
            }
        ));
        self.obj().add(&rtprecv).unwrap();
        rtprecv.sync_state_with_parent().unwrap();
        {
            let mut state = self.state.lock().unwrap();
            state.rtp_id = Some(rtp_id);
            state.rtprecv = Some(rtprecv);
        }

        let listener = ListenerHandle::acquire()?;
        let session_id = settings.session_id;

        // The listener callback has to return immediately and calling into the
        // element could deadlock, so events are queued to a drain task.
        let events = Arc::new(EventQueue::new());
        let events_cb = Arc::clone(&events);

        // Stored before the callback is registered: listen() replays known
        // sessions, and the drain task checks state.events for a stop.
        self.state.lock().unwrap().events = Some(events);

        let obj_weak = self.obj().downgrade();
        listener.listen(move |session, event| {
            if let Some(session_id) = &session_id
                && session.sdp.origin().sess_id != *session_id
            {
                return;
            }
            if let Some(originating_source) = &originating_source
                && session.originating_source != *originating_source
            {
                return;
            }

            gst::info!(
                CAT,
                "SAP session {} ({}) {event:?}",
                session.sdp.origin().sess_id,
                session.sdp.name()
            );

            let Some(obj) = obj_weak.upgrade() else {
                return;
            };
            events_cb.push(&obj, Event::Session(Box::new(session.clone()), event));
        });

        // Stored only now that the callback is registered, see above
        self.state.lock().unwrap().listener = Some(listener);

        gst::debug!(CAT, obj = &obj, "Listening for SAP sessions");

        Ok(())
    }

    fn stop(&self) {
        let obj = self.obj();
        gst::debug!(CAT, obj = &obj, "Stopping SAP session listener");

        let (listener, events, elements) = {
            let mut state = self.state.lock().unwrap();
            state.rtp_id = None;
            (
                state.listener.take(),
                state.events.take(),
                [state.rtpsend.take(), state.rtprecv.take()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>(),
            )
        };

        // The listener callbacks only queue events. Quitting waits until all
        // queued events are processed and no event handler is running.
        if listener.is_some() {
            gst::info!(CAT, obj = &obj, "Stopped listening for SAP sessions");
        }
        drop(listener);
        if let Some(events) = events {
            events.quit();
        }

        self.teardown_session(false);
        self.state.lock().unwrap().flow_combiner.reset();

        for element in elements {
            remove_element(&*obj, &element);
        }
    }

    fn handle_session_event(&self, session: &SapSession, event: SessionEvent) {
        let mut end = false;
        let mut teardown = false;
        let mut build = false;
        {
            let state = self.state.lock().unwrap();

            // Do nothing if stopped in the meantime. state.events instead
            // of the listener: it is stored before the callback is
            // registered in start(), and both are taken by stop() at the
            // same time.
            if state.events.is_none() {
                return;
            }

            let key = session.key();
            let sess_id = &session.sdp.origin().sess_id;

            // Only one session at a time is handled: an ended (dead) one is
            // replaced by the next announcement, a live one is never given
            // up.
            let current = state
                .session
                .as_ref()
                .map(|(key, entry)| (key.clone(), entry.dead));
            match event {
                SessionEvent::Added => match &current {
                    None => {
                        // Building must not happen under the state mutex, see
                        // build_session().
                        build = true;
                    }
                    // Always rebuild after an ended session, also when it is
                    // announced again: the old pads would clash with the new
                    // ones, and apps see the restart via new pads.
                    Some((_, true)) => {
                        gst::info!(
                            CAT,
                            obj = &self.obj(),
                            "SAP session {sess_id} was announced after the current session \
                             ended, removing its streams"
                        );
                        teardown = true;
                        build = true;
                    }
                    Some((_, false)) => {
                        gst::warning!(
                            CAT,
                            obj = &self.obj(),
                            "Already handling a session, ignoring new SAP session {sess_id}"
                        );
                    }
                },
                SessionEvent::Updated => {
                    if current.as_ref().map(|(k, _)| k) != Some(&key) {
                        return;
                    }
                    let equivalent = state
                        .session
                        .as_ref()
                        .is_some_and(|(_key, entry)| sdp_equivalent(&entry.sdp, &session.sdp));
                    if equivalent {
                        gst::debug!(
                            CAT,
                            obj = &self.obj(),
                            "Ignoring SAP session update for {sess_id}, no relevant changes"
                        );
                        return;
                    }

                    // TODO: Support updating sessions in-place
                    gst::warning!(
                        CAT,
                        obj = &self.obj(),
                        "SAP session {sess_id} was updated incompatibly, \
                         sending EOS and rebuilding all its streams"
                    );
                    teardown = true;
                    build = true;
                }
                SessionEvent::Removed | SessionEvent::Expired => {
                    if current.as_ref().map(|(k, _)| k) != Some(&key) {
                        return;
                    }
                    gst::debug!(
                        CAT,
                        obj = &self.obj(),
                        "SAP session {sess_id} {event:?}, sending EOS on all its pads"
                    );
                    end = true;
                }
            }
        }

        if end {
            self.end_session();
        }
        if teardown {
            self.teardown_session(true);
        }
        // The updated session stays in the listener cache, so no new Added
        // event arrives: its streams have to be rebuilt here.
        if build && let Err(err) = self.build_session(session) {
            gst::error!(
                CAT,
                obj = &self.obj(),
                "Failed to handle SAP session {}: {err:#}",
                session.sdp.origin().sess_id
            );
        }
    }

    /// Builds the streams of a new session. Only the event queue drain task
    /// calls this, so no other session event can be handled concurrently.
    ///
    /// The session is inserted before any element is synced to the parent
    /// state, so handle_rtp_src_pad_added() finds it when data starts flowing.
    fn build_session(&self, session: &SapSession) -> Result<(), anyhow::Error> {
        let obj = self.obj();
        let settings = self.settings.lock().unwrap().clone();

        let (rtprecv, rtp_id, mut rtpsend) = {
            let state = self.state.lock().unwrap();
            let rtprecv = state.rtprecv.clone().context("rtprecv not set up")?;
            // Stored together with the rtprecv in start()
            let rtp_id = state.rtp_id.clone().expect("rtp-id set with rtprecv");
            (rtprecv, rtp_id, state.rtpsend.clone())
        };
        let rtpsend_existed = rtpsend.is_some();

        let stream_id_base = sdp_stream_id_base(session.sdp.origin());
        let mut entry = SessionEntry {
            sdp: session.sdp.clone(),
            stream_id_base,
            group_id: None,
            no_more_pads: false,
            streams: Default::default(),
            dead: false,
        };

        // Elements of every stream, synced after the session is inserted into
        // the state
        let mut activate = Vec::new();

        // TODO: support BUNDLE groups (RFC 8843), the tagged medias share one
        // transport instead of every media getting its own sockets
        for (mline, media) in session.sdp.media().iter().enumerate() {
            match self.build_stream(
                &rtprecv,
                &mut rtpsend,
                &rtp_id,
                &settings,
                session,
                media,
                mline,
            ) {
                Ok(Some((stream_id, stream))) => {
                    activate.push((stream_id, stream.elements.clone()));
                    entry.streams.insert(stream_id, stream);
                }
                Ok(None) => (),
                Err(err) => {
                    gst::error!(
                        CAT,
                        obj = &obj,
                        "Failed to handle media {} of SAP session {}: {err:#}",
                        mline,
                        session.sdp.origin().sess_id
                    );
                }
            }
        }

        if entry.streams.is_empty() {
            // Storing a session without streams would block the element
            // forever, so leave it free for another session instead.
            gst::warning!(
                CAT,
                obj = &obj,
                "SAP session {} has no receivable media",
                session.sdp.origin().sess_id
            );

            // An rtpsend created for one of the medias is kept for the next
            // session, unless the element was stopped in the meantime.
            let stopped = {
                let mut state = self.state.lock().unwrap();
                if state.events.is_some() {
                    state.rtpsend = rtpsend.clone();
                    false
                } else {
                    true
                }
            };
            if stopped {
                self.teardown_unbuilt_session(entry, rtpsend, rtpsend_existed);
            }

            obj.no_more_pads();
            return Ok(());
        }

        // A stopped element must not get a session: stop() would never tear
        // it down.
        if let Some(entry) = self.try_insert_session(session, entry, &rtpsend) {
            self.teardown_unbuilt_session(entry, rtpsend, rtpsend_existed);
            return Ok(());
        }

        // Sync only now that everything is linked, in reverse order so that
        // sources with running tasks start last and do not error out.
        for (stream_id, elements) in activate {
            for element in elements.iter().rev() {
                element.sync_state_with_parent().unwrap();
            }

            // Watch the rtprecv session for ended SSRCs. The handlers go away
            // with the session object when the stream is torn down.
            let rtp_session = rtprecv.emit_by_name::<glib::Object>("get-session", &[&stream_id]);
            for signal in ["bye-ssrc", "timeout-ssrc"] {
                rtp_session.connect_closure(
                    signal,
                    false,
                    glib::closure!(
                        #[weak]
                        obj,
                        #[strong]
                        stream_id,
                        #[upgrade_or_default]
                        move |_session: &glib::Object, ssrc: u32| {
                            obj.imp().handle_ssrc_ended(stream_id, ssrc);
                        }
                    ),
                );
            }
        }

        // Bring rtpsend to the parent state once its downstream elements are
        // synced.
        if let Some(rtpsend) = rtpsend.as_ref() {
            rtpsend.sync_state_with_parent().unwrap();
        }

        Ok(())
    }

    /// Inserts a built session into the state. Returns the entry back if the
    /// element was stopped in the meantime.
    fn try_insert_session(
        &self,
        session: &SapSession,
        entry: SessionEntry,
        rtpsend: &Option<gst::Element>,
    ) -> Option<SessionEntry> {
        let mut state = self.state.lock().unwrap();
        if state.events.is_none() {
            return Some(entry);
        }

        state.session = Some((session.key(), entry));
        state.rtpsend = rtpsend.clone();

        None
    }

    /// Tears down a session built while the element was stopped: stop() has
    /// already run and will not remove it. No element was synced, so there
    /// are no ghost pads. Must be called without the state mutex held.
    fn teardown_unbuilt_session(
        &self,
        entry: SessionEntry,
        rtpsend: Option<gst::Element>,
        rtpsend_existed: bool,
    ) {
        for (_stream_id, mut stream) in entry.streams {
            self.teardown_stream(&mut stream);
        }

        // An rtpsend created while building is not in the state, so stop() did
        // not tear it down. One that existed before is handled by stop().
        if let Some(rtpsend) = rtpsend.filter(|_| !rtpsend_existed) {
            remove_element(&*self.obj(), &rtpsend);
        }
    }

    /// Sets up a single media line of a session and returns its stream and
    /// stream id.
    #[allow(clippy::too_many_arguments)]
    fn build_stream(
        &self,
        rtprecv: &gst::Element,
        rtpsend: &mut Option<gst::Element>,
        rtp_id: &str,
        settings: &Settings,
        session: &SapSession,
        media: &sdp_types_gst::Media,
        mline: usize,
    ) -> Result<Option<(u32, Stream)>, anyhow::Error> {
        let obj = self.obj();

        let direction = session.sdp.rtp_direction_for_mline(mline);
        if matches!(direction, Direction::RecvOnly | Direction::Inactive) {
            gst::warning!(
                CAT,
                obj = &obj,
                "Media {mline} of SAP session {} is {direction:?}, not receivable, skipping",
                session.sdp.origin().sess_id
            );
            return Ok(None);
        }

        // A port of 0 means that the media stream is not available
        // (RFC 3261 §9.2)
        if media.port() == 0 {
            gst::warning!(
                CAT,
                obj = &obj,
                "Media {mline} of SAP session {} has port 0, i.e. no media stream, skipping",
                session.sdp.origin().sess_id
            );
            return Ok(None);
        }

        let dest = media
            .connection()
            .or(session.sdp.connection())
            .and_then(|connection| connection.try_address().ok())
            .map(|address| address.ip);
        let Some(dest) = dest else {
            gst::warning!(
                CAT,
                obj = &obj,
                "Media {mline} of SAP session {} has no connection address, skipping",
                session.sdp.origin().sess_id
            );
            return Ok(None);
        };

        let caps = sdp_types_gst::media_to_caps(media).with_context(|| {
            format!(
                "Failed to convert caps of media {mline} of SAP session {}",
                session.sdp.origin().sess_id
            )
        })?;

        let mut stream = Stream {
            caps: Some(caps.clone()),
            ..Default::default()
        };

        // RFC 4570 source filter, applied to the UDP sources
        let source_filter = source_filter(&session.sdp, media, dest);

        // Stream ids are the media line indexes of the session
        let stream_id = mline as u32;

        // On failure the partially built stream is torn down again, otherwise
        // its request pads are never released and requesting them again for a
        // later session would fail.
        let mut build = || -> Result<(), anyhow::Error> {
            // RTP reception: udpsrc2 -> queue -> rtprecv
            let rtp_sink_pad = rtprecv
                .request_pad_simple(&format!("rtp_sink_{stream_id}"))
                .expect("rtp_sink pad requested");
            stream.request_pads.push((rtprecv.clone(), rtp_sink_pad));

            set_pt_map(rtprecv, stream_id, &caps);

            let rtp_src = make_udpsrc2(
                &format!("udpsrc2-rtp-{stream_id}"),
                SrcSocket::Bind {
                    address: dest,
                    port: u32::from(media.port()),
                    timeout: settings.timeout,
                },
                Some(&caps),
                settings.multicast_iface.as_deref(),
                source_filter.as_ref().map(|(f, e)| (f.as_str(), *e)),
            )?;
            obj.add(&rtp_src).unwrap();
            stream.elements.push(rtp_src.clone());

            let queue_rtp = make_queue(&format!("queue-rtp-{stream_id}"));
            obj.add(&queue_rtp).unwrap();
            stream.elements.push(queue_rtp.clone());
            rtp_src.link(&queue_rtp).expect("rtp src linked to queue");
            queue_rtp
                .link_pads(Some("src"), rtprecv, Some(&format!("rtp_sink_{stream_id}")))
                .expect("queue linked to rtprecv");

            // RTCP is received on the port announced via a=rtcp for multicast
            // sessions, where it is the RTCP port of the whole session
            // (RFC 3550 §6), and on RTP port + 1 otherwise (RFC 3550 §11)
            let rtcp_rx_port = match media.rtcp() {
                Some(rtcp) if dest.is_multicast() => Some(rtcp.port),
                _ => media.port().checked_add(1),
            };
            // Send RTCP to the a=rtcp port if present, otherwise to the port
            // RTCP is received on (RFC 3605)
            let rtcp_tx_port = media.rtcp().map(|rtcp| rtcp.port).or(rtcp_rx_port);

            let rtcp_udpsrc = if settings.rtcp_mode.receive() && !media.rtcp_mux() {
                let rtcp_rx_port = rtcp_rx_port.context("RTP port 65535 has no RTCP port")?;

                // RTCP reception: udpsrc2 -> rtprecv. With a=rtcp-mux RTCP is
                // demuxed from the RTP port by rtprecv itself (RFC 5761).
                let rtcp_sink_pad = rtprecv
                    .request_pad_simple(&format!("rtcp_sink_{stream_id}"))
                    .expect("rtcp_sink pad requested");
                stream.request_pads.push((rtprecv.clone(), rtcp_sink_pad));

                let rtcp_src = make_udpsrc2(
                    &format!("udpsrc2-rtcp-{stream_id}"),
                    SrcSocket::Bind {
                        address: dest,
                        port: u32::from(rtcp_rx_port),
                        timeout: settings.timeout,
                    },
                    Some(&gst::Caps::new_empty_simple("application/x-rtcp")),
                    settings.multicast_iface.as_deref(),
                    source_filter.as_ref().map(|(f, e)| (f.as_str(), *e)),
                )?;
                obj.add(&rtcp_src).unwrap();
                stream.elements.push(rtcp_src.clone());

                rtcp_src
                    .link_pads(
                        Some("src"),
                        rtprecv,
                        Some(&format!("rtcp_sink_{stream_id}")),
                    )
                    .expect("rtcp src linked to rtprecv");

                Some(rtcp_src)
            } else {
                None
            };

            if settings.rtcp_mode.send() {
                // RFC 4585 feedback is signalled per media via the transport
                // profile, rtpsend has a single profile for the whole element
                let avpf = session
                    .sdp
                    .media()
                    .iter()
                    .any(|media| media.proto().is_avpf());
                let rtpsend = self.get_or_create_rtpsend(rtpsend, rtp_id, avpf);

                let rtcp_src_pad = rtpsend
                    .request_pad_simple(&format!("rtcp_src_{stream_id}"))
                    .expect("rtcp_src pad requested");
                stream.request_pads.push((rtpsend.clone(), rtcp_src_pad));

                set_pt_map(&rtpsend, stream_id, &caps);

                // RTCP destination as per RFC 3550 §3/§11, RFC 3605 and
                // RFC 5761
                let (rtcp_dest, rtcp_dest_port) = if media.rtcp_mux() {
                    (dest, media.port())
                } else {
                    // The a=rtcp address if given, otherwise the same
                    // destination address as RTP: for multicast sessions RTCP
                    // is sent to the same group so that all participants
                    // receive it (RFC 3550 §6)
                    let rtcp_dest = media
                        .rtcp()
                        .and_then(|rtcp| match rtcp.try_address() {
                            Some(Ok(address)) => Some(address.ip),
                            Some(Err(err)) => {
                                gst::warning!(
                                    CAT,
                                    obj = &obj,
                                    "Ignoring invalid a=rtcp address of media {mline}: {err}"
                                );
                                None
                            }
                            None => None,
                        })
                        .unwrap_or(dest);
                    let rtcp_dest_port = rtcp_tx_port.context("RTP port 65535 has no RTCP port")?;
                    (rtcp_dest, rtcp_dest_port)
                };

                let rtcp_socket_udpsrc = if media.rtcp_mux() {
                    &rtp_src
                } else {
                    rtcp_udpsrc.as_ref().context(
                        "RTCP reception is required for sending RTCP when not using a=rtcp-mux",
                    )?
                };
                let socket = used_socket(rtcp_socket_udpsrc)?;

                let rtcp_sink = make_udpsink2(
                    &format!("udpsink2-rtcp-{stream_id}"),
                    rtcp_dest,
                    rtcp_dest_port,
                    SinkSocket::Shared(&socket),
                    settings.multicast_iface.as_deref(),
                    settings.multicast_loopback,
                )?;
                obj.add(&rtcp_sink).unwrap();
                stream.elements.push(rtcp_sink.clone());

                rtpsend
                    .link_pads(
                        Some(&format!("rtcp_src_{stream_id}")),
                        &rtcp_sink,
                        Some("sink"),
                    )
                    .expect("rtcp_src linked to udpsink2");
            }

            Ok(())
        };
        if let Err(err) = build() {
            self.teardown_stream(&mut stream);
            return Err(err);
        }

        Ok(Some((stream_id, stream)))
    }

    /// Tears down the request pads and elements of a stream. Must be called
    /// without the state mutex held.
    fn teardown_stream(&self, stream: &mut Stream) {
        teardown_chain(
            &*self.obj(),
            (
                std::mem::take(&mut stream.request_pads),
                std::mem::take(&mut stream.elements),
            ),
        );
    }

    fn get_or_create_rtpsend(
        &self,
        rtpsend: &mut Option<gst::Element>,
        rtp_id: &str,
        avpf: bool,
    ) -> gst::Element {
        if let Some(rtpsend) = rtpsend.as_ref() {
            return rtpsend.clone();
        }

        let element = gst::ElementFactory::make("rtpsend")
            .name("rtpsend")
            .property("rtp-id", rtp_id)
            .property_from_str("rtp-profile", if avpf { "avpf" } else { "avp" })
            .build()
            .unwrap();
        self.obj().add(&element).unwrap();
        // READY initializes the runtime and shared state needed by the
        // rtcp_src pad requested by the caller. The parent state is synced
        // later with the stream elements.
        element.set_state(gst::State::Ready).unwrap();
        *rtpsend = Some(element.clone());

        element
    }

    // Updates the flow of a ghost pad, ignoring not-linked errors until all
    // pads were added: downstream might only link the pads it is interested in.
    fn update_pad_flow(
        &self,
        pad: &gst::ProxyPad,
        ret: Result<gst::FlowSuccess, gst::FlowError>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (ret, no_more_pads) = {
            let mut state = self.state.lock().unwrap();
            (
                state.flow_combiner.update_pad_flow(pad, ret),
                state
                    .session
                    .as_ref()
                    .is_some_and(|(_, entry)| entry.no_more_pads),
            )
        };

        if let Err(gst::FlowError::NotLinked) = ret
            && !no_more_pads
        {
            return Ok(gst::FlowSuccess::Ok);
        }

        ret
    }

    fn proxy_pad_chain(
        &self,
        pad: &gst::ProxyPad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let ret = gst::ProxyPad::chain_default(pad, Some(&*self.obj()), buffer);
        self.update_pad_flow(pad, ret)
    }

    fn proxy_pad_chain_list(
        &self,
        pad: &gst::ProxyPad,
        bufferlist: gst::BufferList,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let ret = gst::ProxyPad::chain_list_default(pad, Some(&*self.obj()), bufferlist);
        self.update_pad_flow(pad, ret)
    }

    /// Replaces the stream-start and caps events of the RTP source, which are
    /// derived from the RTP headers, with the ones stored on the ghost pad.
    fn proxy_pad_event(
        &self,
        stream_id: &str,
        group_id: gst::GroupId,
        caps: &gst::Caps,
        pad: &gst::ProxyPad,
        mut event: gst::Event,
    ) -> bool {
        match event.view() {
            gst::EventView::StreamStart(stream_start) => {
                event = gst::event::StreamStart::builder(stream_id)
                    .group_id(group_id)
                    .seqnum(stream_start.seqnum())
                    .build();
            }
            gst::EventView::Caps(caps_event) => {
                event = gst::event::Caps::builder(caps)
                    .seqnum(caps_event.seqnum())
                    .build();
            }
            _ => (),
        }

        gst::Pad::event_default(pad, Some(&*self.obj()), event)
    }

    /// Adds a ghost pad for a new rtp_src_<stream>_<pt>_<ssrc> pad of
    /// rtprecv, or re-targets the existing one to the new RTP source.
    fn handle_rtp_src_pad_added(&self, pad: &gst::Pad) {
        let Some((stream_id, pt, ssrc)) = parse_rtp_src_pad_name(pad.name().as_str()) else {
            return;
        };

        let obj = self.obj();
        let (ghost, all_pads_added) = {
            let mut state = self.state.lock().unwrap();
            // Stopped in the meantime, see handle_session_event().
            if state.events.is_none() {
                return;
            }

            let Some((_key, entry)) = state.session.as_mut() else {
                return;
            };
            let Some(stream) = entry.streams.get_mut(&stream_id) else {
                gst::debug!(
                    CAT,
                    obj = &obj,
                    "Ignoring pad {} of unknown stream {stream_id}",
                    pad.name()
                );
                return;
            };

            // rtprecv creates a source pad for whatever it receives, spurious
            // payload types that are not in the SDP are not exposed
            if !entry
                .sdp
                .media()
                .get(stream_id as usize)
                .is_some_and(|media| media.formats().iter().any(|&fmt| fmt as u32 == pt))
            {
                gst::debug!(
                    CAT,
                    obj = &obj,
                    "Ignoring pad {} of stream {stream_id} with payload type {pt} not in the SDP",
                    pad.name()
                );
                return;
            }

            if let Some(ghost) = &stream.ghost {
                // The RTP source changed: re-target the pad, keeping its sticky events.
                gst::debug!(
                    CAT,
                    obj = &obj,
                    "Re-targeting ghost pad {} to {}",
                    ghost.name(),
                    pad.name()
                );
                match ghost.set_target(Some(pad)) {
                    Ok(()) => stream.target = Some((pt, ssrc)),
                    Err(err) => gst::error!(
                        CAT,
                        obj = &obj,
                        "Failed to re-target ghost pad {}: {err}",
                        ghost.name()
                    ),
                }
                return;
            }

            let Some(caps) = stream.caps.clone() else {
                return;
            };

            let ghost_name = format!("src_{stream_id}");
            let stream_id_str = format!("{}/{}", entry.stream_id_base, stream_id);
            let group_id = *entry.group_id.get_or_insert_with(gst::GroupId::next);
            let event_stream_id = stream_id_str.clone();
            let event_caps = caps.clone();

            let templ = obj
                .pad_template("src_%u")
                .expect("Pad template is registered on the element");
            let ghost = gst::GhostPad::builder_from_template_with_target(&templ, pad)
                .unwrap()
                .name(&ghost_name)
                .proxy_pad_chain_function(move |pad, parent, buffer| {
                    let parent = parent.and_then(|p| p.parent());
                    SapSrc::catch_panic_pad_function(
                        parent.as_ref(),
                        || Err(gst::FlowError::Error),
                        |imp| imp.proxy_pad_chain(pad, buffer),
                    )
                })
                .proxy_pad_chain_list_function(move |pad, parent, bufferlist| {
                    let parent = parent.and_then(|p| p.parent());
                    SapSrc::catch_panic_pad_function(
                        parent.as_ref(),
                        || Err(gst::FlowError::Error),
                        |imp| imp.proxy_pad_chain_list(pad, bufferlist),
                    )
                })
                .proxy_pad_event_function(move |pad, parent, event| {
                    let parent = parent.and_then(|p| p.parent());
                    if parent.is_none() {
                        return gst::Pad::event_default(pad, parent.as_ref(), event);
                    }
                    SapSrc::catch_panic_pad_function(
                        parent.as_ref(),
                        || false,
                        |imp| {
                            imp.proxy_pad_event(&event_stream_id, group_id, &event_caps, pad, event)
                        },
                    )
                })
                .build();
            if let Err(err) = ghost.set_active(true) {
                gst::error!(
                    CAT,
                    obj = &obj,
                    "Failed to activate ghost pad {ghost_name}: {err}"
                );
                return;
            }

            // Store the stream-start and caps events, they replace the RTP source's.
            let seqnum = gst::Seqnum::next();
            let _ = ghost.store_sticky_event(
                &gst::event::StreamStart::builder(&stream_id_str)
                    .group_id(group_id)
                    .seqnum(seqnum)
                    .build(),
            );
            let _ =
                ghost.store_sticky_event(&gst::event::Caps::builder(&caps).seqnum(seqnum).build());

            stream.ghost = Some(ghost.clone());
            stream.target = Some((pt, ssrc));
            let all_pads_added =
                !entry.no_more_pads && entry.streams.values().all(|stream| stream.ghost.is_some());
            if all_pads_added {
                entry.no_more_pads = true;
            }
            state.flow_combiner.add_pad(&ghost);
            (ghost, all_pads_added)
        };

        obj.add_pad(&ghost).expect("Pad names are unique");

        obj.child_added(&ghost, &ghost.name());

        // The session might have been torn down while the pad was added.
        let stale = {
            let mut state = self.state.lock().unwrap();
            let stale = state
                .session
                .as_ref()
                .and_then(|(_key, entry)| entry.streams.get(&stream_id))
                .is_none_or(|stream| stream.ghost.as_ref() != Some(&ghost));
            if stale {
                state.flow_combiner.remove_pad(&ghost);
            }
            stale
        };
        if stale {
            let _ = obj.remove_pad(&ghost);
            obj.child_removed(&ghost, &ghost.name());
            return;
        }

        if all_pads_added {
            gst::info!(CAT, obj = &obj, "All pads of the session were added");
            obj.no_more_pads();
        }
    }

    /// SSRC ended via BYE or timeout: send EOS on the pad, but keep it so
    /// that it can be re-targeted when the stream restarts. Pads are only
    /// removed with the session on teardown.
    fn handle_ssrc_ended(&self, stream_id: u32, ssrc: u32) {
        let ghost = {
            let state = self.state.lock().unwrap();
            // Stopped in the meantime, see handle_session_event().
            if state.events.is_none() {
                return;
            }

            let Some(stream) = state
                .session
                .as_ref()
                .and_then(|(_key, entry)| entry.streams.get(&stream_id))
            else {
                return;
            };

            // Only the current source is relevant, older ones were re-targeted away.
            if stream.target.map(|(_, target_ssrc)| target_ssrc) != Some(ssrc) {
                return;
            }
            stream.ghost.clone()
        };

        let Some(ghost) = ghost else {
            return;
        };

        gst::debug!(
            CAT,
            obj = &self.obj(),
            "SSRC {ssrc:x} of stream {stream_id} ended, sending EOS on pad {}",
            ghost.name()
        );

        let _ = ghost.push_event(gst::event::Eos::new());
    }

    /// The session was deleted or expired: send EOS on all its pads and mark
    /// it dead, but keep them until the next session announcement or a stop.
    fn end_session(&self) {
        let ghosts = {
            let mut state = self.state.lock().unwrap();
            let Some((_key, entry)) = state.session.as_mut() else {
                return;
            };
            entry.dead = true;
            entry
                .streams
                .values()
                .filter_map(|stream| stream.ghost.clone())
                .collect::<Vec<_>>()
        };

        for ghost in ghosts {
            let _ = ghost.push_event(gst::event::Eos::new());
        }
    }

    /// Removes all state of the current session: ghost pads (optionally after
    /// EOS), requested pads and internal elements. Must be called without the
    /// state mutex held: streaming threads lock it from the pad stream lock.
    fn teardown_session(&self, eos: bool) {
        let Some((_key, entry)) = self.state.lock().unwrap().session.take() else {
            return;
        };

        let obj = self.obj();
        let mut ghosts = Vec::new();
        for (_stream_id, mut stream) in entry.streams {
            if let Some(ghost) = stream.ghost.take() {
                if eos {
                    let _ = ghost.push_event(gst::event::Eos::new());
                }
                let _ = obj.remove_pad(&ghost);
                obj.child_removed(&ghost, &ghost.name());
                ghosts.push(ghost);
            }

            self.teardown_stream(&mut stream);
        }

        let mut state = self.state.lock().unwrap();
        for ghost in ghosts {
            state.flow_combiner.remove_pad(&ghost);
        }
    }
}

impl BinImpl for SapSrc {
    fn handle_message(&self, message: gst::Message) {
        if message
            .structure()
            .is_some_and(|structure| structure.has_name("GstUDPSrcTimeout"))
        {
            let warn = {
                let mut state = self.state.lock().unwrap();
                if state.warned_timeout {
                    false
                } else {
                    state.warned_timeout = true;
                    true
                }
            };
            if warn {
                gst::warning!(
                    CAT,
                    obj = &self.obj(),
                    "No data received after the configured timeout, \
                     maybe a firewall is blocking reception"
                );
            }
            return;
        }

        self.parent_handle_message(message)
    }
}

impl ChildProxyImpl for SapSrc {
    fn child_by_index(&self, index: u32) -> Option<glib::Object> {
        let parent_children_count = self.parent_children_count();

        if index < parent_children_count {
            self.parent_child_by_index(index)
        } else {
            self.obj()
                .pads()
                .into_iter()
                .nth((index - parent_children_count) as usize)
                .map(|p| p.upcast())
        }
    }

    fn children_count(&self) -> u32 {
        self.parent_children_count() + self.obj().num_pads() as u32
    }

    fn child_by_name(&self, name: &str) -> Option<glib::Object> {
        match self.parent_child_by_name(name) {
            Some(child) => Some(child),
            _ => self.obj().static_pad(name).map(|pad| pad.upcast()),
        }
    }
}

/// Whether both SDPs describe the same set of media streams. Differences in
/// e.g. fmtp / ptime / extmap attributes are not considered relevant, but the
/// connection addresses and directions are.
fn sdp_equivalent(a: &sdp_types_gst::Session, b: &sdp_types_gst::Session) -> bool {
    fn media_equivalent(a: &sdp_types_gst::Media, b: &sdp_types_gst::Media) -> bool {
        a.media() == b.media()
            && a.proto() == b.proto()
            && a.port() == b.port()
            && a.connection() == b.connection()
            && a.direction() == b.direction()
            && a.formats() == b.formats()
            && a.rtpmaps() == b.rtpmaps()
    }

    a.connection() == b.connection()
        && a.media().len() == b.media().len()
        && a.media()
            .iter()
            .zip(b.media())
            .all(|(a, b)| media_equivalent(a, b))
}

/// Stream ID prefix derived from the SDP origin, sanitized to the
/// characters allowed in stream IDs.
fn sdp_stream_id_base(origin: &sdp_types::Origin) -> String {
    format!("{}-{}", origin.unicast_address, origin.sess_id)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The RFC 4570 `a=source-filter` attributes that apply to a media with the
/// connection address `dest`, as the udpsrc2 `source-filter` property value
/// and its exclusivity. A media-level filter overrides the session-level one,
/// even if it does not apply to the media's destination (RFC 4570 §3.1).
fn source_filter(
    session: &sdp_types_gst::Session,
    media: &sdp_types_gst::Media,
    dest: IpAddr,
) -> Option<(String, bool)> {
    if media
        .other_attributes()
        .iter()
        .any(|(name, _)| name == "source-filter")
    {
        return parse_source_filter(media.other_attributes(), dest);
    }

    parse_source_filter(session.other_attributes(), dest)
}

/// Parses the `a=source-filter` lines in `attributes`, applying those whose
/// <dest-address> matches the connection address `dest` or is the "*"
/// wildcard (RFC 4570 §3.1), as the udpsrc2 `source-filter` property value
/// and its exclusivity.
fn parse_source_filter(
    attributes: &[(String, Option<String>)],
    dest: IpAddr,
) -> Option<(String, bool)> {
    let mut exclusive = None;
    let mut sources = vec![];

    for (name, value) in attributes {
        if name != "source-filter" {
            continue;
        }
        let Some(value) = value else {
            continue;
        };

        // <filter-mode> <nettype> <address-types> <dest-address> <src-list>
        let mut tokens = value.split_whitespace();
        let mode = match tokens.next() {
            Some("incl") => false,
            Some("excl") => true,
            mode => {
                gst::fixme!(CAT, "Unsupported source-filter mode {mode:?} in '{value}'");
                continue;
            }
        };
        // Only Internet protocol filters are supported
        match tokens.next() {
            Some("IN") => (),
            nettype => {
                gst::warning!(
                    CAT,
                    "Ignoring a=source-filter with unsupported network type {nettype:?}"
                );
                continue;
            }
        }
        let want_ipv4 = match tokens.next() {
            Some("IP4") => Some(true),
            Some("IP6") => Some(false),
            // Only valid with an FQDN or "*" <dest-address> (RFC 4570 §3.1)
            Some("*") => None,
            address_type => {
                gst::warning!(
                    CAT,
                    "Ignoring a=source-filter with unsupported address type {address_type:?}"
                );
                continue;
            }
        };
        let Some(filter_dest) = tokens.next() else {
            gst::warning!(CAT, "Malformed a=source-filter attribute '{value}'");
            continue;
        };
        if want_ipv4.is_none() && filter_dest != "*" {
            gst::warning!(
                CAT,
                "Ignoring a=source-filter with the address type wildcard and a literal \
                 destination address in '{value}'"
            );
            continue;
        }

        // Only filters for the media's connection address, or for all
        // destinations via the "*" wildcard, apply to the media
        if filter_dest != "*" && filter_dest.parse::<IpAddr>() != Ok(dest) {
            continue;
        }

        if exclusive.is_some_and(|prev| prev != mode) {
            gst::warning!(
                CAT,
                "Ignoring source-filter with conflicting mode in '{value}'"
            );
            continue;
        }
        exclusive = Some(mode);

        for token in tokens {
            match token.parse::<IpAddr>() {
                Ok(addr) if want_ipv4.is_none_or(|ipv4| addr.is_ipv4() == ipv4) => {
                    sources.push(addr);
                }
                Ok(addr) => gst::warning!(
                    CAT,
                    "Ignoring source address '{addr}' that does not match the address type \
                     of '{value}'"
                ),
                Err(_) => gst::warning!(
                    CAT,
                    "Ignoring source address '{token}' that is not an IP address"
                ),
            }
        }
    }

    if sources.is_empty() {
        return None;
    }
    sources.sort();
    sources.dedup();
    Some((
        sources
            .iter()
            .map(|addr| addr.to_string())
            .collect::<Vec<_>>()
            .join(","),
        exclusive.expect("a filter mode was parsed"),
    ))
}

/// Parses an rtp_src_<stream>_<pt>_<ssrc> pad name of rtprecv.
fn parse_rtp_src_pad_name(name: &str) -> Option<(u32, u32, u32)> {
    let mut parts = name.split('_');
    if parts.next()? != "rtp" || parts.next()? != "src" {
        return None;
    }
    let stream = parts.next()?.parse().ok()?;
    let pt = parts.next()?.parse().ok()?;
    let ssrc = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((stream, pt, ssrc))
}
