// SPDX-License-Identifier: MPL-2.0

use crate::signaller::Signallable;
/**
 * element-webrtcsink:
 *
 * {{ net/webrtc/README.md[0:190] }}
 *
 */
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

mod homegrown_cc;

mod imp;

glib::wrapper! {
    pub struct BaseWebRTCSink(ObjectSubclass<imp::BaseWebRTCSink>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy, gst_video::Navigation;
}

glib::wrapper! {
    pub struct WebRTCSink(ObjectSubclass<imp::WebRTCSink>) @extends BaseWebRTCSink, gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy, gst_video::Navigation;
}

glib::wrapper! {
    pub struct AwsKvsWebRTCSink(ObjectSubclass<imp::AwsKvsWebRTCSink>) @extends BaseWebRTCSink, gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy, gst_video::Navigation;
}

#[derive(thiserror::Error, Debug)]
pub enum WebRTCSinkError {
    #[error("no session with id")]
    NoSessionWithId(String),
    #[error("consumer refused media")]
    ConsumerRefusedMedia { session_id: String, media_idx: u32 },
    #[error("consumer did not provide valid payload for media")]
    ConsumerNoValidPayload { session_id: String, media_idx: u32 },
    #[error("SDP mline index is currently mandatory")]
    MandatorySdpMlineIndex,
    #[error("duplicate session id")]
    DuplicateSessionId(String),
    #[error("error setting up consumer pipeline")]
    SessionPipelineError {
        session_id: String,
        peer_id: String,
        details: String,
    },
}

impl Default for BaseWebRTCSink {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl BaseWebRTCSink {
    pub fn with_signaller(signaller: Signallable) -> Self {
        let ret: BaseWebRTCSink = glib::Object::new();

        let ws = ret.imp();
        ws.set_signaller(signaller).unwrap();

        ret
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstWebRTCSinkCongestionControl")]
pub enum WebRTCSinkCongestionControl {
    #[enum_value(name = "Disabled: no congestion control is applied", nick = "disabled")]
    Disabled,
    #[enum_value(name = "Homegrown: simple sender-side heuristic", nick = "homegrown")]
    Homegrown,
    #[enum_value(name = "Google Congestion Control algorithm", nick = "gcc")]
    GoogleCongestionControl,
}

#[glib::flags(name = "GstWebRTCSinkMitigationMode")]
enum WebRTCSinkMitigationMode {
    #[flags_value(name = "No mitigation applied", nick = "none")]
    NONE = 0b00000000,
    #[flags_value(name = "Lowered resolution", nick = "downscaled")]
    DOWNSCALED = 0b00000001,
    #[flags_value(name = "Lowered framerate", nick = "downsampled")]
    DOWNSAMPLED = 0b00000010,
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    BaseWebRTCSink::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    WebRTCSinkCongestionControl::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    gst::Element::register(
        Some(plugin),
        "webrtcsink",
        gst::Rank::None,
        WebRTCSink::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "awskvswebrtcsink",
        gst::Rank::None,
        AwsKvsWebRTCSink::static_type(),
    )?;

    Ok(())
}
