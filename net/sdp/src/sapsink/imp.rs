// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(unused_doc_comments)]

/**
 * SECTION:element-sapsink
 * @see_also: sapsrc, sapdeviceprovider
 *
 * `sapsink` is a network sink that sends RTP streams and announces them as a
 * session with the Session Announcement Protocol (SAP). It can be combined
 * with RTP payloaders to send multicast RTP sessions.
 *
 * `sapsink` is a bin with request sink pads named `sink_%u`, one per RTP
 * stream. The destination of a stream is configured with the `dest-ip` and
 * `dest-port` pad properties. `dest-port` is required. If `dest-ip` is unset,
 * the stream is sent to the default multicast group `239.1.2.1`. The first caps
 * on a pad define the media line of the session. The session is announced once
 * all pads have caps.
 *
 * The internal `rtpsend` and `rtprecv` elements and the per-stream UDP
 * elements (`udpsink2-rtp-<id>`, `udpsrc2-rtcp-<id>`, `udpsrc2-rtcp-mux-<id>`,
 * `udpsink2-rtcp-<id>`) are added to the bin with stable names, depending on
 * the RTCP mode and rtcp-mux, and can be retrieved with gst_bin_get_by_name()
 * or the #GstChildProxy interface.
 *
 * The session description is built from the #GstSapSink:session-name,
 * #GstSapSink:session-description, #GstSapSink:session-id and
 * #GstSapSink:session-version properties. The #GstSapSink:source-address
 * property is used as the SDP origin address and the SAP originating source.
 * If unset it is derived automatically: an address of one of the
 * #GstSapSink:multicast-iface interfaces, or the source address the routing
 * gives for the stream destinations.
 *
 * Announcements are sent every few minutes, with the first one as soon as
 * all pads have caps after the element reaches PAUSED, to the SAP groups
 * selected by #GstSapSink:sap-addresses. If unset, the SAP multicast group
 * matching the scope of the stream destination addresses is used (RFC 2365
 * for IPv4, RFC 3513 for IPv6). Announcements are sent on the multicast
 * interfaces or the interface owning the source address, not once per
 * interface. On shutdown the session is deleted with a few SAP deletion
 * announcements.
 *
 * All streams of a session have to use the same address family, as the
 * session has a single origin address.
 *
 * By default RTCP is not used. With #GstSapSink:rtcp-mode set to `sendonly`
 * or `sendrecv`, RTCP is sent to the RTP destination port + 1, or to the
 * same port if #GstSapSink:rtcp-mux is set. The #GstSapSink:rtp-profile
 * property selects RTP/AVP or RTP/AVPF.
 *
 * By default multicast loopback is disabled, so local receivers on the same
 * host do not receive the streams and the sender does not receive its own
 * RTCP. Set #GstSapSink:multicast-loopback to true for local testing. SAP
 * announcements are always looped back locally so that multiple SAP
 * listeners on the same host can see each other.
 *
 * ## Examples
 * |[
 * gst-launch-1.0 -v audiotestsrc is-live=true ! opusenc ! rtpopuspay2 pt=96 ! sapsink multicast-loopback=true sink_0::dest-port=5004
 * ]| A pipeline to send an Opus stream to the default multicast group and announce it.
 *
 * Since: plugins-rs-0.16.0
 */
use gio::prelude::*;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use anyhow::{Context, anyhow, bail};

use sdp_types::{Connection, Rtcp, TransportProto};

use super::pad::CapsStatus;
use crate::common::{
    RtcpMode, SinkSocket, SrcSocket, StreamChain, ensure_required_elements, make_queue,
    make_udpsink2, make_udpsrc2, next_rtp_id, remove_element, set_pt_map, teardown_chain,
    used_socket,
};
use crate::listener::{
    GLOBAL_V4_MCAST_GROUP, GLOBAL_V6_MCAST_GROUP, LINK_LOCAL_V4_MCAST_GROUP, ListenerHandle,
    ORG_LOCAL_V4_MCAST_GROUP, SAP_PORT, SITE_LOCAL_V4_MCAST_GROUP, SessionId,
    parse_multicast_groups, scoped_v6_mcast_group,
};

use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{LazyLock, Mutex},
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new("sapsink", gst::DebugColorFlags::empty(), Some("SAP Sink"))
});

const DEFAULT_SESSION_NAME: &str = "GStreamer SAP session";
// Some IPv4 domain-scoped multicast address from 239.0.0.0/8 (RFC 2365)
const DEFAULT_DEST_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(239, 1, 2, 1));

// Factories needed while running, checked up front in change_state() so
// that instantiating elements later can not fail
const REQUIRED_ELEMENTS: &[&str] = &[
    "queue",
    "clocksync",
    "funnel",
    "udpsrc2",
    "udpsink2",
    "rtprecv",
    "rtpsend",
];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstSapSinkRtpProfile")]
pub enum RtpProfile {
    #[default]
    #[enum_value(name = "RTP/AVP", nick = "avp")]
    Avp,
    #[enum_value(name = "RTP/AVPF", nick = "avpf")]
    Avpf,
}

impl RtpProfile {
    fn nick(self) -> &'static str {
        // Look the nick up in the registered enum values instead of
        // duplicating the strings of the #[enum_value] attributes
        let class = glib::EnumClass::new::<RtpProfile>();
        class
            .value(self as i32)
            .expect("registered enum value")
            .nick()
    }

    fn proto(self) -> TransportProto {
        match self {
            Self::Avp => TransportProto::RtpAvp,
            Self::Avpf => TransportProto::RtpAvpf,
        }
    }
}

#[derive(Clone, Debug)]
struct Settings {
    // 0 generates a random session ID
    session_id: u64,
    session_name: String,
    session_description: Option<String>,
    session_version: u64,
    compress: bool,
    sap_addresses: Option<String>,
    source_address: Option<String>,
    multicast_iface: Option<String>,
    multicast_loopback: bool,
    rtcp_mode: RtcpMode,
    rtcp_mux: bool,
    rtp_profile: RtpProfile,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            session_id: 0,
            session_name: DEFAULT_SESSION_NAME.into(),
            session_description: None,
            session_version: 0,
            compress: false,
            sap_addresses: None,
            source_address: None,
            multicast_iface: None,
            multicast_loopback: false,
            rtcp_mode: RtcpMode::default(),
            rtcp_mux: false,
            rtp_profile: RtpProfile::default(),
        }
    }
}

#[derive(Default)]
struct Stream {
    elements: Vec<gst::Element>,
    request_pads: Vec<(gst::Element, gst::Pad)>,
}

#[derive(Default)]
struct State {
    started: bool,
    // Whether the session was announced, reset on stop
    announced: bool,
    listener: Option<ListenerHandle>,
    session_id: Option<SessionId>,
    rtp_id: Option<String>,
    rtpsend: Option<gst::Element>,
    rtprecv: Option<gst::Element>,
    // One stream per requested sink pad
    streams: BTreeMap<u32, Stream>,
    next_pad_id: u32,
}

#[derive(Default)]
pub struct SapSink {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for SapSink {
    const NAME: &'static str = "GstSapSink";
    type Type = super::SapSink;
    type ParentType = gst::Bin;
    type Interfaces = (gst::ChildProxy,);
}

impl ObjectImpl for SapSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt64::builder("session-id")
                    .nick("Session ID")
                    .blurb("Session ID of the announced session, i.e. the session ID of the SDP o= line. 0 generates a random one")
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("session-name")
                    .nick("Session Name")
                    .blurb("Name of the announced session")
                    .default_value(DEFAULT_SESSION_NAME)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("session-description")
                    .nick("Session Description")
                    .blurb("Description of the announced session")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("session-version")
                    .nick("Session Version")
                    .blurb("Session version of the announced session, i.e. the session version of the SDP o= line")
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("compress")
                    .nick("Compress")
                    .blurb("Whether to compress the SAP announcements with zlib")
                    .default_value(false)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("sap-addresses")
                    .nick("SAP Addresses")
                    .blurb(
                        "Comma-separated list of multicast group addresses to announce the session on. If unset, the SAP multicast group matching the scope of the stream destination addresses is used",
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("source-address")
                    .nick("Source Address")
                    .blurb("Source address of the session, used as the SDP origin address and the SAP originating source. If unset, it is derived from the multicast interfaces or the routing to the stream destinations")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("multicast-iface")
                    .nick("Multicast Interface")
                    .blurb("Comma-separated list of network interfaces to use for multicast")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("multicast-loopback")
                    .nick("Multicast Loopback")
                    .blurb("Enable multicast loopback so that local receivers on the same host receive the streams; needed for local testing")
                    .default_value(false)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("rtcp-mode", Settings::default().rtcp_mode)
                    .nick("RTCP Mode")
                    .blurb("Whether to send and/or receive RTCP")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("rtcp-mux")
                    .nick("RTCP Mux")
                    .blurb("Send and receive RTCP on the same port as RTP (a=rtcp-mux)")
                    .default_value(false)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("rtp-profile", Settings::default().rtp_profile)
                    .nick("RTP Profile")
                    .blurb("Profile of the announced RTP session")
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
        self.obj().set_element_flags(gst::ElementFlags::SINK);
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "session-id" => {
                self.settings.lock().unwrap().session_id =
                    value.get().expect("type checked upstream");
            }
            "session-name" => {
                self.settings.lock().unwrap().session_name =
                    value.get().expect("type checked upstream");
            }
            "session-description" => {
                let v = value.get::<Option<&str>>().expect("type checked upstream");
                self.settings.lock().unwrap().session_description =
                    v.filter(|s| !s.is_empty()).map(str::to_string);
            }
            "session-version" => {
                self.settings.lock().unwrap().session_version =
                    value.get().expect("type checked upstream");
            }
            "compress" => {
                self.settings.lock().unwrap().compress =
                    value.get().expect("type checked upstream");
            }
            "sap-addresses" => {
                let v = value.get::<Option<&str>>().expect("type checked upstream");
                self.settings.lock().unwrap().sap_addresses =
                    v.filter(|s| !s.is_empty()).map(str::to_string);
            }
            "source-address" => {
                let v = value.get::<Option<&str>>().expect("type checked upstream");
                self.settings.lock().unwrap().source_address =
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
            "rtcp-mode" => {
                self.settings.lock().unwrap().rtcp_mode =
                    value.get().expect("type checked upstream");
            }
            "rtcp-mux" => {
                self.settings.lock().unwrap().rtcp_mux =
                    value.get().expect("type checked upstream");
            }
            "rtp-profile" => {
                self.settings.lock().unwrap().rtp_profile =
                    value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "session-id" => settings.session_id.to_value(),
            "session-name" => settings.session_name.as_str().to_value(),
            "session-description" => settings.session_description.as_ref().to_value(),
            "session-version" => settings.session_version.to_value(),
            "compress" => settings.compress.to_value(),
            "sap-addresses" => settings.sap_addresses.as_ref().to_value(),
            "source-address" => settings.source_address.as_ref().to_value(),
            "multicast-iface" => settings.multicast_iface.as_ref().to_value(),
            "multicast-loopback" => settings.multicast_loopback.to_value(),
            "rtcp-mode" => settings.rtcp_mode.to_value(),
            "rtcp-mux" => settings.rtcp_mux.to_value(),
            "rtp-profile" => settings.rtp_profile.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for SapSink {}

impl ElementImpl for SapSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "SAP Sink",
                "Sink/Network",
                "Session Announcement Protocol sink",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::new_empty_simple("application/x-rtp");
            let sink_pad_template = gst::PadTemplate::with_gtype(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
                super::SapSinkPad::static_type(),
            )
            .unwrap();

            vec![sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    // A pad only gets its queue here, the sending chain is built once all
    // pads have caps, see maybe_announce().
    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        _name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let obj = self.obj();

        let id = {
            let mut state = self.state.lock().unwrap();
            if state.started {
                gst::warning!(
                    CAT,
                    obj = &obj,
                    "Pads can only be requested before the element is running"
                );
                return None;
            }
            let id = state.next_pad_id;
            state.next_pad_id = state.next_pad_id.wrapping_add(1);
            id
        };
        let name = format!("sink_{id}");

        let queue = make_queue(&format!("queue-{id}"));
        obj.add(&queue).unwrap();
        queue.sync_state_with_parent().unwrap();

        let ghost = gst::PadBuilder::<super::SapSinkPad>::from_template(templ)
            .name(name.as_str())
            .event_function(move |pad, parent, event| {
                SapSink::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.handle_event(pad, event),
                )
            })
            .build();
        let queue_sink = queue.static_pad("sink").expect("queue has a sink pad");
        if let Err(err) = ghost.set_target(Some(&queue_sink)) {
            gst::error!(CAT, obj = &obj, "Failed to set target of pad {name}: {err}");
            remove_element(&*obj, &queue);
            return None;
        }

        if let Err(err) = ghost.set_active(true) {
            gst::error!(CAT, obj = &obj, "Failed to activate pad {name}: {err}");
            remove_element(&*obj, &queue);
            return None;
        }

        obj.add_pad(&ghost).expect("Pad names are unique");

        self.state.lock().unwrap().streams.insert(
            id,
            Stream {
                elements: vec![queue.clone()],
                ..Default::default()
            },
        );

        obj.child_added(&ghost, &ghost.name());

        gst::debug!(CAT, obj = &obj, "Created sink pad {name}");

        Some(ghost.upcast())
    }

    fn release_pad(&self, pad: &gst::Pad) {
        let started = self.state.lock().unwrap().started;
        if started {
            gst::warning!(
                CAT,
                obj = &self.obj(),
                "Pad {} was released while the session is running, its stream is \
                 torn down but the announced session is not updated",
                pad.name()
            );
        }

        self.parent_release_pad(pad);
        self.obj().child_removed(pad, &pad.name());

        let id = pad.downcast_ref::<super::SapSinkPad>().and_then(|pad| {
            pad.name()
                .strip_prefix("sink_")
                .and_then(|id| id.parse().ok())
        });

        let (chain, queue) = {
            let mut state = self.state.lock().unwrap();
            match id.and_then(|id| state.streams.remove(&id)) {
                Some(mut stream) => {
                    // The first element is the pad's queue, the rest is the
                    // sending chain
                    let mut elements = std::mem::take(&mut stream.elements);
                    let queue = (!elements.is_empty()).then(|| elements.remove(0));
                    ((std::mem::take(&mut stream.request_pads), elements), queue)
                }
                None => (StreamChain::default(), None),
            }
        };

        teardown_chain(&*self.obj(), chain);

        // Pads are also released during dispose, when the bin already removed
        // all children
        if let Some(queue) = queue
            && queue.parent().is_some()
        {
            remove_element(&*self.obj(), &queue);
        }
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let ret = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::NullToReady => {
                ensure_required_elements(&*self.obj(), REQUIRED_ELEMENTS)?;
            }
            gst::StateChange::ReadyToPaused => {
                if let Err(err) = self.start() {
                    gst::error!(
                        CAT,
                        obj = &self.obj(),
                        "Failed starting to announce SAP session: {err}"
                    );
                    gst::element_error!(
                        self.obj(),
                        gst::CoreError::Failed,
                        ("Failed starting to announce SAP session: {err}")
                    );
                    return Err(gst::StateChangeError);
                }
            }
            gst::StateChange::PausedToReady => {
                self.stop();
            }
            _ => (),
        }

        Ok(ret)
    }
}

impl SapSink {
    fn start(&self) -> Result<(), anyhow::Error> {
        let res = self.start_inner();
        if res.is_err() {
            self.stop();
        }
        res
    }

    fn start_inner(&self) -> Result<(), anyhow::Error> {
        let obj = self.obj();
        gst::debug!(CAT, obj = &obj, "Preparing SAP session");
        let settings = self.settings.lock().unwrap().clone();

        if settings.rtcp_mode == RtcpMode::ReceiveOnly {
            bail!("rtcp-mode=recvonly is not supported when sending");
        }
        if let Some(source_address) = &settings.source_address {
            source_address
                .parse::<IpAddr>()
                .with_context(|| format!("Invalid source-address '{source_address}'"))?;
        }

        let pads = sink_pads(&obj);
        if pads.is_empty() {
            bail!("At least one sink pad has to be requested");
        }

        // Validate the destination of every stream before the session can be
        // announced
        for pad in &pads {
            pad_dest_checked(pad, settings.rtcp_mux)?;
        }

        let rtp_id = next_rtp_id("sapsink-");
        let rtpsend = gst::ElementFactory::make("rtpsend")
            .name("rtpsend")
            .property("rtp-id", &rtp_id)
            .property_from_str("rtp-profile", settings.rtp_profile.nick())
            .build()
            .unwrap();
        obj.add(&rtpsend).unwrap();
        rtpsend.sync_state_with_parent().unwrap();
        self.state.lock().unwrap().rtpsend = Some(rtpsend);

        let rtprecv = if settings.rtcp_mode.receive() {
            let recv = gst::ElementFactory::make("rtprecv")
                .name("rtprecv")
                .property("rtp-id", &rtp_id)
                .build()
                .unwrap();
            obj.add(&recv).unwrap();
            recv.sync_state_with_parent().unwrap();
            Some(recv)
        } else {
            None
        };

        {
            let mut state = self.state.lock().unwrap();
            state.started = true;
            state.rtp_id = Some(rtp_id);
            state.rtprecv = rtprecv;
        }

        gst::debug!(
            CAT,
            obj = &obj,
            "Session prepared with {} sink pad(s), waiting for caps to announce",
            pads.len()
        );

        Ok(())
    }

    /// Propagate EOS from the RTP path to the RTCP path, which never receives
    /// one because the BYE is sent as a packet instead
    fn inject_eos_into_rtcp_path(&self, rtp_pad: &gst::Pad, rtcp_pad: &gst::Pad) {
        let obj = self.obj().downgrade();
        let rtcp_pad = rtcp_pad.downgrade();
        rtp_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = info.data.as_ref()
                && let gst::EventView::Eos(_) = event.view()
            {
                let (Some(obj), Some(rtcp_pad)) = (obj.upgrade(), rtcp_pad.upgrade()) else {
                    return gst::PadProbeReturn::Ok;
                };
                let event = event.clone();
                obj.call_async(move |_obj| {
                    rtcp_pad.send_event(event);
                });
            }
            gst::PadProbeReturn::Ok
        });
    }

    /// Sets up a single pad and returns the SDP media line describing it.
    fn build_stream(
        &self,
        settings: &Settings,
        source_addr: IpAddr,
        id: u32,
        pad_name: &str,
        (dest, dest_port): (IpAddr, u16),
        caps: &gst::Caps,
    ) -> Result<sdp_types_gst::Media, anyhow::Error> {
        let obj = self.obj();
        gst::debug!(
            CAT,
            obj = &obj,
            "Setting up stream {id} ({pad_name}) to {dest}:{dest_port}"
        );

        // Take what is needed from the state under the lock, the chain is
        // built without it.
        let (queue, rtpsend, rtprecv) = {
            let mut state = self.state.lock().unwrap();
            let rtpsend = state.rtpsend.clone().context("Element not started")?;
            let rtprecv = state.rtprecv.clone();
            let stream = state
                .streams
                .get_mut(&id)
                .with_context(|| format!("No stream state for pad {pad_name}"))?;
            let queue = stream
                .elements
                .first()
                .cloned()
                .context("Queue missing from stream")?;
            (queue, rtpsend, rtprecv)
        };

        let mux = settings.rtcp_mux;
        let send_rtcp = settings.rtcp_mode.send();
        let receive_rtcp = settings.rtcp_mode.receive();

        // RFC 5761 §4: RTP payload types 64-95 MUST NOT be used when
        // multiplexing RTP and RTCP on the same port, such packets are
        // indistinguishable from RTCP
        if mux {
            for structure in caps.iter() {
                if let Ok(pt) = structure.get::<i32>("payload")
                    && (64..=95).contains(&pt)
                {
                    bail!(
                        "Payload type {pt} on pad {pad_name} cannot be used with rtcp-mux, \
                         such RTP packets are indistinguishable from RTCP (RFC 5761 §4)"
                    );
                }
            }
        }

        // Build the stream locally and register it at the end, tearing it
        // down on failure.
        let mut stream = Stream::default();
        let mut build = || -> Result<sdp_types_gst::Media, anyhow::Error> {
            // Sending chain: queue -> clocksync -> rtpsend.rtp_sink_N
            let clocksync = gst::ElementFactory::make("clocksync")
                .name(format!("clocksync-{id}"))
                .build()
                .unwrap();
            obj.add(&clocksync).unwrap();
            stream.elements.push(clocksync.clone());

            let rtp_sink = rtpsend
                .request_pad_simple(&format!("rtp_sink_{id}"))
                .expect("rtp_sink pad requested");
            stream.request_pads.push((rtpsend.clone(), rtp_sink));

            set_pt_map(&rtpsend, id, caps);

            queue.link(&clocksync).expect("queue linked to clocksync");
            clocksync
                .link_pads(Some("src"), &rtpsend, Some(&format!("rtp_sink_{id}")))
                .expect("clocksync linked to rtpsend");

            // RTP output: rtpsend.rtp_src_N -> (funnel) -> udpsink2
            let rtp_sink_el = make_udpsink2(
                &format!("udpsink2-rtp-{id}"),
                dest,
                dest_port,
                // With a=rtcp-mux the RTP socket also receives RTCP and so has
                // to be bound to the destination port and join the group
                SinkSocket::Bind {
                    bind_port: if mux && receive_rtcp { dest_port } else { 0 },
                    auto_multicast: mux && receive_rtcp && dest.is_multicast(),
                },
                settings.multicast_iface.as_deref(),
                settings.multicast_loopback,
            )?;
            obj.add(&rtp_sink_el).unwrap();
            stream.elements.push(rtp_sink_el.clone());

            let mut funnel_rtcp_sink = None;
            if mux {
                let fun = gst::ElementFactory::make("funnel")
                    .name(format!("funnel-{id}"))
                    .build()
                    .unwrap();
                obj.add(&fun).unwrap();
                stream.elements.push(fun.clone());

                let funnel_rtp_sink = fun
                    .request_pad_simple("sink_%u")
                    .expect("funnel sink pad requested");
                stream
                    .request_pads
                    .push((fun.clone(), funnel_rtp_sink.clone()));
                rtpsend
                    .link_pads(
                        Some(&format!("rtp_src_{id}")),
                        &fun,
                        Some(funnel_rtp_sink.name().as_str()),
                    )
                    .expect("rtp_src linked to funnel");

                if send_rtcp {
                    let rtcp_sink = fun
                        .request_pad_simple("sink_%u")
                        .expect("funnel sink pad requested");
                    stream.request_pads.push((fun.clone(), rtcp_sink.clone()));
                    funnel_rtcp_sink = Some((fun.clone(), rtcp_sink.name()));

                    // The funnel only forwards EOS when all its pads have seen one
                    self.inject_eos_into_rtcp_path(&funnel_rtp_sink, &rtcp_sink);
                }

                fun.link(&rtp_sink_el).expect("funnel linked to udpsink2");
            } else {
                rtpsend
                    .link_pads(Some(&format!("rtp_src_{id}")), &rtp_sink_el, Some("sink"))
                    .expect("rtp_src linked to udpsink2");
            }

            // RTCP reception. With a=rtcp-mux RTCP is received on the RTP socket
            // and demuxed from RTP by payload type (RFC 5761), otherwise on a
            // separate socket.
            let mut rtcp_rx_port = None;
            let mut rtcp_src_el = None;
            if receive_rtcp {
                let rtprecv = rtprecv.as_ref().context("rtprecv not set up")?;
                let rtcp_sink = rtprecv
                    .request_pad_simple(&format!("rtcp_sink_{id}"))
                    .expect("rtcp_sink pad requested");
                stream.request_pads.push((rtprecv.clone(), rtcp_sink));

                set_pt_map(rtprecv, id, caps);

                let rtcp_src = if mux {
                    let socket = used_socket(&rtp_sink_el)?;
                    let udpsrc = make_udpsrc2(
                        &format!("udpsrc2-rtcp-mux-{id}"),
                        SrcSocket::Shared(&socket),
                        // no caps, the probe below filters instead
                        None,
                        settings.multicast_iface.as_deref(),
                        None,
                    )?;

                    // Pass on only RTCP packets, any RTP on the multiplexed
                    // socket is not expected and dropped.
                    udpsrc.static_pad("src").unwrap().add_probe(
                        gst::PadProbeType::BUFFER | gst::PadProbeType::BLOCK,
                        |_pad, info| {
                            if let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_ref()
                                && !is_rtcp_packet(buffer)
                            {
                                return gst::PadProbeReturn::Drop;
                            }
                            gst::PadProbeReturn::Ok
                        },
                    );

                    udpsrc
                } else if dest.is_multicast() {
                    // All participants of a multicast session receive RTCP on the
                    // same port, the RTP port + 1 (RFC 3550 §6)
                    make_udpsrc2(
                        &format!("udpsrc2-rtcp-{id}"),
                        SrcSocket::Bind {
                            address: dest,
                            port: u32::from(dest_port.saturating_add(1)),
                            timeout: 0,
                        },
                        Some(&gst::Caps::new_empty_simple("application/x-rtcp")),
                        settings.multicast_iface.as_deref(),
                        None,
                    )?
                } else {
                    // Unicast has no session-wide RTCP port and the peer can
                    // only learn this one via a=rtcp (RFC 3605): bind any port
                    make_udpsrc2(
                        &format!("udpsrc2-rtcp-{id}"),
                        SrcSocket::Bind {
                            address: source_addr,
                            port: 0,
                            timeout: 0,
                        },
                        Some(&gst::Caps::new_empty_simple("application/x-rtcp")),
                        settings.multicast_iface.as_deref(),
                        None,
                    )?
                };
                obj.add(&rtcp_src).unwrap();
                stream.elements.push(rtcp_src.clone());

                rtcp_src
                    .link_pads(Some("src"), rtprecv, Some(&format!("rtcp_sink_{id}")))
                    .expect("rtcp src linked to rtprecv");

                // Announce the port explicitly, which also lifts the even/odd
                // port pair convention of RFC 3550 §11
                if !mux {
                    rtcp_rx_port = Some(self.bound_port(&rtcp_src)?);
                }
                rtcp_src_el = Some(rtcp_src);
            }

            // RTCP transmission
            if send_rtcp {
                let rtcp_src_pad = rtpsend
                    .request_pad_simple(&format!("rtcp_src_{id}"))
                    .expect("rtcp_src pad requested");
                stream.request_pads.push((rtpsend.clone(), rtcp_src_pad));

                if let Some((funnel, sink_name)) = funnel_rtcp_sink.as_ref() {
                    rtpsend
                        .link_pads(
                            Some(&format!("rtcp_src_{id}")),
                            funnel,
                            Some(sink_name.as_str()),
                        )
                        .expect("rtcp_src linked to funnel");
                } else {
                    let rtcp_socket = match rtcp_src_el.as_ref() {
                        Some(rtcp_src) => Some(used_socket(rtcp_src)?),
                        None => None,
                    };
                    // RTCP destination is RTP destination port + 1 (RFC 3550)
                    let rtcp_sink_el = make_udpsink2(
                        &format!("udpsink2-rtcp-{id}"),
                        dest,
                        dest_port.saturating_add(1),
                        match &rtcp_socket {
                            Some(socket) => SinkSocket::Shared(socket),
                            None => SinkSocket::Bind {
                                bind_port: 0,
                                // element default, joins a multicast destination
                                auto_multicast: true,
                            },
                        },
                        settings.multicast_iface.as_deref(),
                        settings.multicast_loopback,
                    )?;
                    obj.add(&rtcp_sink_el).unwrap();
                    stream.elements.push(rtcp_sink_el.clone());

                    rtpsend
                        .link_pads(Some(&format!("rtcp_src_{id}")), &rtcp_sink_el, Some("sink"))
                        .expect("rtcp_src linked to udpsink2");

                    self.inject_eos_into_rtcp_path(
                        &rtp_sink_el.static_pad("sink").unwrap(),
                        &rtcp_sink_el.static_pad("sink").unwrap(),
                    );
                }
            }

            let mut media = sdp_types_gst::caps_to_media(caps)
                .with_context(|| format!("Failed to convert caps of pad {pad_name} to SDP"))?;
            media.set_port(dest_port);
            media.set_connection(Some(Connection::from_ip_addr(dest)));
            media.set_proto(settings.rtp_profile.proto());
            // a=rtcp-mux is only meaningful if RTCP is actually used
            if mux && (send_rtcp || receive_rtcp) {
                media.set_rtcp_mux(true);
            }
            if let Some(rtcp_rx_port) = rtcp_rx_port {
                // For multicast the connection address is also the RTCP
                // destination, so only the port has to be announced
                media.set_rtcp(Some(if dest.is_multicast() {
                    Rtcp::with_port(rtcp_rx_port)
                } else {
                    Rtcp::with_ip_addr(rtcp_rx_port, source_addr)
                }));
            }

            Ok(media)
        };

        let media = match build() {
            Ok(media) => media,
            Err(err) => {
                self.teardown_stream(&mut stream);
                return Err(err);
            }
        };

        // Only registered chains are torn down by stop() and the announcement
        // retry. Keep the elements, registration moves them into the state.
        let elements = stream.elements.clone();
        let registered = {
            let mut state = self.state.lock().unwrap();
            if !state.started {
                false
            } else if let Some(stream_state) = state.streams.get_mut(&id) {
                stream_state.elements.append(&mut stream.elements);
                stream_state.request_pads.append(&mut stream.request_pads);
                true
            } else {
                false
            }
        };
        if !registered {
            self.teardown_stream(&mut stream);
            bail!("Element was stopped or pad of stream {id} was released during setup");
        }

        // Sync in reverse order so that sources with running tasks start last.
        // The pad can be released concurrently, in which case the chain is gone.
        for element in elements.iter().rev() {
            if let Err(err) = element.sync_state_with_parent() {
                gst::debug!(
                    CAT,
                    obj = &obj,
                    "Failed to sync {} to the parent state: {err}",
                    element.name()
                );
            }
        }

        Ok(media)
    }

    /// Tears down the request pads and elements of a stream that was built but
    /// never registered in the state. Must be called without the state mutex
    /// held.
    fn teardown_stream(&self, stream: &mut Stream) {
        teardown_chain(
            &*self.obj(),
            (
                std::mem::take(&mut stream.request_pads),
                std::mem::take(&mut stream.elements),
            ),
        );
    }

    fn build_sdp(
        &self,
        settings: &Settings,
        source_addr: IpAddr,
        medias: Vec<sdp_types_gst::Media>,
    ) -> Result<sdp_types_gst::Session, anyhow::Error> {
        let sess_id = if settings.session_id == 0 {
            rand::random::<u64>() & 0x7fff_ffff_ffff_ffff
        } else {
            settings.session_id
        };

        Ok(sdp_types_gst::Session::builder()
            .origin(sdp_types::Origin::with_ip_addr(
                sess_id,
                settings.session_version,
                source_addr,
            ))
            .name(settings.session_name.clone())
            .information(settings.session_description.clone())
            // A session without explicit timing, i.e. a permanent session, is
            // announced: its lifetime is managed by the announcements
            // themselves (RFC 2974 §4)
            .media(medias)
            .build())
    }

    fn stop(&self) {
        let obj = self.obj();
        gst::debug!(CAT, obj = &obj, "Stopping SAP session announcement");

        // Take blocking operations out of the state: no element or pad
        // operations and no listener drop under the state mutex.
        let (listener, chains, elements) = {
            let mut state = self.state.lock().unwrap();
            state.started = false;
            state.announced = false;
            state.rtp_id = None;
            state.session_id = None;

            let chains = self.drain_stream_chains(&mut state);

            (
                state.listener.take(),
                chains,
                [state.rtpsend.take(), state.rtprecv.take()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>(),
            )
        };

        // Reset per-stream pad state so the element can be restarted later
        for pad in sink_pads(&obj) {
            pad.imp().reset();
        }

        // Dropping the handle unregisters the session if needed, which can
        // join the listener thread.
        if listener.is_some() {
            gst::info!(CAT, obj = &obj, "Stopped announcing SAP session");
        }
        drop(listener);

        for chain in chains {
            teardown_chain(&*obj, chain);
        }

        for element in elements {
            remove_element(&*obj, &element);
        }
    }

    /// Takes the sending chains of all streams out of the state, keeping their
    /// per-pad queues, which are reused when the element is started again.
    fn drain_stream_chains(&self, state: &mut State) -> Vec<StreamChain> {
        state
            .streams
            .values_mut()
            .map(|stream| {
                (
                    std::mem::take(&mut stream.request_pads),
                    stream.elements.drain(1..).collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    fn bound_port(&self, element: &gst::Element) -> Result<u16, anyhow::Error> {
        let socket = used_socket(element)?;
        let address = socket
            .local_address()
            .ok()
            .with_context(|| format!("Socket of {} is not bound", element.name()))?;
        let address = address
            .downcast::<gio::InetSocketAddress>()
            .map_err(|_| anyhow!("Socket of {} is not an IPv4/IPv6 socket", element.name()))?;

        Ok(address.port())
    }

    fn handle_event(&self, pad: &super::SapSinkPad, event: gst::Event) -> bool {
        gst::trace!(
            CAT,
            obj = &self.obj(),
            "Event {} on pad {}",
            event.type_(),
            pad.name()
        );
        if let gst::EventView::Caps(caps) = event.view()
            && !self.handle_caps(pad, caps.caps_owned())
        {
            // Conflicting caps, EOS was sent to the pad: reject the event so
            // that upstream sees the negotiation failure
            return false;
        }

        gst::Pad::event_default(pad, Some(&*self.obj()), event)
    }

    // Whether the caps event should be forwarded to the pad's target
    fn handle_caps(&self, pad: &super::SapSinkPad, caps: gst::Caps) -> bool {
        let status = pad.imp().check_caps(&caps);
        gst::debug!(
            CAT,
            obj = &self.obj(),
            "Caps on pad {}: {status:?}",
            pad.name()
        );

        match status {
            CapsStatus::Stored => {
                self.maybe_announce();
            }
            CapsStatus::Same => (),
            CapsStatus::Conflict => {
                gst::warning!(
                    CAT,
                    obj = &self.obj(),
                    "Caps of pad {} changed after being set, the announced session \
                     is not updated, sending EOS",
                    pad.name()
                );
                if let Some(target) = pad.target() {
                    let _ = target.push_event(gst::event::Eos::new());
                }
                return false;
            }
        }

        true
    }

    /// Announces the session once started and every pad has caps. The first
    /// caller that sees all pads with caps does the announcement.
    fn maybe_announce(&self) {
        let obj = self.obj();

        let announce = {
            let mut state = self.state.lock().unwrap();
            if state.announced {
                gst::debug!(CAT, obj = &obj, "Session already announced");
                false
            } else if !state.started {
                gst::debug!(
                    CAT,
                    obj = &obj,
                    "Pad received caps before the element is started"
                );
                false
            } else {
                let missing = sink_pads(&obj)
                    .into_iter()
                    .filter(|pad| pad.imp().caps().is_none())
                    .map(|pad| pad.name())
                    .collect::<Vec<_>>();
                if missing.is_empty() {
                    state.announced = true;
                    true
                } else {
                    gst::debug!(
                        CAT,
                        obj = &obj,
                        "Still waiting for caps on pad(s): {}",
                        missing.join(", ")
                    );
                    false
                }
            }
        };

        if !announce {
            return;
        }

        gst::debug!(CAT, obj = &obj, "All pads have caps, announcing session");
        if let Err(err) = self.announce() {
            gst::element_error!(
                &obj,
                gst::CoreError::Failed,
                ("Failed to announce SAP session: {err}")
            );

            // Tear down partially built streams and allow a retry later,
            // e.g. after a restart of the element
            let chains = {
                let mut state = self.state.lock().unwrap();
                state.announced = false;
                self.drain_stream_chains(&mut state)
            };
            for chain in chains {
                teardown_chain(&*obj, chain);
            }
        }
    }

    fn announce(&self) -> Result<(), anyhow::Error> {
        let obj = self.obj();
        let settings = self.settings.lock().unwrap().clone();

        // The destinations are needed before the streams are built, to derive
        // the source address and the announcement groups
        let pads = sink_pads(&obj);
        let mut dests = Vec::with_capacity(pads.len());
        for pad in &pads {
            dests.push(pad_dest_checked(pad, settings.rtcp_mux)?);
        }
        let first_dest = dests.first().copied().context("No sink pads to announce")?;

        // A session has a single origin address, so all streams have to use
        // the same address family
        if let Some(dest) = dests
            .iter()
            .map(|(dest, _)| *dest)
            .find(|dest| dest.is_ipv6() != first_dest.0.is_ipv6())
        {
            bail!(
                "Stream destinations {} and {} use different address families, which \
                 cannot be announced in a single SAP session",
                first_dest.0,
                dest
            );
        }

        let source_addr = match settings.source_address.as_deref() {
            Some(source_address) => source_address
                .parse()
                .with_context(|| format!("Invalid source-address '{source_address}'"))?,
            None => derive_source_address(first_dest.0, settings.multicast_iface.as_deref())?,
        };

        let mut medias = Vec::with_capacity(pads.len());
        for (pad, dest) in pads.iter().zip(&dests) {
            let id = stream_id(pad);
            let caps = pad.imp().caps().context("Pad lost its caps")?;
            let media = self.build_stream(&settings, source_addr, id, &pad.name(), *dest, &caps)?;
            medias.push(media);
        }

        let sdp = self.build_sdp(&settings, source_addr, medias)?;
        gst::debug!(
            CAT,
            obj = &obj,
            "Built SDP for session:\n{}",
            sdp.to_sdp_string()
        );
        let dest_ips = dests.iter().map(|(dest, _)| *dest).collect::<Vec<_>>();
        let groups = match settings.sap_addresses.as_deref() {
            Some(addresses) => {
                let groups = parse_multicast_groups(addresses);
                if groups.is_empty() {
                    bail!("No valid multicast group address in sap-addresses '{addresses}'");
                }
                groups
            }
            None => sap_groups_for_dests(&dest_ips),
        };

        // Announce on the interfaces the media is sent on instead of once per
        // interface: the configured ones, or the one owning the source address
        let ifaddrs = getifaddrs::getifaddrs()
            .ok()
            .map(|ifaddrs| ifaddrs.collect::<getifaddrs::Interfaces>());
        let interfaces = settings
            .multicast_iface
            .as_deref()
            .map(|ifaces| {
                ifaces
                    .split(',')
                    .map(str::trim)
                    .filter(|iface| !iface.is_empty())
                    .map(|iface| {
                        // Resolve friendly names on Windows to the names used
                        // by the listener
                        ifaddrs
                            .as_ref()
                            .and_then(|ifaddrs| interface_name(ifaddrs, iface))
                            .unwrap_or_else(|| iface.to_string())
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|interfaces| !interfaces.is_empty())
            .or_else(|| {
                let ifaddrs = ifaddrs.as_ref()?;
                interface_name_for_address(ifaddrs, source_addr).map(|name| vec![name])
            });

        let mut listener = ListenerHandle::acquire()?;
        let session_id = listener.register_session(
            &sdp,
            source_addr,
            &groups,
            interfaces.as_deref(),
            settings.compress,
        )?;

        let mut state = self.state.lock().unwrap();
        state.session_id = Some(session_id);
        state.listener = Some(listener);

        let interfaces = interfaces
            .as_ref()
            .map(|interfaces| interfaces.join(","))
            .unwrap_or_else(|| "all interfaces".to_string());
        gst::info!(
            CAT,
            obj = &obj,
            "Announcing SAP session {} ({}) from {source_addr} to {} via {interfaces}",
            sdp.origin().sess_id,
            sdp.name(),
            groups
                .iter()
                .map(|group| group.to_string())
                .collect::<Vec<_>>()
                .join(","),
        );

        Ok(())
    }
}

impl BinImpl for SapSink {}

impl ChildProxyImpl for SapSink {
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

fn sink_pads(obj: &super::SapSink) -> Vec<super::SapSinkPad> {
    obj.pads()
        .into_iter()
        .filter_map(|pad| pad.downcast::<super::SapSinkPad>().ok())
        .collect()
}

/// Destination address and port of a pad, with the default multicast
/// destination as fallback, rejecting a destination port of 65535 without
/// rtcp-mux: RTCP would be sent to the RTP destination port itself instead
/// of port + 1 (RFC 3550).
fn pad_dest_checked(
    pad: &super::SapSinkPad,
    rtcp_mux: bool,
) -> Result<(IpAddr, u16), anyhow::Error> {
    let (ip, port) = pad.imp().dest();
    let dest = ip
        .as_deref()
        .map(|ip| {
            ip.parse::<IpAddr>()
                .with_context(|| format!("Invalid dest-ip on pad {}", pad.name()))
        })
        .transpose()?
        .unwrap_or(DEFAULT_DEST_IP);
    if port == 0 {
        bail!("dest-port has to be set on pad {}", pad.name());
    }
    if port == u16::MAX && !rtcp_mux {
        bail!(
            "dest-port 65535 on pad {} cannot be used without rtcp-mux",
            pad.name()
        );
    }

    Ok((dest, port))
}

/// Whether the interface has the given name or, on Windows, the given
/// friendly name
fn interface_matches(iface: &getifaddrs::Interface<getifaddrs::Addresses>, name: &str) -> bool {
    if iface.name == name {
        return true;
    }

    #[cfg(windows)]
    if iface.description == name {
        return true;
    }

    false
}

/// The name of the interface with the given name or, on Windows, the given
/// friendly name
fn interface_name(ifaddrs: &getifaddrs::Interfaces, name: &str) -> Option<String> {
    ifaddrs
        .values()
        .find(|iface| interface_matches(iface, name))
        .map(|iface| iface.name.clone())
}

/// Whether the address is only usable on the local link or host and should
/// not be preferred as the session source address
fn is_local_only(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_loopback() || address.is_link_local(),
        IpAddr::V6(address) => address.is_loopback() || address.is_unicast_link_local(),
    }
}

/// The preferred address of the given family of the named network interface.
/// Link-local and loopback addresses are only used if the interface has no
/// other address of the family.
fn interface_address(ifaddrs: &getifaddrs::Interfaces, name: &str, v6: bool) -> Option<IpAddr> {
    let family = if v6 {
        getifaddrs::AddressFamily::V6
    } else {
        getifaddrs::AddressFamily::V4
    };

    let mut fallback = None;
    for iface in ifaddrs
        .values()
        .filter(|iface| interface_matches(iface, name))
    {
        let Some(addresses) = iface.address.get_all(family) else {
            continue;
        };
        for address in addresses.iter().filter_map(getifaddrs::Address::ip_addr) {
            if !is_local_only(address) {
                return Some(address);
            }
            fallback.get_or_insert(address);
        }
    }

    fallback
}

/// The name of the interface owning the given local address
fn interface_name_for_address(ifaddrs: &getifaddrs::Interfaces, addr: IpAddr) -> Option<String> {
    ifaddrs.values().find_map(|iface| {
        iface
            .address
            .iter()
            .flatten()
            .filter_map(getifaddrs::Address::ip_addr)
            .any(|address| address == addr)
            .then(|| iface.name.clone())
    })
}

/// Source address of the session when `source-address` is unset: an address
/// of one of the `multicast-iface` interfaces with the address family of the
/// destination or, without configured interfaces, the source address the
/// routing selects for the destination. Connecting a UDP socket only performs
/// a local route lookup and sends no packets.
fn derive_source_address(
    dest: IpAddr,
    multicast_ifaces: Option<&str>,
) -> Result<IpAddr, anyhow::Error> {
    if let Some(ifaces) = multicast_ifaces {
        let ifaddrs = getifaddrs::getifaddrs()
            .context("Failed to enumerate network interfaces")?
            .collect::<getifaddrs::Interfaces>();

        let mut names = Vec::new();
        for iface in ifaces
            .split(',')
            .map(str::trim)
            .filter(|iface| !iface.is_empty())
        {
            if let Some(address) = interface_address(&ifaddrs, iface, dest.is_ipv6()) {
                return Ok(address);
            }
            names.push(iface);
        }

        bail!(
            "None of the interfaces '{}' in multicast-iface has an IPv{} address for \
             destination {dest}, set source-address explicitly",
            names.join(","),
            if dest.is_ipv6() { 6 } else { 4 }
        );
    }

    let socket = socket2::Socket::new(
        if dest.is_ipv6() {
            socket2::Domain::IPV6
        } else {
            socket2::Domain::IPV4
        },
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .context("Failed to create socket for source address lookup")?;
    socket
        .connect(&SocketAddr::new(dest, SAP_PORT).into())
        .with_context(|| {
            format!(
                "No route to destination {dest}, set source-address or multicast-iface explicitly"
            )
        })?;
    let source_addr = socket
        .local_addr()
        .with_context(|| format!("Failed to look up the local address for destination {dest}"))?;

    source_addr
        .as_socket()
        .filter(|addr| !addr.ip().is_unspecified())
        .map(|addr| addr.ip())
        .with_context(|| {
            format!(
                "No local address found for destination {dest}, set source-address or \
                 multicast-iface explicitly"
            )
        })
}

/// The SAP multicast group matching the scope of a media destination:
/// link-local IPv4 groups use the link-local SAP group, 239.192.0.0/14 the
/// organization-scoped one, the rest of 239.0.0.0/8 the site-local one
/// (RFC 2365 §3). IPv6 destinations use the group of their scope
/// (RFC 3513 §2.4). Everything else, including unicast, announces globally.
fn sap_group_for_dest(dest: IpAddr) -> IpAddr {
    match dest {
        IpAddr::V4(dest) => {
            let o = dest.octets();
            if o[0] == 224 && o[1] == 0 && o[2] == 0 {
                LINK_LOCAL_V4_MCAST_GROUP
            } else if o[0] == 239 {
                if (192..=195).contains(&o[1]) {
                    ORG_LOCAL_V4_MCAST_GROUP
                } else {
                    SITE_LOCAL_V4_MCAST_GROUP
                }
            } else {
                GLOBAL_V4_MCAST_GROUP
            }
        }
        IpAddr::V6(dest) => {
            let octets = dest.octets();
            // Multicast addresses carry the scope in the low nibble of the
            // second octet
            if octets[0] == 0xff {
                scoped_v6_mcast_group(octets[1] & 0x0f).unwrap_or(GLOBAL_V6_MCAST_GROUP)
            } else {
                GLOBAL_V6_MCAST_GROUP
            }
        }
    }
}

/// The SAP multicast groups for all stream destinations, deduplicated
fn sap_groups_for_dests(dests: &[IpAddr]) -> Vec<IpAddr> {
    let mut groups = dests
        .iter()
        .copied()
        .map(sap_group_for_dest)
        .collect::<Vec<_>>();
    groups.sort();
    groups.dedup();
    groups
}

/// Stream id of a pad, i.e. the number of its sink_N pad name.
/// request_new_pad() always names the pads it creates.
fn stream_id(pad: &super::SapSinkPad) -> u32 {
    pad.name()
        .strip_prefix("sink_")
        .and_then(|id| id.parse().ok())
        .expect("Pad names follow the sink_%u template")
}

/// Whether a buffer on a multiplexed RTP/RTCP socket looks like an RTCP
/// packet: version 2 and an RTCP payload type (RFC 5761).
fn is_rtcp_packet(buffer: &gst::Buffer) -> bool {
    let Ok(map) = buffer.map_readable() else {
        return false;
    };
    let bytes = map.as_slice();
    bytes.len() >= 4 && (bytes[0] >> 6) == 2 && (192..=223).contains(&bytes[1])
}
