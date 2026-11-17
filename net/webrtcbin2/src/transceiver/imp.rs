// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::MutexGuard;

use gst::glib;
use gst::glib::WeakRef;
use gst::prelude::*;
use gst::subclass::prelude::*;

use sdp_types::Connection;
use sdp_types::Direction;
use sdp_types::Mid;
use sdp_types::TransportProto;

use crate::webrtcsend::WebRTCSendSinkPad;
use sdp_types_gst::{Media, Session};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "webrtc2transceiver",
        gst::DebugColorFlags::empty(),
        Some("WebRTC2Transceiver"),
    )
});

#[derive(Debug, Default)]
pub struct Transceiver {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
pub struct State {
    mline: Option<usize>,
    send_pad: Option<WeakRef<gst::Pad>>,
    recv_pad: Option<WeakRef<gst::Pad>>,
    direction: Direction,
    current_direction: Option<Direction>,
    mid: Option<String>,
    pending_mid: Option<String>,
    codec_preferences: Option<gst::Caps>,
    // TODO: simulcast?
}

impl State {
    pub fn mline(&self) -> Option<usize> {
        self.mline
    }
    pub fn direction(&self) -> Direction {
        self.direction
    }
    pub fn set_direction(&mut self, direction: Direction) {
        self.direction = direction;
    }
    pub fn current_direction(&self) -> Option<Direction> {
        self.current_direction
    }
    pub fn set_current_direction(&mut self, direction: Direction) {
        self.current_direction = Some(direction);
    }
    pub fn send_pad(&self) -> Option<gst::Pad> {
        self.send_pad.as_ref().and_then(|pad| pad.upgrade())
    }
    pub fn set_send_pad(&mut self, pad: &gst::Pad) {
        gst::trace!(CAT, "Setting send pad to {:?}", pad);
        self.send_pad = Some(pad.downgrade());
    }
    pub fn mid(&self) -> Option<&str> {
        self.mid.as_deref()
    }
    pub fn set_mid(&mut self, mid: Option<String>) {
        self.mid = mid;
    }
    pub fn set_pending_mid(&mut self, mid: String) {
        self.pending_mid = Some(mid);
    }
    pub fn pending_mid(&self) -> Option<&str> {
        self.pending_mid.as_deref()
    }
    pub fn codec_preferences(&self) -> Option<&gst::Caps> {
        self.codec_preferences.as_ref()
    }
    pub fn set_codec_preferences(&mut self, prefs: Option<gst::Caps>) {
        self.codec_preferences = prefs;
    }
    pub fn update_from_sdp(&mut self, local_sdp: &Session, idx: usize, remote_sdp: &Session) {
        self.mline = Some(idx);
        self.mid = local_sdp.media()[idx].mid().map(|mid| mid.0.clone());
        self.pending_mid.take();

        let new_dir = local_sdp.media()[idx]
            .direction()
            .intersect_with_remote(remote_sdp.media()[idx].direction());

        if self.current_direction != Some(new_dir) {
            gst::debug!(
                CAT,
                "Changing direction of transceiver from {:?} to {:?}",
                self.current_direction,
                new_dir
            );
            if new_dir == Direction::Inactive {
                // TODO: send EOS on any receive src pads
            }
        }
        self.current_direction = Some(new_dir);
    }

    pub fn send_caps(&self) -> Option<gst::Caps> {
        self.codec_preferences().cloned().or_else(|| {
            self.send_pad()
                .and_downcast_ref::<WebRTCSendSinkPad>()
                .and_then(|pad| pad.imp().state().received_caps().cloned())
        })
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Transceiver {
    const NAME: &'static str = "GstWebRTCBin2Transceiver";
    type Type = super::Transceiver;
    type ParentType = gst::Object;
}

impl ObjectImpl for Transceiver {}

impl GstObjectImpl for Transceiver {}

impl Transceiver {
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn generate_offer_media(&self, idx: u32) -> Option<Media> {
        let mut state = self.state();
        gst::trace!(
            CAT,
            imp = self,
            "Attempting to retrieve offer media with transceiver direction {:?}",
            state.direction
        );
        gst::trace!(CAT, imp = self, "state: {state:?}");
        match state.direction {
            Direction::SendRecv | Direction::SendOnly | Direction::Inactive => state
                .codec_preferences
                .clone()
                .or_else(|| {
                    state.send_pad.as_ref().and_then(|pad| {
                        pad.upgrade().and_then(|pad| {
                            gst::trace!(CAT, imp = self, "have send pad {pad:?}");
                            pad.current_caps().or_else(|| Some(pad.query_caps(None)))
                        })
                    })
                })
                .and_then(|caps| {
                    rtp_caps_to_media(&caps).map(|mut rtp| {
                        let mid = state.mid.clone().or(Some(
                            state
                                .pending_mid
                                .get_or_insert_with(|| idx.to_string())
                                .clone(),
                        ));
                        rtp.set_mid(mid.map(Mid));
                        rtp
                    })
                }),
            Direction::RecvOnly => state
                .codec_preferences
                .clone()
                .or_else(|| {
                    state.recv_pad.as_ref().and_then(|pad| {
                        pad.upgrade().and_then(|pad| {
                            gst::trace!(CAT, imp = self, "have receive pad {pad:?}");
                            pad.current_caps().or_else(|| Some(pad.query_caps(None)))
                        })
                    })
                })
                .and_then(|caps| {
                    rtp_caps_to_media(&caps).map(|mut rtp| {
                        let mid = state.mid.clone().or(Some(
                            state
                                .pending_mid
                                .get_or_insert_with(|| idx.to_string())
                                .clone(),
                        ));
                        rtp.set_mid(mid.map(Mid));
                        rtp
                    })
                }),
        }
    }
}

pub(crate) fn rtp_caps_to_media(caps: &gst::Caps) -> Option<Media> {
    gst::trace!(CAT, "converting caps to media: {caps}");
    let mut media = sdp_types_gst::caps_to_media(caps).ok()?;
    media.set_proto(TransportProto::UdpTlsRtpSavpf);
    media.set_rtcp_mux(true);
    media.set_rtcp_mux_only(true);
    media.set_connection(Some(Connection::from_ip_addr(
        std::net::Ipv4Addr::UNSPECIFIED,
    )));
    gst::trace!(CAT, "produced media {media:?}");
    Some(media)
}
