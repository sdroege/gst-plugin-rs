// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::MutexGuard;

use gst::glib;
use gst::glib::WeakRef;
use gst::prelude::*;
use gst::subclass::prelude::*;

use crate::webrtcsend::WebRTCSendSinkPad;
use crate::webrtcsession::sdp::Direction;
use crate::webrtcsession::sdp::MediaSpecifics;
use crate::webrtcsession::sdp::MediaType;
use crate::webrtcsession::sdp::RtpExtension;
use crate::webrtcsession::sdp::RtpMap;
use crate::webrtcsession::sdp::WebRTCSdp;
use crate::webrtcsession::sdp::WebRTCSdpMedia;

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
    pub fn update_from_sdp(&mut self, local_sdp: &WebRTCSdp, idx: usize, remote_sdp: &WebRTCSdp) {
        self.mline = Some(idx);
        self.mid = local_sdp.media[idx].mid.clone();
        self.pending_mid.take();

        let local_rtp = local_sdp.media[idx].specifics.rtp().unwrap();
        let remote_rtp = remote_sdp.media[idx].specifics.rtp().unwrap();

        let new_dir = local_rtp
            .direction
            .intersect_with_remote(remote_rtp.direction);

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

    pub fn generate_offer_media(&self, idx: u32) -> Option<WebRTCSdpMedia> {
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
                        rtp.mid = state.mid.clone().or(Some(
                            state
                                .pending_mid
                                .get_or_insert_with(|| idx.to_string())
                                .clone(),
                        ));
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
                        rtp.mid = state.mid.clone().or(Some(
                            state
                                .pending_mid
                                .get_or_insert_with(|| idx.to_string())
                                .clone(),
                        ));
                        rtp
                    })
                }),
        }
    }
}

static IGNORED_RTP_CAPS_FIELDS: [&str; 7] = [
    "media",
    "timestamp-offset",
    "seqnum-offset",
    "encoding-name",
    "encoding-params",
    "ssrc",
    "clock-rate",
];

pub(crate) fn rtp_caps_to_media(caps: &gst::Caps) -> Option<WebRTCSdpMedia> {
    gst::trace!(CAT, "converting caps to media: {caps}");
    let s = caps.structure(0)?;
    if !s.has_name("application/x-rtp") {
        return None;
    }
    let media_type = s.get::<String>("media").ok()?.parse::<MediaType>().ok()?;
    let mut media = WebRTCSdpMedia::new_rtp(media_type);

    for s in 0..caps.len() {
        let s = caps.structure(s)?;
        let media_type = s.get::<String>("media").ok()?.parse::<MediaType>().ok()?;
        let payload = s.get::<i32>("payload").ok()?;
        if media_type != media.media {
            return None;
        }
        let MediaSpecifics::Rtp(rtp) = &mut media.specifics else {
            unimplemented!();
        };
        if !(0..=127).contains(&payload) || rtp.formats.contains(&(payload as u8)) {
            return None;
        }
        let payload = payload as u8;

        let encoding_name = s.get::<String>("encoding-name").ok();
        let encoding_params = s.get::<String>("encoding-params").ok();
        //let ssrc = s.get::<u32>("ssrc").ok();
        let clock_rate = s.get::<i32>("clock-rate").ok();
        let mut fmtp = String::new();
        for field in s.fields() {
            if IGNORED_RTP_CAPS_FIELDS.contains(&field.as_str())
                || field.starts_with("srtp-")
                || field.starts_with("srtcp-")
                || field.starts_with("rtcp-fb-")
                || field.starts_with("ssrc-")
                || field.starts_with("x-gst-rtsp-server-rtx-time")
            {
                continue;
            } else if field.starts_with("extmap-") {
                let id = field[6..].parse::<u8>().ok()?;
                if [0, 15].contains(&id) {
                    continue;
                }
                let val = s.value(field).unwrap();
                if val.is_type(glib::Type::STRING) {
                    rtp.extmap.insert(RtpExtension {
                        id,
                        direction: Direction::SendRecv,
                        name: val.get::<String>().ok()?,
                        params: None,
                    });
                } else if val.is_type(gst::Array::static_type()) {
                    let arr = val.get::<gst::Array>().ok()?;
                    if arr.len() != 3 {
                        return None;
                    }
                    let direction = arr[0].get::<&str>().ok()?;
                    let direction = if direction.is_empty() {
                        Direction::SendRecv
                    } else {
                        direction.parse::<Direction>().ok()?
                    };
                    let name = arr[1].get::<String>().ok()?;
                    let params = arr[2].get::<String>().ok()?;
                    let params = if params.is_empty() {
                        None
                    } else {
                        Some(params)
                    };
                    rtp.extmap.insert(RtpExtension {
                        id,
                        direction,
                        name,
                        params,
                    });
                }
            } else if field.starts_with("a-") || field.starts_with("x-") {
                // TODO handle
                continue;
            } else if s.has_field_with_type(field, glib::Type::STRING) {
                let val = s.get::<&str>(field).ok()?;
                if !fmtp.is_empty() {
                    fmtp.push(';');
                }
                fmtp.push_str(field);
                fmtp.push('=');
                fmtp.push_str(val);
            }
            // TODO: rid
        }
        rtp.formats.push(payload);
        if !fmtp.is_empty() {
            rtp.fmtps.insert(payload, fmtp);
        }
        if let Some(encoding_name) = encoding_name {
            rtp.rtpmaps.insert(
                payload,
                RtpMap {
                    name: encoding_name,
                    clock_rate: clock_rate? as u32,
                    params: encoding_params,
                },
            );
        }
    }
    gst::trace!(CAT, "produced media {media:?}");
    Some(media)
}
