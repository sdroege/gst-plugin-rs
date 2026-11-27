// SPDX-License-Identifier: MPL-2.0

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::LazyLock;
use std::sync::Mutex;

use futures::StreamExt;
use gst::glib;
use gst::glib::subclass::ObjectImplWeakRef;
use gst::prelude::*;
use gst::subclass::prelude::*;

use librice::agent::Agent;
use librice::agent::AgentMessage;
use librice::agent::{TurnConfig, TurnCredentials};
use librice::candidate::Candidate;
use librice::candidate::TransportType;
use librice::component::ComponentConnectionState;
use librice::stream::Credentials;
use librice::stream::Stream;
use tokio::task::JoinHandle;

use sdp_types::AddrType;
use sdp_types::Connection;
use sdp_types::Direction;
use sdp_types::Fingerprint;
use sdp_types::HashFunc;
use sdp_types::IceOptions;
use sdp_types::IcePwd;
use sdp_types::IceUfrag;
use sdp_types::MediaType;
use sdp_types::Mid;
use sdp_types::NetType;
use sdp_types::Origin;
use sdp_types::Setup;
use sdp_types::Time;
use sdp_types::TransportProto;

use crate::RUNTIME;
use crate::transceiver::Transceiver;
use crate::transceiver::imp::rtp_caps_to_media;
use crate::webrtcsend::WebRTCSendSinkPad;
use crate::webrtcsession::SignalingStatus;
use crate::webrtcsession::dtls::DtlsEvent;
use crate::webrtcsession::dtls::DtlsPollRet;
use crate::webrtcsession::dtls::KeyMaterial;
use crate::webrtcsession::dtls::TlsImpl;
use crate::webrtcsession::sdp::WebRTCSdpType;
use crate::webrtcsession::sdp::{
    answer_media, answer_media_from_offer, answer_setup, intersect_setup, is_datachannel,
    is_rejected_media, media_intersect, parse_webrtc_sdp, select_bundle_tag,
};
use sdp_types_gst::{Media, Session};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "webrtc2session",
        gst::DebugColorFlags::empty(),
        Some("WebRTCSession"),
    )
});

static PROMISE_REPLY_NAME: &str = "application/x-webrtcbin2-promise";

impl SignalingStatus {
    fn is_correct_for_desc_type(&self, typ: WebRTCSdpType, is_remote: bool) -> bool {
        match typ {
            WebRTCSdpType::Rollback => ![
                Self::Stable,
                Self::HaveLocalPrAnswer,
                Self::HaveRemotePrAnswer,
            ]
            .contains(self),
            WebRTCSdpType::Offer => {
                if is_remote {
                    [Self::Stable, Self::HaveRemoteOffer].contains(self)
                } else {
                    [Self::Stable, Self::HaveLocalOffer].contains(self)
                }
            }
            WebRTCSdpType::Answer | WebRTCSdpType::PrAnswer => {
                if is_remote {
                    [Self::HaveLocalOffer, Self::HaveRemotePrAnswer].contains(self)
                } else {
                    [Self::HaveRemoteOffer, Self::HaveLocalPrAnswer].contains(self)
                }
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct WebRTCSession {
    _rtp_id: String,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
pub(crate) struct State {
    closed: bool,

    weak_send: Option<glib::WeakRef<crate::webrtcsend::WebRTCSend>>,
    weak_recv: Option<glib::WeakRef<crate::webrtcrecv::WebRTCRecv>>,

    rtp_session: Option<gst::Object>,

    signaling_status: SignalingStatus,
    pending_local_desc: Option<(WebRTCSdpType, Session)>,
    pending_remote_desc: Option<(WebRTCSdpType, Session)>,
    current_local_desc: Option<(WebRTCSdpType, Session)>,
    current_remote_desc: Option<(WebRTCSdpType, Session)>,

    operations: Option<Operations>,

    transceivers: Vec<Transceiver>,
    negotiation_needed: bool,

    ice: Option<Ice>,
    transports: Vec<Transport>,

    stun_servers: Vec<StunServer>,
    turn_servers: Vec<TurnServer>,
    // TODO: sctp_transport, data_channels, last_created_offer/answer, early_candidates,
    // ice_connection_state, ice_gathering_state, connection_state,
    // local_ice_credentials_to_replace
}

struct DtlsPollFuture {
    weak_self: ObjectImplWeakRef<WebRTCSession>,
    transport_id: u32,
    timer: Pin<Box<tokio::time::Sleep>>,
}

impl DtlsPollFuture {
    fn new(weak_self: ObjectImplWeakRef<WebRTCSession>, transport_id: u32) -> Self {
        Self {
            weak_self,
            transport_id,
            timer: Box::pin(tokio::time::sleep_until(std::time::Instant::now().into())),
        }
    }
}

struct DtlsData {
    _transport_id: u32,
    data: Vec<u8>,
}

impl futures::Stream for DtlsPollFuture {
    type Item = DtlsData;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let Some(this) = self.weak_self.upgrade() else {
            return std::task::Poll::Ready(None);
        };
        let mut inner = this.state.lock().unwrap();
        let Some(transport) = inner
            .transports
            .iter_mut()
            .find(|transport| transport.id == self.transport_id)
        else {
            return std::task::Poll::Ready(None);
        };
        transport.dtls_waker = Some(cx.waker().clone());
        let wait_until = match transport.dtls.poll(std::time::Instant::now()) {
            DtlsPollRet::WaitUntil(new_now) => new_now,
            DtlsPollRet::Closed => return std::task::Poll::Ready(None),
        };
        gst::trace!(
            CAT,
            "polling dtls {} waiting until {wait_until:?}",
            self.transport_id
        );

        if let Some(event) = transport.dtls.poll_event() {
            match event {
                DtlsEvent::Connected => (),
                DtlsEvent::KeyingMaterial(material) => {
                    let is_client = transport.dtls.is_client().unwrap();
                    inner.dtls_key_material(self.transport_id, material, is_client)
                }
            }
        } else if let Some(data) = transport.dtls.poll_transmit() {
            return std::task::Poll::Ready(Some(DtlsData {
                _transport_id: self.transport_id,
                data,
            }));
        }

        self.timer.as_mut().reset(wait_until.into());
        if core::future::Future::poll(self.timer.as_mut(), cx).is_ready() {
            cx.waker().wake_by_ref();
            return core::task::Poll::Pending;
        }
        core::task::Poll::Pending
    }
}

#[derive(Debug)]
struct Transport {
    id: u32,
    ice_stream_id: usize,
    dtls: TlsImpl,
    dtls_task: Option<JoinHandle<()>>,
    dtls_waker: Option<core::task::Waker>,
    expected_remote_fingerprint: Vec<Fingerprint>,
}

#[derive(Debug)]
struct Ice {
    agent: Agent,
    mline_stream_id_map: BTreeMap<u32, Stream>,
}

impl Ice {
    fn stream_for_mline(&self, mline: u32) -> Option<&Stream> {
        self.mline_stream_id_map.get(&mline)
    }
    fn mline_for_stream(&self, stream_id: usize) -> Option<u32> {
        self.mline_stream_id_map.iter().find_map(|(mline, stream)| {
            if stream.id() == stream_id {
                Some(*mline)
            } else {
                None
            }
        })
    }
}

/// The stored descriptions don't contain the locally gathered ICE candidates,
/// add them to a copy of the description for external consumption.
fn sdp_with_local_candidates(sdp: &Session, ice: &Ice) -> String {
    let mut sdp = sdp.clone();
    for (i, media) in sdp.media_mut().iter_mut().enumerate() {
        let Some(stream) = ice.stream_for_mline(i as u32) else {
            continue;
        };
        for candidate in stream.local_candidates() {
            let value = candidate.to_sdp_string();
            let value = value.strip_prefix("a=candidate:").unwrap_or(&value);
            match value.parse::<sdp_types::Candidate>() {
                Ok(c) => media.candidates_mut().push(c),
                Err(e) => gst::warning!(CAT, "Ignoring local candidate {value}: {e}"),
            }
        }
    }
    sdp.to_sdp_string()
}

#[derive(Debug)]
enum TransceiverSearch {
    Mid,
    PendingMid,
    MLine,
    SendCaps,
}

impl State {
    pub(crate) fn set_rtp_session(&mut self, rtp_session: gst::Object) {
        self.rtp_session = Some(rtp_session);
    }
    pub fn transceivers_mut(&mut self) -> impl Iterator<Item = &mut Transceiver> {
        self.transceivers.iter_mut()
    }

    pub fn add_transceiver(&mut self, transceiver: Transceiver) {
        self.transceivers.push(transceiver);
    }

    fn transceiver_for_sdp_media(
        &self,
        media: &Media,
        idx: usize,
    ) -> Option<(TransceiverSearch, Transceiver)> {
        let mut send_caps = None;
        for transceiver in self.transceivers.iter() {
            let trans_state = transceiver.imp().state();
            if media
                .mid()
                .is_some_and(|mid| trans_state.mid() == Some(mid.0.as_str()))
            {
                return Some((TransceiverSearch::Mid, transceiver.clone()));
            }
            if media
                .mid()
                .is_some_and(|mid| trans_state.pending_mid() == Some(mid.0.as_str()))
            {
                return Some((TransceiverSearch::PendingMid, transceiver.clone()));
            }
            if trans_state.mline() == Some(idx) {
                return Some((TransceiverSearch::MLine, transceiver.clone()));
            }
            if trans_state.mline().is_none()
                && let Some(caps) = trans_state.send_caps()
            {
                let Some(send_media) = rtp_caps_to_media(&caps) else {
                    continue;
                };
                if media_intersect(&send_media, media).is_none() {
                    continue;
                };
                send_caps.get_or_insert(transceiver);
            }
        }
        if let Some(transceiver) = send_caps {
            return Some((TransceiverSearch::SendCaps, transceiver.clone()));
        }
        None
    }

    fn update_negotiation_needed(&mut self) {
        // if we are closed, abort
        if self.closed {
            gst::trace!(CAT, "we are closed");
            return;
        }
        let Some(op_sender) = self.operations.as_ref().map(|op| op.op_sender.clone()) else {
            gst::trace!(CAT, "no operations loop yet");
            return;
        };
        if self.signaling_status != SignalingStatus::Stable {
            gst::trace!(
                CAT,
                "signaling state is not stable, it is {:?}",
                self.signaling_status
            );
            return;
        }
        if !self.check_if_negotiation_is_needed() {
            gst::trace!(CAT, "no negotiation possible yet");
            self.negotiation_needed = false;
            return;
        }
        if self.negotiation_needed {
            gst::trace!(CAT, "negotiation needed already in progress");
            return;
        }
        self.negotiation_needed = true;
        let _guard = RUNTIME.enter();
        RUNTIME.spawn(async move {
            let _ = op_sender
                .send(ExternalEvent {
                    ty: ExternalEventType::CheckNegotiationNeeded,
                    promise: None,
                })
                .await;
        });
    }

    fn all_sink_pads_have_caps(&self) -> bool {
        self.transceivers.iter().all(|transceiver| {
            let state = transceiver.imp().state();
            state.codec_preferences().is_some()
                || state.send_pad().is_some_and(|pad| {
                    let pad = pad.downcast_ref::<WebRTCSendSinkPad>().unwrap();
                    let state = pad.imp().state();
                    state.received_caps().is_some()
                })
        })
    }

    fn check_if_negotiation_is_needed(&mut self) -> bool {
        if !self.all_sink_pads_have_caps() {
            gst::log!(CAT, "not all sink pads have caps, cannot negotiate yet");
            return false;
        };
        let Some((local_type, local_desc)) = self.current_local_desc.as_ref() else {
            gst::log!(CAT, "no local description set");
            return true;
        };
        let Some((_remote_type, remote_desc)) = self.current_remote_desc.as_ref() else {
            gst::log!(CAT, "no remote description set");
            return true;
        };
        // TODO: check if data channel has been added
        // TODO: check if sink pads have caps

        for transceiver in self.transceivers.iter() {
            let trans_state = transceiver.imp().state();
            let Some(mline) = trans_state.mline() else {
                gst::log!(CAT, "unassociated transceiver {transceiver:?}");
                return true;
            };
            assert!(mline < local_desc.media().len());
            assert!(mline < remote_desc.media().len());
            let local_dir = local_desc.rtp_direction_for_mline(mline);
            let remote_dir = remote_desc.rtp_direction_for_mline(mline);
            match local_type {
                WebRTCSdpType::Offer
                    if local_dir != trans_state.direction()
                        && remote_dir.reverse() != trans_state.direction() =>
                {
                    gst::log!(
                        CAT,
                        "transceiver direction ({:?}) doesn't match description (local {local_dir:?} remote {remote_dir:?} (reversed {:?}))",
                        trans_state.direction(),
                        remote_dir.reverse()
                    );
                    return true;
                }
                WebRTCSdpType::Answer
                    if trans_state.direction().intersect_with_remote(remote_dir) != local_dir =>
                {
                    gst::log!(
                        CAT,
                        "transceiver direction({:?}) doesn't match the new description intersected direction {:?} (prev local {local_dir:?} remote {remote_dir:?}",
                        trans_state.direction(),
                        trans_state.direction().intersect_with_remote(remote_dir)
                    );
                    return true;
                }
                _ => (),
            }
        }

        false
    }

    fn check_negotiation_needed(&mut self) {
        if self.negotiation_needed {
            let Some(op_sender) = self.operations.as_ref().map(|op| op.op_sender.clone()) else {
                return;
            };
            self.negotiation_needed = false;
            let _guard = RUNTIME.enter();
            RUNTIME.spawn(async move {
                let _ = op_sender
                    .send(ExternalEvent {
                        ty: ExternalEventType::EmitNegotiationNeeded,
                        promise: None,
                    })
                    .await;
            });
        }
    }

    fn maybe_start_dtls(&mut self, weak_self: ObjectImplWeakRef<WebRTCSession>, transport_id: u32) {
        let Some(transport) = self
            .transports
            .iter_mut()
            .find(|transport| transport.id == transport_id)
        else {
            return;
        };
        if transport.dtls_task.is_some() {
            return;
        }
        let s = DtlsPollFuture::new(weak_self.clone(), transport_id);
        let send_task = crate::RUNTIME.spawn(async move {
            let mut s = core::pin::pin!(s);
            while let Some(data) = s.next().await {
                let Some(this) = weak_self.upgrade() else {
                    break;
                };
                let component = {
                    let mut state = this.state.lock().unwrap();
                    let ice = state.ice.as_mut().expect("No ICE");
                    let Some(stream) = ice.stream_for_mline(transport_id) else {
                        gst::trace!(CAT, "no ICE stream for transport {transport_id}");
                        break;
                    };
                    let stream = stream.clone();
                    // FIXME: component
                    stream.component(1).unwrap()
                };
                if let Err(e) = component.send(&data.data).await {
                    gst::warning!(CAT, "Failed to send data: {e:?}");
                    break;
                }
            }
        });
        transport.dtls_task = Some(send_task);
    }

    fn dtls_key_material(&mut self, transport_id: u32, material: KeyMaterial, is_client: bool) {
        gst::info!(CAT, "have dtls key material");
        let Some(webrtcrecv) = self.weak_recv.as_ref().and_then(|weak| weak.upgrade()) else {
            gst::fixme!(CAT, "have dtls keying material with no webrtcrecv");
            return;
        };
        let Some(transport) = self
            .transports
            .iter()
            .find(|transport| transport.id == transport_id)
        else {
            return;
        };
        if let Some(fp) = transport.dtls.remote_fingerprint()
            && let Some((_type, sdp)) = self
                .pending_remote_desc
                .as_ref()
                .or(self.current_remote_desc.as_ref())
            && let Some(media) = sdp.media().get(transport_id as usize)
            && sdp
                .fingerprints()
                .iter()
                .all(|sdp_fp| sdp_fp.hash_func == HashFunc::Sha256 && sdp_fp.fingerprint != fp)
            && media
                .fingerprints()
                .iter()
                .all(|sdp_fp| sdp_fp.hash_func == HashFunc::Sha256 && sdp_fp.fingerprint != fp)
        {
            gst::fixme!(CAT, "DTLS fingerprint does not match SDP!");
            return;
        }
        webrtcrecv.imp().key_material(&material, is_client);
        let Some(webrtcsend) = self.weak_send.as_ref().and_then(|weak| weak.upgrade()) else {
            gst::fixme!(CAT, "have dtls keying material with no webrtcsend");
            return;
        };
        let mut webrtcsend_state = webrtcsend.imp().state();
        webrtcsend_state.key_material(&material, is_client);

        for transceiver in self.transceivers.iter() {
            let transceiver_state = transceiver.imp().state();
            if transceiver_state
                .current_direction()
                .is_some_and(|dir| dir.has_send())
                && let Some((sink_pad, block_id)) = transceiver_state
                    .send_pad()
                    .as_ref()
                    .filter(|pad| pad.current_caps().is_some())
                    .and_then(|pad| pad.downcast_ref::<WebRTCSendSinkPad>())
                    .and_then(|pad| {
                        pad.clone()
                            .imp()
                            .state()
                            .take_block_id()
                            .map(|block_id| (pad, block_id))
                    })
            {
                webrtcsend_state.unblock_sink_pad(sink_pad);
                gst::debug!(CAT, obj = sink_pad, "removing blocking probe {block_id:?}");
                sink_pad.remove_probe(block_id);
            }
        }
    }

    pub(crate) fn set_webrtc_send(
        &mut self,
        webrtc_send: glib::WeakRef<crate::webrtcsend::WebRTCSend>,
    ) {
        self.weak_send = Some(webrtc_send);
    }

    pub(crate) fn set_webrtc_recv(
        &mut self,
        webrtc_recv: glib::WeakRef<crate::webrtcrecv::WebRTCRecv>,
    ) {
        self.weak_recv = Some(webrtc_recv);
    }
}

#[derive(Debug)]
struct Operations {
    _operations: JoinHandle<()>,
    op_sender: tokio::sync::mpsc::Sender<ExternalEvent>,
}

#[derive(Debug)]
struct ExternalEvent {
    ty: ExternalEventType,
    promise: Option<gst::Promise>,
}

#[derive(Debug)]
enum ExternalEventType {
    CreateOffer {
        options: Option<gst::Structure>,
    },
    CreateAnswer {
        options: Option<gst::Structure>,
    },
    LocalDescription {
        typ: String,
        sdp: Option<String>,
    },
    RemoteDescription {
        typ: String,
        sdp: String,
    },
    IceCandidate {
        mlineindex: u32,
        mid: Option<String>,
        candidate: String,
    },

    // Internal tasks
    CheckNegotiationNeeded,
    EmitNegotiationNeeded,
}

#[derive(Copy, Clone, Debug, glib::ErrorDomain, PartialEq, Eq, thiserror::Error)]
#[error_domain(name = "GstWebRTCBin2Error")]
enum WebRTCError {
    #[error("Not in the correct state for this operation")]
    InvalidState,
    #[error("The string did not match the expected pattern")]
    SyntaxError,
    #[error("Operation is not supported")]
    InvalidAccessError,
}

#[track_caller]
fn resolve_promise_with<F: Fn() -> Option<(&'static str, glib::SendValue)>>(
    promise: Option<gst::Promise>,
    f: F,
) {
    if let Some(promise) = promise {
        let ret = f();
        promise.reply(ret.map(|(name, value)| {
            gst::Structure::builder(PROMISE_REPLY_NAME)
                .field(name, value)
                .build()
        }));
    }
}

#[track_caller]
fn resolve_promise_with_error<F: Fn() -> glib::Error>(promise: Option<gst::Promise>, f: F) {
    let err = f();
    gst::warning!(CAT, "resolving promise with error: {err}");
    resolve_promise_with(promise, || Some(("error", err.to_send_value())));
}

impl WebRTCSession {
    fn create_offer(&self, options: Option<gst::Structure>, promise: Option<gst::Promise>) {
        gst::info!(CAT, imp = self, "creating offer with options: {options:?}");
        let mut state = self.state.lock().unwrap();
        let mut desc = Session::builder()
            .origin(Origin {
                username: Some("-".to_string()),
                sess_id: (rand::random::<u64>() & 0x7fff_ffff_ffff_ffff).to_string(),
                sess_version: 0,
                nettype: NetType::In,
                addrtype: AddrType::Ip4,
                unicast_address: "0.0.0.0".to_string(),
            })
            .times(vec![Time {
                start_time: 0,
                stop_time: 0,
                repeats: vec![],
            }])
            .ice_options(Some(IceOptions::new(["trickle"])))
            .build();

        gst::debug!(CAT, imp = self, "transceivers: {:?}", state.transceivers);

        let mut idx = 0;
        let transport_idx = 0;
        let mut new_dtls = vec![];
        let mut medias = vec![];
        for transceiver in state.transceivers.iter() {
            let Some(mut new_media) = transceiver.imp().generate_offer_media(idx) else {
                gst::debug!(
                    CAT,
                    imp = self,
                    "Could not generate offer media for transceiver {transceiver:?}. Ignoring"
                );
                continue;
            };
            new_media.set_proto(TransportProto::UdpTlsRtpSavpf);
            new_media.set_connection(Some(Connection::from_ip_addr(
                std::net::Ipv4Addr::UNSPECIFIED,
            )));
            let mid = idx.to_string();
            new_media.set_mid(Some(Mid(mid.clone())));
            let dtls = if let Some(transport) = state
                .transports
                .iter()
                .find(|transport| transport.id == transport_idx)
            {
                &transport.dtls
            } else {
                new_dtls.push((transport_idx, TlsImpl::new()));
                new_dtls.last().map(|(_idx, dtls)| dtls).unwrap()
            };
            // RFC 8843 section 7.1.3 and RFC 8829 section 5.2.1 put
            // a=fingerprint only in the BUNDLE-tagged m-line. Firefox
            // crashes on m-lines without one, so add it to every m-line.
            new_media
                .fingerprints_mut()
                .push(Fingerprint::with_fingerprint(
                    HashFunc::Sha256,
                    dtls.local_fingerprint(),
                ));
            if idx == transport_idx {
                new_media.set_bundle_only(false);
                new_media.set_port(9);
                new_media.set_setup(
                    dtls.is_client()
                        .map(|client| {
                            if client {
                                Setup::Active
                            } else {
                                Setup::Passive
                            }
                        })
                        .or(Some(Setup::ActPass)),
                );
                new_media.set_ice_ufrag(Some(IceUfrag(librice::random_string(8))));
                new_media.set_ice_pwd(Some(IcePwd(librice::random_string(32))));
            } else {
                new_media.set_bundle_only(true);
                new_media.set_port(0);
                new_media.set_rtcp_mux(false);
                new_media.set_rtcp_mux_only(false);
                new_media.set_rtcp_rsize(false);
                new_media.set_rtcp(None);
                new_media.set_crypto(vec![]);
                new_media.set_key_mgmt(vec![]);
                new_media.set_other_attributes(vec![]);
                new_media.set_ice_ufrag(None);
                new_media.set_ice_pwd(None);
                new_media.set_ice_options(None);
                new_media.set_setup(None);
                new_media.set_candidates(vec![]);
                new_media.set_end_of_candidates(false);
            }
            gst::trace!(CAT, imp = self, "generated media: {new_media:?}");
            medias.push(new_media);
            desc.add_bundle_tag(&mid);
            idx += 1;
        }
        desc.set_media(medias);
        for (idx, dtls) in new_dtls {
            let ice = state.ice.as_mut().expect("No ICE");
            let stream = ice
                .mline_stream_id_map
                .entry(idx)
                .or_insert_with(|| ice.agent.add_stream());
            let stream_id = stream.id();
            state.transports.push(Transport {
                id: idx,
                ice_stream_id: stream_id,
                dtls,
                dtls_task: None,
                dtls_waker: None,
                expected_remote_fingerprint: vec![],
            });
        }
        drop(state);
        resolve_promise_with(promise, || {
            Some(("sdp", desc.to_sdp_string().to_send_value()))
        });
    }

    fn create_answer(&self, options: Option<gst::Structure>, promise: Option<gst::Promise>) {
        gst::info!(CAT, imp = self, "creating answer with options {options:?}");
        let mut state = self.state.lock().unwrap();
        let Some((_typ, pending_remote)) = state.pending_remote_desc.as_ref() else {
            drop(state);
            resolve_promise_with_error(promise, || {
                glib::Error::new(
                    WebRTCError::InvalidState,
                    "No remote description to create an answer from",
                )
            });
            return;
        };
        let mut desc = Session::builder()
            .origin(Origin {
                username: Some("-".to_string()),
                sess_id: pending_remote.origin().sess_id.clone(),
                sess_version: 0,
                nettype: NetType::In,
                addrtype: AddrType::Ip4,
                unicast_address: "0.0.0.0".to_string(),
            })
            .times(vec![Time {
                start_time: 0,
                stop_time: 0,
                repeats: vec![],
            }])
            .ice_options(Some(IceOptions::new(["trickle"])))
            .build();
        let mut new_dtls = vec![];
        let mut medias = vec![];
        let bundle_idx = select_bundle_tag(pending_remote);
        let bundle_tags = pending_remote.bundle_tags();
        let mut bundled_mids = vec![];
        for (idx, media) in pending_remote.media().iter().enumerate() {
            let in_offer_group =
                bundle_idx.is_some() && media.mid().is_some_and(|mid| bundle_tags.contains(&mid.0));
            let is_bundled = in_offer_group && !is_rejected_media(media);
            let is_tagged = is_bundled && bundle_idx == Some(idx);
            let is_rejected = is_rejected_media(media) || (media.bundle_only() && !is_bundled);
            gst::trace!(
                CAT,
                "creating answer media for idx {idx} bundled {is_bundled} tagged {is_tagged} rejected {is_rejected} media: {media:?}"
            );

            if is_rejected {
                // Reject the media line: zero port, no transport of its own
                // and not part of the BUNDLE group (RFC 8843 section 7.3.3).
                let new_media = Media::builder(media.media())
                    .port(0)
                    .proto(media.proto().clone())
                    .formats(media.formats().to_vec())
                    .fmt_raw(media.fmt_raw().map(str::to_owned))
                    .mid(media.mid().cloned())
                    .connection(Some(Connection::from_ip_addr(
                        std::net::Ipv4Addr::UNSPECIFIED,
                    )))
                    .build();
                medias.push(new_media);
                continue;
            }

            let mut new_media = match state.transceiver_for_sdp_media(media, idx).and_then(
                |(_search, transceiver)| transceiver.imp().generate_offer_media(idx as u32),
            ) {
                Some(local_media) => answer_media(media, local_media),
                None => answer_media(media, answer_media_from_offer(media)),
            };
            new_media.set_mid(media.mid().cloned());
            new_media.set_candidates(vec![]);
            new_media.set_end_of_candidates(false);
            new_media.set_remote_candidates(vec![]);
            new_media.set_ice_options(None);
            new_media.set_proto(TransportProto::UdpTlsRtpSavpf);
            new_media.set_connection(Some(Connection::from_ip_addr(
                std::net::Ipv4Addr::UNSPECIFIED,
            )));
            new_media.set_bundle_only(false);

            let dtls = if let Some(transport) = state
                .transports
                .iter()
                .find(|transport| transport.id == idx as u32)
            {
                &transport.dtls
            } else {
                new_dtls.push((idx, TlsImpl::new()));
                new_dtls.last().map(|(_idx, dtls)| dtls).unwrap()
            };
            // Add a fingerprint to every m-line, see create_offer().
            new_media.set_fingerprints(vec![Fingerprint::with_fingerprint(
                HashFunc::Sha256,
                dtls.local_fingerprint(),
            )]);

            if is_bundled && !is_tagged {
                // Bundled media other than the tagged section only get a zero
                // port and a=bundle-only (RFC 8843 section 7.1.3).
                new_media.set_port(0);
                new_media.set_bundle_only(true);
                new_media.set_ice_ufrag(None);
                new_media.set_ice_pwd(None);
                new_media.set_setup(None);
                new_media.set_rtcp_mux(false);
                new_media.set_rtcp_mux_only(false);
                new_media.set_rtcp_rsize(false);
                new_media.set_rtcp(None);
                new_media.set_crypto(vec![]);
                new_media.set_key_mgmt(vec![]);
                new_media.set_other_attributes(vec![]);
                if let Some(mid) = media.mid() {
                    bundled_mids.push(mid.0.clone());
                }
                medias.push(new_media);
                continue;
            }

            new_media.set_port(9);
            new_media.set_ice_ufrag(Some(IceUfrag(librice::random_string(4))));
            new_media.set_ice_pwd(Some(IcePwd(librice::random_string(32))));
            new_media.set_setup(
                dtls.is_client()
                    .map(|client| {
                        if client {
                            Setup::Active
                        } else {
                            Setup::Passive
                        }
                    })
                    .or(Some(answer_setup(media.setup()))),
            );
            medias.push(new_media);
        }
        if let Some(bundle_idx) = bundle_idx
            && let Some(mid) = pending_remote.media()[bundle_idx].mid()
        {
            // RFC 8843 section 7.3.4: the answerer BUNDLE-tag comes first.
            desc.add_bundle_tag(&mid.0);
            for tag in bundle_tags {
                if tag != &mid.0 && bundled_mids.contains(tag) {
                    desc.add_bundle_tag(tag);
                }
            }
        }
        desc.set_media(medias);
        for (idx, dtls) in new_dtls {
            let ice = state.ice.as_mut().expect("No ICE");
            let stream = ice
                .mline_stream_id_map
                .entry(idx as u32)
                .or_insert_with(|| ice.agent.add_stream());
            let stream_id = stream.id();
            state.transports.push(Transport {
                id: idx as u32,
                ice_stream_id: stream_id,
                dtls,
                dtls_task: None,
                dtls_waker: None,
                expected_remote_fingerprint: vec![],
            });
        }
        drop(state);
        resolve_promise_with(promise, || {
            Some(("sdp", desc.to_sdp_string().to_send_value()))
        });
    }

    fn set_local_description(
        &self,
        typ: String,
        sdp: Option<String>,
        promise: Option<gst::Promise>,
    ) {
        let Ok(typ) = WebRTCSdpType::from_str(&typ) else {
            gst::error!(CAT, imp = self, "Unknown SDP type {typ}");
            resolve_promise_with_error(promise, || {
                glib::Error::new(WebRTCError::SyntaxError, "Unknown SDP type {typ}")
            });
            return;
        };
        let Some(sdp) = sdp else {
            resolve_promise_with_error(promise, || {
                glib::Error::new(
                    WebRTCError::InvalidAccessError,
                    "NULL local-description is not currently supported",
                )
            });
            return;
        };
        self.set_description(typ, sdp, false, promise);
    }

    fn set_remote_description(&self, typ: String, sdp: String, promise: Option<gst::Promise>) {
        let Ok(typ) = WebRTCSdpType::from_str(&typ) else {
            gst::error!(CAT, imp = self, "Unknown SDP type {typ}");
            resolve_promise_with_error(promise, || {
                glib::Error::new(WebRTCError::SyntaxError, "Unknown SDP type {typ}")
            });
            return;
        };
        self.set_description(typ, sdp, true, promise);
    }

    fn set_description(
        &self,
        typ: WebRTCSdpType,
        sdp: String,
        is_remote: bool,
        promise: Option<gst::Promise>,
    ) {
        let mut state = self.state.lock().unwrap();
        let signaling_status = state.signaling_status;
        if !signaling_status.is_correct_for_desc_type(typ, is_remote) {
            drop(state);
            resolve_promise_with_error(promise, || {
                glib::Error::new(
                    WebRTCError::InvalidState,
                    &format!(
                        "Signaling state {signaling_status:?} incorrect for setting a local description of type {typ}"
                    ),
                )
            });
            return;
        }
        if typ == WebRTCSdpType::Rollback
            && [SignalingStatus::Stable, SignalingStatus::HaveLocalPrAnswer]
                .contains(&state.signaling_status)
        {
            drop(state);
            resolve_promise_with_error(promise, || {
                glib::Error::new(
                    WebRTCError::InvalidAccessError,
                    &format!(
                        "Rollback or provisional SDP provided in the wrong signaling state {signaling_status:?}",
                    ),
                )
            });
            return;
        }
        let remote = if is_remote { "remote" } else { "local" };

        gst::trace!(
            CAT,
            imp = self,
            "Attempting to set {remote} {typ:?} SDP: {sdp}"
        );

        let sdp = match parse_webrtc_sdp(&sdp) {
            Ok(sdp) => sdp,
            Err(e) => {
                drop(state);
                resolve_promise_with_error(promise, || {
                    glib::Error::new(
                        WebRTCError::InvalidAccessError,
                        &format!("Failed to parse the {remote} description: {e}",),
                    )
                });
                return;
            }
        };

        if is_remote {
            match typ {
                WebRTCSdpType::Offer => {
                    state.pending_remote_desc = Some((typ, sdp.clone()));
                    state.signaling_status = SignalingStatus::HaveRemoteOffer;
                }
                WebRTCSdpType::Answer => {
                    state.current_remote_desc = Some((typ, sdp.clone()));
                    state.current_local_desc = state.pending_local_desc.take();
                    state.signaling_status = SignalingStatus::Stable;
                }
                WebRTCSdpType::PrAnswer | WebRTCSdpType::Rollback => {
                    drop(state);
                    resolve_promise_with_error(promise, || {
                        glib::Error::new(
                            WebRTCError::InvalidAccessError,
                            "Rollbacks and provisional answers are unsupported",
                        )
                    });
                    return;
                }
            }
        } else {
            match typ {
                WebRTCSdpType::Offer => {
                    state.pending_local_desc = Some((typ, sdp.clone()));
                    state.signaling_status = SignalingStatus::HaveLocalOffer;
                }
                WebRTCSdpType::Answer => {
                    state.current_local_desc = Some((typ, sdp.clone()));
                    state.current_remote_desc = state.pending_remote_desc.take();
                    state.signaling_status = SignalingStatus::Stable;
                }
                WebRTCSdpType::PrAnswer | WebRTCSdpType::Rollback => {
                    drop(state);
                    resolve_promise_with_error(promise, || {
                        glib::Error::new(
                            WebRTCError::InvalidAccessError,
                            "Rollbacks and provisional answers are unsupported",
                        )
                    });
                    return;
                }
            }
        }
        let stun_servers = state.stun_servers.clone();
        let turn_servers = state.turn_servers.clone();
        // As the answerer, select the offerer-tagged media line ourselves
        // (RFC 8843 section 7.3.1). Otherwise the first BUNDLE-tag is the
        // tagged line.
        let bundle_idx = if is_remote && typ == WebRTCSdpType::Offer {
            select_bundle_tag(&sdp)
        } else {
            sdp.bundle_idx()
        };
        let bundle_tags = sdp.bundle_tags();
        for (i, media) in sdp.media().iter().enumerate() {
            let in_group =
                bundle_idx.is_some() && media.mid().is_some_and(|mid| bundle_tags.contains(&mid.0));
            let is_bundled = in_group && !is_rejected_media(media);
            let is_rejected = is_rejected_media(media) || (media.bundle_only() && !is_bundled);
            if is_rejected {
                continue;
            }
            let transport_id = if is_bundled {
                bundle_idx.unwrap() as u32
            } else {
                i as u32
            };
            let provides_transport = !is_bundled || Some(i) == bundle_idx;
            let ice = state.ice.as_mut().expect("No ICE");
            let stream = ice
                .mline_stream_id_map
                .entry(transport_id)
                .or_insert_with(|| ice.agent.add_stream());
            if stream.component(librice::component::RTP).is_none() {
                let component = stream.add_component().unwrap();
                let weak_self = self.downgrade();
                crate::RUNTIME.spawn(async move {
                    let mut recv = component.recv();
                    let mut valid_fingerprint = false;
                    while let Some(recv) = recv.next().await {
                        gst::trace!(CAT, "received {} bytes from ICE", recv.len());
                        if recv.is_empty() {
                            continue;
                        }
                        let Some(this) = weak_self.upgrade() else {
                            break;
                        };
                        let mut state = this.state.lock().unwrap();
                        let Some(dtls) = state
                            .transports
                            .iter_mut()
                            .find(|transport| transport.id == transport_id)
                        else {
                            gst::trace!(CAT, "no transport with id {transport_id}");
                            continue;
                        };
                        if is_rtp(recv[0]) {
                            if !valid_fingerprint {
                                gst::debug!(
                                    CAT,
                                    "No remote fingerprint yet, cannot forward incoming RTP data"
                                );
                                continue;
                            }
                            let Some(webrtcrecv) =
                                state.weak_recv.as_ref().and_then(|weak| weak.upgrade())
                            else {
                                gst::fixme!(
                                    CAT,
                                    imp = this,
                                    "have srtp of {} bytes with no webrtcrecv",
                                    recv.len()
                                );
                                continue;
                            };
                            webrtcrecv.imp().recv_srtp(recv.to_vec());
                        } else if dtls.dtls.is_client().is_some()
                            && let Some(app_data) = dtls.dtls.handle_incoming(&recv).unwrap()
                        {
                            if !valid_fingerprint {
                                gst::debug!(
                                    CAT,
                                    "No remote fingerprint yet, cannot forward incoming RTP data"
                                );
                                continue;
                            }
                            gst::fixme!(
                                CAT,
                                imp = this,
                                "have dtls application data of {} bytes",
                                app_data.len()
                            );
                            let Some(_webrtcrecv) =
                                state.weak_recv.as_ref().and_then(|weak| weak.upgrade())
                            else {
                                gst::fixme!(
                                    CAT,
                                    imp = this,
                                    "have dtls application data of {} bytes with no webrtcrecv",
                                    app_data.len()
                                );
                                continue;
                            };
                        } else {
                            if let Some(waker) = dtls.dtls_waker.take() {
                                waker.wake()
                            }
                            if !valid_fingerprint
                                && let Some(remote_fp) = dtls.dtls.remote_fingerprint()
                                && dtls
                                    .expected_remote_fingerprint
                                    .iter()
                                    .any(|expected| expected.fingerprint == remote_fp)
                            {
                                gst::info!(CAT, "found valid remote dtls certificate fingerprint");
                                valid_fingerprint = true;
                            }
                        }
                    }
                });
            }
            let transport_media = &sdp.media()[transport_id as usize];
            let credentials = Credentials::new(
                transport_media.ice_ufrag().unwrap().0.as_str(),
                transport_media.ice_pwd().unwrap().0.as_str(),
            );
            if is_remote {
                stream.set_remote_credentials(&credentials);
                if provides_transport {
                    for cand in media.candidates().iter() {
                        let Ok(cand) = Candidate::from_sdp_string(&format!("a=candidate:{cand}"))
                        else {
                            gst::warning!(
                                CAT,
                                imp = self,
                                "Ignoring unparsable candidate: {cand:?}"
                            );
                            continue;
                        };
                        stream.add_remote_candidate(&cand);
                    }
                }
            } else if provides_transport {
                for stun in stun_servers.iter() {
                    let Ok(addr) = IpAddr::from_str(&stun.addr) else {
                        // TODO: wait for dns resolution to complete.
                        gst::warning!(
                            CAT,
                            "DNS resolution of STUN server {} is not complete yet. Ignored",
                            stun.addr
                        );
                        continue;
                    };
                    let transport = match stun.scheme {
                        StunScheme::Stun => TransportType::Udp,
                        // STUN over TCP-TLS is not currently supported
                        StunScheme::Stuns => return,
                    };
                    ice.agent
                        .add_stun_server(transport, SocketAddr::new(addr, stun.port));
                }

                for turn in turn_servers.iter() {
                    if turn.scheme == TurnScheme::Turns {
                        // TURN over TLS is not currently supported
                        continue;
                    }

                    let Ok(addr) = IpAddr::from_str(&turn.addr) else {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "DNS resolution of TURN server {} is not complete yet. Ignored",
                            turn.addr
                        );
                        continue;
                    };

                    let credentials = TurnCredentials::new(&turn.username, &turn.password);

                    for transport in &turn.transports {
                        let transport = match transport {
                            TurnTransport::Udp => TransportType::Udp,
                            TurnTransport::Tcp => TransportType::Tcp,
                        };

                        gst::debug!(
                            CAT,
                            imp = self,
                            "Adding turn server with addr: {addr}, port: {}",
                            turn.port
                        );

                        ice.agent.add_turn_server(TurnConfig::new(
                            transport,
                            SocketAddr::new(addr, turn.port).into(),
                            credentials.clone(),
                        ));
                    }
                }

                stream.set_local_credentials(&credentials);
                let stream = stream.clone();
                crate::RUNTIME.spawn(async move {
                    gst::trace!(CAT, "gathering local candidates");
                    stream.gather_candidates().await
                });
            }
        }
        let new_signaling_status = state.signaling_status;
        let mut pt_map = gst::Structure::builder("application/x-rtp-pt-map");
        let mut pending_transceivers = vec![];
        if new_signaling_status == SignalingStatus::Stable {
            for (idx, media) in sdp.media().iter().enumerate() {
                let in_group = bundle_idx.is_some()
                    && media.mid().is_some_and(|mid| bundle_tags.contains(&mid.0));
                let is_bundled = in_group && !is_rejected_media(media);
                let is_rejected = is_rejected_media(media) || (media.bundle_only() && !is_bundled);
                if is_rejected {
                    continue;
                }
                let transport_id = if is_bundled { bundle_idx.unwrap() } else { idx };
                if is_datachannel(media) {
                    // FIXME data channel
                } else {
                    let trans = match state.transceiver_for_sdp_media(media, idx) {
                        Some((_, transceiver)) => transceiver,
                        None => {
                            let transceiver = glib::Object::new::<Transceiver>();
                            gst::trace!(
                                CAT,
                                imp = self,
                                "Constructing new recvonly transceiver {transceiver:?}"
                            );
                            let mut trans_state = transceiver.imp().state();
                            trans_state.set_direction(Direction::RecvOnly);
                            pending_transceivers.push(transceiver.clone());
                            drop(trans_state);
                            transceiver
                        }
                    };

                    let local_sdp = state.current_local_desc.as_ref().unwrap().clone();
                    let remote_sdp = state.current_remote_desc.as_ref().unwrap().clone();
                    let local_media = &local_sdp.1.media()[idx];
                    let remote_media = &remote_sdp.1.media()[idx];
                    if idx == transport_id {
                        let mut remote_fingerprints = remote_media.fingerprints().to_vec();
                        remote_fingerprints.extend_from_slice(remote_sdp.1.fingerprints());

                        let local_setup = local_media.setup().or(local_sdp.1.setup());
                        let remote_setup = remote_media.setup().or(remote_sdp.1.setup());
                        let Some(final_setup) = intersect_setup(local_setup, remote_setup) else {
                            resolve_promise_with_error(promise, || {
                                glib::Error::new(
                                    WebRTCError::SyntaxError,
                                    &format!(
                                        "Cannot intersect DTLS setup values for media {idx}, local: {local_setup:?}, remote: {remote_setup:?}"
                                    ),
                                )
                            });
                            return;
                        };

                        let Some(transport) = state
                            .transports
                            .iter_mut()
                            .find(|transport| transport.id == transport_id as u32)
                        else {
                            // All previous code paths should have resulted in a transport
                            unreachable!();
                        };
                        transport.dtls.set_client(final_setup == Setup::Active);
                        transport.expected_remote_fingerprint = remote_fingerprints;
                    }

                    let mut trans_state = trans.imp().state();
                    trans_state.update_from_sdp(&local_sdp.1, idx, &remote_sdp.1);

                    for format in local_media.formats().iter() {
                        let Some(rtpmap) = local_media.rtpmaps().get(format) else {
                            continue;
                        };
                        let media_str = match media.media() {
                            MediaType::Audio => "audio",
                            MediaType::Video => "video",
                            MediaType::Application => "application",
                            _ => unreachable!(),
                        };
                        let caps = gst::Caps::builder("application/x-rtp")
                            .field("payload", *format as i32)
                            .field("clock-rate", rtpmap.clock_rate as i32)
                            .field("encoding-name", rtpmap.encoding_name.to_ascii_uppercase())
                            .field("media", media_str)
                            .build();
                        pt_map = pt_map.field(format.to_string(), caps);
                    }
                }
            }
        }
        state.transceivers.extend(pending_transceivers);
        let pt_map = pt_map.build();
        gst::debug!(CAT, "constructed pt-map {pt_map}");
        if let Some(rtp_session) = state.rtp_session.as_ref() {
            rtp_session.set_property("pt-map", pt_map);
        } else {
            gst::warning!(
                CAT,
                "No rtp session configured, cannot set pt-map, sending RTP may not work"
            );
        }
        drop(state);

        if let Some(promise) = promise {
            promise.reply(None);
        }
        if signaling_status != new_signaling_status {
            self.obj().notify("signaling-state");
        }
    }

    fn add_ice_candidate(
        &self,
        mlineindex: u32,
        _mid: Option<String>,
        mut candidate: String,
        promise: Option<gst::Promise>,
    ) {
        let candidate = if candidate.is_empty() {
            None
        } else {
            if candidate.starts_with("candidate") {
                let mut new_candidate = "a=".to_string();
                new_candidate.push_str(&candidate);
                candidate = new_candidate;
            }
            let Ok(candidate) = Candidate::from_sdp_string(&candidate) else {
                resolve_promise_with_error(promise, || {
                    glib::Error::new(WebRTCError::SyntaxError, "Failed to parse candidate")
                });
                return;
            };
            Some(candidate)
        };

        let mut inner = self.state.lock().unwrap();
        let Some(ice) = inner.ice.as_mut() else {
            resolve_promise_with_error(promise, || {
                glib::Error::new(WebRTCError::InvalidState, "No ICE agent yet")
            });
            return;
        };
        let Some(stream) = ice.mline_stream_id_map.get_mut(&mlineindex) else {
            resolve_promise_with_error(promise, || {
                glib::Error::new(WebRTCError::InvalidState, "Unknown mline")
            });
            return;
        };
        if let Some(candidate) = candidate {
            stream.add_remote_candidate(&candidate);
        } else {
            stream.end_of_remote_candidates();
        }
    }

    fn add_stun_server(&self, stun_server: &str) {
        let weak_self = self.downgrade();
        let mut state = self.state.lock().unwrap();
        let Some(serv) = parse_stun_server(stun_server) else {
            gst::warning!(CAT, "Failed to parse stun server: {stun_server}");
            return;
        };
        state.stun_servers.push(serv.clone());
        if let Ok(addr) = IpAddr::from_str(&serv.addr) {
            if let Some(ice) = state.ice.as_ref() {
                let transport = match serv.scheme {
                    StunScheme::Stun => TransportType::Udp,
                    // STUN over TCP-TLS is not currently supported
                    StunScheme::Stuns => return,
                };
                ice.agent
                    .add_stun_server(transport, SocketAddr::new(addr, serv.port));
            }
        } else {
            resolve_server_dns(
                serv.clone(),
                serv.addr.clone(),
                serv.port,
                weak_self,
                |state| &mut state.stun_servers,
            );
        }
    }

    fn add_turn_server(&self, turn_server: &str) {
        let weak_self = self.downgrade();
        let mut state = self.state.lock().unwrap();
        let Some(serv) = parse_turn_server(turn_server) else {
            gst::warning!(
                CAT,
                imp = self,
                "Failed to parse turn server: {turn_server}"
            );
            return;
        };

        state.turn_servers.push(serv.clone());

        if let Ok(addr) = IpAddr::from_str(&serv.addr) {
            if let Some(ice) = state.ice.as_ref() {
                if serv.scheme == TurnScheme::Turns {
                    gst::warning!(CAT, imp = self, "TURN over TCP + TLS not supported");
                    return;
                }

                let credentials = TurnCredentials::new(&serv.username, &serv.password);

                for transport in &serv.transports {
                    let transport = match transport {
                        TurnTransport::Udp => TransportType::Udp,
                        TurnTransport::Tcp => TransportType::Tcp,
                    };

                    gst::debug!(
                        CAT,
                        imp = self,
                        "Adding turn server with addr: {addr}, port: {}",
                        serv.port
                    );

                    ice.agent.add_turn_server(TurnConfig::new(
                        transport,
                        SocketAddr::new(addr, serv.port).into(),
                        credentials.clone(),
                    ));
                }
            }
        } else {
            resolve_server_dns(
                serv.clone(),
                serv.addr.clone(),
                serv.port,
                weak_self,
                |state| &mut state.turn_servers,
            );
        }
    }

    fn on_ice_candidate(&self, mlineindex: u32, mid: Option<String>, candidate: String) {
        gst::debug!(
            CAT,
            imp = self,
            "Have local candidate for mline {mlineindex}, mid: {mid:?}: {candidate:}"
        );
        self.obj()
            .emit_by_name::<()>("on-ice-candidate", &[&mlineindex, &mid, &candidate]);
    }

    pub fn check_negotiation_needed(&self) {
        self.ensure_operations();
        self.state.lock().unwrap().update_negotiation_needed();
    }

    fn ensure_operations(&self) -> Option<tokio::sync::mpsc::Sender<ExternalEvent>> {
        let weak_self = self.downgrade();
        let inner = self.state.lock().unwrap();

        if let Some(op) = inner.operations.as_ref() {
            return Some(op.op_sender.clone());
        }
        drop(inner);

        std::thread::scope(move |scope| {
            scope
                .spawn(move || {
                    let this = weak_self.upgrade()?;
                    let mut inner = this.state.lock().unwrap();

                    // librice uses this runtime to spawn IO/sleep tasks.
                    let _guard = crate::RUNTIME.enter();

                    let agent = Agent::builder().trickle_ice(true).build();
                    inner.ice = Some(Ice {
                        agent: agent.clone(),
                        mline_stream_id_map: Default::default(),
                    });

                    let mut ice_msg = agent.messages();

                    let _ice_task = crate::RUNTIME.spawn({
                        let weak_self = weak_self.clone();
                        async move {
                            while let Some(msg) = ice_msg.next().await {
                                let Some(this) = weak_self.upgrade() else {
                                    break;
                                };
                                let mut inner = this.state.lock().unwrap();
                                let Some(ice) = inner.ice.as_ref() else {
                                    gst::warning!(CAT, "No ICE yet!");
                                    continue;
                                };
                                match msg {
                                    AgentMessage::GatheredCandidate(stream, gathered) => {
                                        let Some(mline) = ice.mline_for_stream(stream.id()) else {
                                            gst::warning!(
                                                CAT,
                                                "No mline for stream id {}",
                                                stream.id()
                                            );
                                            continue;
                                        };
                                        let candidate = gathered.candidate();
                                        stream.add_local_gathered_candidate(gathered);
                                        drop(inner);
                                        this.on_ice_candidate(
                                            mline,
                                            None,
                                            candidate.to_sdp_string(),
                                        );
                                    }
                                    AgentMessage::GatheringComplete(component) => {
                                        let Some(mline) =
                                            ice.mline_for_stream(component.stream().id())
                                        else {
                                            gst::warning!(
                                                CAT,
                                                "No mline for stream id {}",
                                                component.stream().id()
                                            );
                                            continue;
                                        };
                                        drop(inner);
                                        this.on_ice_candidate(mline, None, "".to_string());
                                    }
                                    // TODO: update some state properties
                                    AgentMessage::ComponentStateChange(component, new_state) => {
                                        gst::fixme!(
                                            CAT,
                                            "component {} changed state to {new_state:?}",
                                            component.id()
                                        );
                                        // TODO: update ice-connection-state, peer-connection-state,
                                        // etc

                                        if new_state == ComponentConnectionState::Connected {
                                            let transport_id = inner
                                                .transports
                                                .iter()
                                                .find(|transport| {
                                                    transport.ice_stream_id
                                                        == component.stream().id()
                                                })
                                                .unwrap()
                                                .id;
                                            inner.maybe_start_dtls(weak_self.clone(), transport_id);
                                        }
                                    }
                                }
                            }
                        }
                    });

                    let (sender, mut receiver) = tokio::sync::mpsc::channel::<ExternalEvent>(8);
                    let task = crate::RUNTIME.spawn(async move {
                        while let Some(event) = receiver.recv().await {
                            let Some(this) = weak_self.upgrade() else {
                                return;
                            };
                            match event.ty {
                                ExternalEventType::LocalDescription { typ, sdp } => {
                                    this.set_local_description(typ, sdp, event.promise)
                                }
                                ExternalEventType::RemoteDescription { typ, sdp } => {
                                    this.set_remote_description(typ, sdp, event.promise)
                                }
                                ExternalEventType::IceCandidate {
                                    mlineindex,
                                    mid,
                                    candidate,
                                } => this.add_ice_candidate(
                                    mlineindex,
                                    mid,
                                    candidate,
                                    event.promise,
                                ),
                                ExternalEventType::CreateOffer { options } => {
                                    this.create_offer(options, event.promise)
                                }
                                ExternalEventType::CreateAnswer { options } => {
                                    this.create_answer(options, event.promise)
                                }
                                ExternalEventType::CheckNegotiationNeeded => {
                                    this.state().check_negotiation_needed();
                                }
                                ExternalEventType::EmitNegotiationNeeded => {
                                    this.obj().emit_by_name::<()>("on-negotiation-needed", &[]);
                                }
                            }
                        }
                    });

                    inner.operations = Some(Operations {
                        _operations: task,
                        op_sender: sender.clone(),
                    });

                    Some(sender)
                })
                .join()
                .unwrap()
        })
    }

    fn send_external_event(&self, event: ExternalEvent) {
        let Some(sender) = self.ensure_operations() else {
            resolve_promise_with_error(event.promise, || {
                glib::Error::new(WebRTCError::InvalidState, "PeerConnection Closed")
            });
            return;
        };

        crate::RUNTIME.spawn(async move {
            if let Err(err) = sender.send(event).await {
                resolve_promise_with_error(err.0.promise, || {
                    glib::Error::new(WebRTCError::InvalidState, "PeerConnection Closed")
                });
            }
        });
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub(crate) fn send_data(&self, data: &[u8]) {
        let state = self.state.lock().unwrap();
        // FIXME: multiple transports
        if state.transports.is_empty() {
            gst::warning!(CAT, "no transport yet, dropping");
            return;
        }
        let ice_id = state.transports[0].ice_stream_id;
        let Some(ice) = state.ice.as_ref() else {
            gst::warning!(CAT, "no ICE");
            return;
        };
        let Some(stream) = ice.agent.stream(ice_id) else {
            gst::warning!(CAT, "no ICE stream for ID {ice_id}");
            return;
        };
        // FIXME hardcoded component
        let component = stream.component(1).unwrap();
        // FIXME: block_on
        crate::RUNTIME.block_on(async move {
            if let Err(e) = component.send(data).await {
                gst::warning!(CAT, "Failed to send data: {e:?}");
            }
        });
    }
}

fn is_rtp(byte: u8) -> bool {
    byte > 0x7f && byte < 0xc0
}

#[glib::object_subclass]
impl ObjectSubclass for WebRTCSession {
    const NAME: &'static str = "GstWebRTCSession";
    type Type = super::WebRTCSession;
    type ParentType = gst::Object;
}

impl ObjectImpl for WebRTCSession {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("pending-local-description")
                    .nick("The pending local description")
                    .blurb("The local description that is in the process of being negotiated")
                    .read_only()
                    .build(),
                glib::ParamSpecString::builder("pending-remote-description")
                    .nick("The pending remote description")
                    .blurb("The remote description that is in the process of being negotiated")
                    .read_only()
                    .build(),
                glib::ParamSpecString::builder("current-local-description")
                    .nick("The current local description")
                    .blurb("The latest local description that was successfully negotiated and caused the signaling-state to transition to stable")
                    .read_only()
                    .build(),
                glib::ParamSpecString::builder("current-remote-description")
                    .nick("The current remote description")
                    .blurb("The latest remote description that was successfully negotiated and caused the signaling-state to transition to stable")
                    .read_only()
                    .build(),
                glib::ParamSpecEnum::builder::<SignalingStatus>("signaling-state")
                    .nick("Signaling State")
                    .blurb("The signaling state of the WebRTC connection")
                    .read_only()
                    .build(),
            ]
        });
        PROPS.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "current-local-description" => {
                let state = self.state.lock().unwrap();
                state
                    .current_local_desc
                    .as_ref()
                    .map(|(_typ, sdp)| match state.ice.as_ref() {
                        Some(ice) => sdp_with_local_candidates(sdp, ice),
                        None => sdp.to_sdp_string(),
                    })
                    .to_value()
            }
            "current-remote-description" => {
                let state = self.state.lock().unwrap();
                state
                    .current_remote_desc
                    .as_ref()
                    .map(|(_typ, sdp)| match state.ice.as_ref() {
                        Some(ice) => sdp_with_local_candidates(sdp, ice),
                        None => sdp.to_sdp_string(),
                    })
                    .to_value()
            }
            "pending-local-description" => {
                let state = self.state.lock().unwrap();
                state
                    .pending_local_desc
                    .as_ref()
                    .map(|(_typ, sdp)| match state.ice.as_ref() {
                        Some(ice) => sdp_with_local_candidates(sdp, ice),
                        None => sdp.to_sdp_string(),
                    })
                    .to_value()
            }
            "pending-remote-description" => {
                let state = self.state.lock().unwrap();
                state
                    .pending_remote_desc
                    .as_ref()
                    .map(|(_typ, sdp)| match state.ice.as_ref() {
                        Some(ice) => sdp_with_local_candidates(sdp, ice),
                        None => sdp.to_sdp_string(),
                    })
                    .to_value()
            }
            "signaling-state" => self.state.lock().unwrap().signaling_status.to_value(),
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
            vec![
                glib::subclass::Signal::builder("on-negotiation-needed").build(),
                glib::subclass::Signal::builder("add-stun-server")
                    .param_types([String::static_type()])
                    .action()
                    .class_handler(move |args| {
                        let this = args[0].get::<super::WebRTCSession>().unwrap();
                        let stun_server = args[1].get::<&str>().unwrap();
                        this.imp().add_stun_server(stun_server);
                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("add-turn-server")
                    .param_types([String::static_type()])
                    .action()
                    .class_handler(move |args| {
                        let this = args[0].get::<super::WebRTCSession>().unwrap();
                        let turn_server = args[1].get::<&str>().unwrap();
                        this.imp().add_turn_server(turn_server);
                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("create-answer")
                    .param_types([
                        Option::<gst::Structure>::static_type(),
                        Option::<gst::Promise>::static_type(),
                    ])
                    .action()
                    .class_handler(move |args| {
                        let this = args[0].get::<super::WebRTCSession>().unwrap();
                        let options = args[1].get::<Option<gst::Structure>>().unwrap();
                        let promise = args[2].get::<Option<gst::Promise>>().unwrap();
                        this.imp().send_external_event(ExternalEvent {
                            ty: ExternalEventType::CreateAnswer { options },
                            promise,
                        });
                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("create-offer")
                    .param_types([
                        Option::<gst::Structure>::static_type(),
                        Option::<gst::Promise>::static_type(),
                    ])
                    .action()
                    .class_handler(move |args| {
                        let this = args[0].get::<super::WebRTCSession>().unwrap();
                        let options = args[1].get::<Option<gst::Structure>>().unwrap();
                        let promise = args[2].get::<Option<gst::Promise>>().unwrap();
                        this.imp().send_external_event(ExternalEvent {
                            ty: ExternalEventType::CreateOffer { options },
                            promise,
                        });
                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("set-local-description")
                    .param_types([
                        String::static_type(),
                        Option::<String>::static_type(),
                        Option::<gst::Promise>::static_type(),
                    ])
                    .action()
                    .class_handler(move |args| {
                        let this = args[0].get::<super::WebRTCSession>().unwrap();
                        let typ = args[1].get::<String>().unwrap();
                        let sdp = args[2].get::<Option<String>>().unwrap();
                        let promise = args[3].get::<Option<gst::Promise>>().unwrap();
                        this.imp().send_external_event(ExternalEvent {
                            ty: ExternalEventType::LocalDescription { typ, sdp },
                            promise,
                        });
                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("set-remote-description")
                    .param_types([
                        String::static_type(),
                        String::static_type(),
                        Option::<gst::Promise>::static_type(),
                    ])
                    .action()
                    .class_handler(move |args| {
                        let this = args[0].get::<super::WebRTCSession>().unwrap();
                        let typ = args[1].get::<String>().unwrap();
                        let sdp = args[2].get::<String>().unwrap();
                        let promise = args[3].get::<Option<gst::Promise>>().unwrap();
                        this.imp().send_external_event(ExternalEvent {
                            ty: ExternalEventType::RemoteDescription { typ, sdp },
                            promise,
                        });
                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("add-ice-candidate")
                    .param_types([
                        u32::static_type(),
                        Option::<String>::static_type(),
                        String::static_type(),
                        Option::<gst::Promise>::static_type(),
                    ])
                    .action()
                    .class_handler(move |args| {
                        let this = args[0].get::<super::WebRTCSession>().unwrap();
                        let mlineindex = args[1].get::<u32>().unwrap();
                        let mid = args[2].get::<Option<String>>().unwrap();
                        let candidate = args[3].get::<String>().unwrap();
                        let promise = args[4].get::<Option<gst::Promise>>().unwrap();
                        gst::debug!(
                            CAT,
                            obj = this,
                            "Received remote candidate for mline {mlineindex}, mid: {mid:?}: {candidate:}"
                        );
                        this.imp().send_external_event(ExternalEvent {
                            ty: ExternalEventType::IceCandidate {
                                mlineindex,
                                mid,
                                candidate,
                            },
                            promise,
                        });
                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("on-ice-candidate")
                    .param_types([
                        u32::static_type(),
                        Option::<String>::static_type(),
                        String::static_type(),
                    ])
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }
}

impl GstObjectImpl for WebRTCSession {}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum StunScheme {
    Stun,
    Stuns,
}

impl StunScheme {
    fn default_port(&self) -> u16 {
        match self {
            Self::Stun => 3489,
            Self::Stuns => 5349,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StunServer {
    scheme: StunScheme,
    addr: String,
    port: u16,
}

fn parse_stun_server(stun_server: &str) -> Option<StunServer> {
    let serv = url::Url::parse(stun_server).ok()?;
    let scheme = match serv.scheme() {
        "stuns" => StunScheme::Stuns,
        "stun" => StunScheme::Stun,
        _ => return None,
    };
    if let Some(host) = serv.host() {
        let port = serv.port().unwrap_or_else(|| scheme.default_port());
        Some(StunServer {
            scheme,
            addr: host.to_string(),
            port,
        })
    } else {
        // TODO: stun: format in RFC 7064
        None
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum TurnScheme {
    Turn,
    Turns,
}

impl TurnScheme {
    fn default_port(&self) -> u16 {
        match self {
            Self::Turn => 3478,
            Self::Turns => 5349,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum TurnTransport {
    Udp,
    Tcp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnServer {
    scheme: TurnScheme,
    addr: String,
    port: u16,
    username: String,
    password: String,
    transports: Vec<TurnTransport>,
}

fn parse_turn_server(turn_server: &str) -> Option<TurnServer> {
    let serv = url::Url::parse(turn_server).ok()?;
    let scheme = match serv.scheme() {
        "turns" => TurnScheme::Turns,
        "turn" => TurnScheme::Turn,
        _ => return None,
    };

    if serv.username().is_empty() || serv.password().is_none() {
        return None;
    }

    let transports = match serv.query_pairs().find(|(key, _)| key == "transport") {
        Some((_, value)) => match value.as_ref() {
            "udp" => vec![TurnTransport::Udp],
            "tcp" => vec![TurnTransport::Tcp],
            _ => return None,
        },
        None => vec![TurnTransport::Udp, TurnTransport::Tcp],
    };

    if let Some(host) = serv.host() {
        let port = serv.port().unwrap_or_else(|| scheme.default_port());

        Some(TurnServer {
            scheme,
            addr: host.to_string(),
            port,
            username: serv.username().to_string(),
            password: serv.password().unwrap().to_string(),
            transports,
        })
    } else {
        // TODO: turn: format in RFC 7065
        None
    }
}

trait HasAddr {
    fn addr(&self) -> &str;
    fn set_addr(&mut self, addr: String);
}

impl HasAddr for StunServer {
    fn addr(&self) -> &str {
        &self.addr
    }

    fn set_addr(&mut self, addr: String) {
        self.addr = addr;
    }
}

impl HasAddr for TurnServer {
    fn addr(&self) -> &str {
        &self.addr
    }

    fn set_addr(&mut self, addr: String) {
        self.addr = addr;
    }
}

fn resolve_server_dns<T>(
    serv: T,
    serv_addr: String,
    serv_port: u16,
    weak_self: glib::subclass::ObjectImplWeakRef<WebRTCSession>,
    get_servers: impl FnOnce(&mut State) -> &mut Vec<T> + Send + Sync + 'static,
) where
    T: HasAddr + Clone + Send + Sync + 'static,
{
    let _guard = RUNTIME.enter();

    RUNTIME.spawn(async move {
        gst::debug!(CAT, "dns lookup of {serv_addr}");

        let Ok(lookup) = tokio::net::lookup_host(format!("{serv_addr}:{serv_port}"))
            .await
            .inspect_err(|e| gst::warning!(CAT, "Error resolving {serv_addr}: {e:?}"))
        else {
            return;
        };

        let Some(this) = weak_self.upgrade() else {
            return;
        };
        let mut state = this.state.lock().unwrap();
        let servers = get_servers(&mut state);
        let mut handled = false;

        for sock in lookup {
            gst::debug!(CAT, "dns lookup of {serv_addr} resulted in {sock}");

            if !handled {
                let addr = sock.ip().to_string();

                for entry in servers.iter_mut() {
                    if entry.addr() == serv_addr {
                        gst::debug!(
                            CAT,
                            "replacing existing server address {serv_addr} with {addr}",
                        );

                        entry.set_addr(addr.clone());
                        handled = true;
                    }
                }
            }

            if !handled {
                gst::debug!(CAT, "adding server address {}", sock.ip());

                let mut entry = serv.clone();
                entry.set_addr(sock.ip().to_string());
                servers.push(entry);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use sdp_types::{MediaType, RtpMap};

    use super::*;

    #[test]
    fn parse_stun() {
        assert_eq!(
            parse_stun_server("stun://192.168.0.1").unwrap(),
            StunServer {
                scheme: StunScheme::Stun,
                addr: "192.168.0.1".to_string(),
                port: 3489,
            }
        );
        assert_eq!(
            parse_stun_server("stuns://192.168.0.1").unwrap(),
            StunServer {
                scheme: StunScheme::Stuns,
                addr: "192.168.0.1".to_string(),
                port: 5349,
            }
        );
        assert!(parse_stun_server("http://192.168.0.1").is_none())
    }

    #[test]
    fn parse_turn() {
        assert_eq!(
            parse_turn_server("turn://username:password@192.168.0.1:3478").unwrap(),
            TurnServer {
                scheme: TurnScheme::Turn,
                addr: "192.168.0.1".to_string(),
                port: 3478,
                username: "username".to_string(),
                password: "password".to_string(),
                transports: vec![TurnTransport::Udp, TurnTransport::Tcp],
            }
        );
        assert_eq!(
            parse_turn_server("turns://username:password@192.168.0.1:3478").unwrap(),
            TurnServer {
                scheme: TurnScheme::Turns,
                addr: "192.168.0.1".to_string(),
                port: 3478,
                username: "username".to_string(),
                password: "password".to_string(),
                transports: vec![TurnTransport::Udp, TurnTransport::Tcp],
            }
        );
        assert_eq!(
            parse_turn_server("turn://username:password@192.168.0.1:3478?transport=tcp").unwrap(),
            TurnServer {
                scheme: TurnScheme::Turn,
                addr: "192.168.0.1".to_string(),
                port: 3478,
                username: "username".to_string(),
                password: "password".to_string(),
                transports: vec![TurnTransport::Tcp],
            }
        );
        assert_eq!(
            parse_turn_server("turn://username:password@192.168.0.1:3478?transport=udp").unwrap(),
            TurnServer {
                scheme: TurnScheme::Turn,
                addr: "192.168.0.1".to_string(),
                port: 3478,
                username: "username".to_string(),
                password: "password".to_string(),
                transports: vec![TurnTransport::Udp],
            }
        );
        assert!(
            parse_turn_server("turn://username:password@192.168.0.1:3478?transport=foo").is_none()
        );
        assert!(parse_turn_server("turn://192.168.0.1:3478").is_none());
    }

    fn init() {
        gst::init().unwrap();
    }

    #[derive(Debug, Clone)]
    struct ExpectedSdp {
        media: Vec<ExpectedMedia>,
    }

    #[derive(Debug, Clone)]
    struct ExpectedMedia {
        media: MediaType,
        ice_ufrag: Option<bool>,
        ice_pwd: Option<bool>,
        setup: Option<Setup>,
        mid: String,
        bundle_only: bool,
        fingerprints: usize,
        rtp: Option<Media>,
    }

    fn expected_rtp_media(media: MediaType, pt: u8, encoding_name: &str, clock_rate: u32) -> Media {
        let rtpmap = RtpMap::new(pt, encoding_name, clock_rate);
        Media::builder(media)
            .formats(vec![pt])
            .rtcp_mux(true)
            .rtcp_mux_only(true)
            .rtpmaps(BTreeMap::from([(rtpmap.payload_type, rtpmap)]))
            .build()
    }

    impl ExpectedSdp {
        fn match_against(&self, sdp: &Session) {
            assert_eq!(self.media.len(), sdp.media().len());
            for (our_media, their_media) in self.media.iter().zip(sdp.media().iter()) {
                assert_eq!(our_media.media, their_media.media());
                assert!(
                    our_media
                        .ice_ufrag
                        .is_none_or(|exists| exists == their_media.ice_ufrag().is_some())
                );
                assert!(
                    our_media
                        .ice_pwd
                        .is_none_or(|exists| exists == their_media.ice_pwd().is_some())
                );
                assert_eq!(our_media.setup, their_media.setup().cloned());
                assert_eq!(Some(&our_media.mid), their_media.mid().map(|mid| &mid.0));
                assert_eq!(our_media.bundle_only, their_media.bundle_only());
                assert_eq!(our_media.fingerprints, their_media.fingerprints().len());
                if let Some(our_rtp) = our_media.rtp.as_ref() {
                    assert_eq!(our_rtp.direction(), their_media.direction());
                    assert_eq!(our_rtp.rtcp_mux(), their_media.rtcp_mux());
                    assert_eq!(our_rtp.rtcp_mux_only(), their_media.rtcp_mux_only());
                    assert_eq!(our_rtp.rtcp_rsize(), their_media.rtcp_rsize());
                    assert_eq!(our_rtp.rtcp_fb(), their_media.rtcp_fb());
                    assert_eq!(our_rtp.extmap(), their_media.extmap());
                    assert_eq!(our_rtp.formats(), their_media.formats());
                    assert_eq!(our_rtp.rtpmaps(), their_media.rtpmaps());
                    assert_eq!(our_rtp.rtcp_fbs(), their_media.rtcp_fbs());
                    assert_eq!(our_rtp.fmtps(), their_media.fmtps());
                }
            }
        }
    }

    fn l16_caps() -> gst::Caps {
        gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("payload", 11i32)
            .field("clock-rate", 44100i32)
            .field("encoding-name", "L16")
            .field("channels", 1)
            .build()
    }

    #[test]
    fn session_audio() {
        init();

        let session = glib::Object::new::<super::super::WebRTCSession>();
        session.imp().ensure_operations().unwrap();

        let transceiver = glib::Object::new::<Transceiver>();
        transceiver
            .imp()
            .state()
            .set_codec_preferences(Some(l16_caps()));
        session.imp().state().add_transceiver(transceiver);

        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let session_weak = session.downgrade();
        session.imp().create_offer(
            None,
            Some(gst::Promise::with_change_func(move |reply| {
                let reply = reply.unwrap().unwrap();
                let sdp = reply.get::<String>("sdp").unwrap();
                tx.send(sdp.clone()).unwrap();
                let session = session_weak.upgrade().unwrap();
                session.imp().set_remote_description(
                    "offer".to_string(),
                    sdp,
                    Some(gst::Promise::with_change_func(move |_reply| {
                        let session = session_weak.upgrade().unwrap();
                        let _sdp = session.property::<String>("pending-remote-description");
                    })),
                );
                session.imp().create_answer(
                    None,
                    Some(gst::Promise::with_change_func(move |reply| {
                        let reply = reply.unwrap().unwrap();
                        let sdp = reply.get::<String>("sdp").unwrap();
                        tx.send(sdp.clone()).unwrap();
                    })),
                );
            })),
        );
        let offer = rx.recv().unwrap();
        let sdp = parse_webrtc_sdp(&offer).unwrap();
        let media0 = expected_rtp_media(MediaType::Audio, 11, "L16", 44_100);
        let expected_offer = ExpectedSdp {
            media: vec![ExpectedMedia {
                media: MediaType::Audio,
                ice_ufrag: Some(true),
                ice_pwd: Some(true),
                setup: Some(Setup::ActPass),
                mid: String::from("0"),
                bundle_only: false,
                fingerprints: 1,
                rtp: Some(media0),
            }],
        };
        expected_offer.match_against(&sdp);
        let answer = rx.recv().unwrap();
        let mut expected_answer = expected_offer.clone();
        expected_answer.media[0].setup = Some(Setup::Active);
        let sdp = parse_webrtc_sdp(&answer).unwrap();
        expected_answer.match_against(&sdp);
    }

    fn klv_caps() -> gst::Caps {
        gst::Caps::builder("application/x-rtp")
            .field("media", "application")
            .field("payload", 98i32)
            .field("clock-rate", 90000i32)
            .field("encoding-name", "SMPTE336M")
            .build()
    }

    #[test]
    fn session_klv() {
        init();

        let session = glib::Object::new::<super::super::WebRTCSession>();
        session.imp().ensure_operations().unwrap();

        let transceiver = glib::Object::new::<Transceiver>();
        transceiver
            .imp()
            .state()
            .set_codec_preferences(Some(klv_caps()));
        session.imp().state().add_transceiver(transceiver);

        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let session_weak = session.downgrade();
        session.imp().create_offer(
            None,
            Some(gst::Promise::with_change_func(move |reply| {
                let reply = reply.unwrap().unwrap();
                let sdp = reply.get::<String>("sdp").unwrap();
                tx.send(sdp.clone()).unwrap();
                let session = session_weak.upgrade().unwrap();
                session.imp().set_remote_description(
                    "offer".to_string(),
                    sdp,
                    Some(gst::Promise::with_change_func(move |_reply| {
                        let session = session_weak.upgrade().unwrap();
                        let _sdp = session.property::<String>("pending-remote-description");
                    })),
                );
                session.imp().create_answer(
                    None,
                    Some(gst::Promise::with_change_func(move |reply| {
                        let reply = reply.unwrap().unwrap();
                        let sdp = reply.get::<String>("sdp").unwrap();
                        tx.send(sdp.clone()).unwrap();
                    })),
                );
            })),
        );
        let offer = rx.recv().unwrap();
        let sdp = parse_webrtc_sdp(&offer).unwrap();
        let media0 = expected_rtp_media(MediaType::Application, 98, "SMPTE336M", 90_000);
        let expected_offer = ExpectedSdp {
            media: vec![ExpectedMedia {
                media: MediaType::Application,
                ice_ufrag: Some(true),
                ice_pwd: Some(true),
                setup: Some(Setup::ActPass),
                mid: String::from("0"),
                bundle_only: false,
                fingerprints: 1,
                rtp: Some(media0),
            }],
        };
        expected_offer.match_against(&sdp);
        let answer = rx.recv().unwrap();
        let mut expected_answer = expected_offer.clone();
        expected_answer.media[0].setup = Some(Setup::Active);
        let sdp = parse_webrtc_sdp(&answer).unwrap();
        expected_answer.match_against(&sdp);
    }

    fn vp8_caps() -> gst::Caps {
        gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("payload", 96i32)
            .field("clock-rate", 90000i32)
            .field("encoding-name", "VP8")
            .field("channels", 1)
            .build()
    }

    #[test]
    fn session_audio_video() {
        init();

        let session = glib::Object::new::<super::super::WebRTCSession>();
        session.imp().ensure_operations().unwrap();

        let audio_transceiver = glib::Object::new::<Transceiver>();
        audio_transceiver
            .imp()
            .state()
            .set_codec_preferences(Some(l16_caps()));
        session.imp().state().add_transceiver(audio_transceiver);

        let video_transceiver = glib::Object::new::<Transceiver>();
        video_transceiver
            .imp()
            .state()
            .set_codec_preferences(Some(vp8_caps()));
        session.imp().state().add_transceiver(video_transceiver);

        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let session_weak = session.downgrade();
        session.imp().create_offer(
            None,
            Some(gst::Promise::with_change_func(move |reply| {
                let reply = reply.unwrap().unwrap();
                let sdp = reply.get::<String>("sdp").unwrap();
                tx.send(sdp.clone()).unwrap();
                let session = session_weak.upgrade().unwrap();
                session.imp().set_remote_description(
                    "offer".to_string(),
                    sdp,
                    Some(gst::Promise::with_change_func(move |_reply| {
                        let session = session_weak.upgrade().unwrap();
                        let _sdp = session.property::<String>("pending-remote-description");
                    })),
                );
                session.imp().create_answer(
                    None,
                    Some(gst::Promise::with_change_func(move |reply| {
                        let reply = reply.unwrap().unwrap();
                        let sdp = reply.get::<String>("sdp").unwrap();
                        tx.send(sdp.clone()).unwrap();
                    })),
                );
            })),
        );
        let offer = rx.recv().unwrap();
        let sdp = parse_webrtc_sdp(&offer).unwrap();
        let media0 = expected_rtp_media(MediaType::Audio, 11, "L16", 44_100);
        let mut media1 = expected_rtp_media(MediaType::Video, 96, "VP8", 90_000);
        media1.set_rtcp_mux(false);
        media1.set_rtcp_mux_only(false);
        media1.set_rtcp_rsize(false);
        let expected_offer = ExpectedSdp {
            media: vec![
                ExpectedMedia {
                    media: MediaType::Audio,
                    ice_ufrag: Some(true),
                    ice_pwd: Some(true),
                    setup: Some(Setup::ActPass),
                    mid: String::from("0"),
                    bundle_only: false,
                    fingerprints: 1,
                    rtp: Some(media0),
                },
                ExpectedMedia {
                    media: MediaType::Video,
                    ice_ufrag: Some(false),
                    ice_pwd: Some(false),
                    setup: None,
                    mid: String::from("1"),
                    bundle_only: true,
                    fingerprints: 1,
                    rtp: Some(media1),
                },
            ],
        };
        expected_offer.match_against(&sdp);
        let answer = rx.recv().unwrap();
        let mut expected_answer = expected_offer.clone();
        expected_answer.media[0].setup = Some(Setup::Active);
        let sdp = parse_webrtc_sdp(&answer).unwrap();
        expected_answer.match_against(&sdp);
    }
}
