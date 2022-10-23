// SPDX-License-Identifier: MPL-2.0

/**
 * element-webrtcsink:
 *
 * {{ net/webrtc/README.md[0:190] }}
 *
 */
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::error::Error;

mod homegrown_cc;
mod imp;

glib::wrapper! {
    pub struct WebRTCSink(ObjectSubclass<imp::WebRTCSink>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy, gst_video::Navigation;
}

unsafe impl Send for WebRTCSink {}
unsafe impl Sync for WebRTCSink {}

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

pub trait Signallable: Sync + Send + 'static {
    fn start(&mut self, element: &WebRTCSink) -> Result<(), Box<dyn Error>>;

    fn handle_sdp(
        &mut self,
        element: &WebRTCSink,
        session_id: &str,
        sdp: &gst_webrtc::WebRTCSessionDescription,
    ) -> Result<(), Box<dyn Error>>;

    /// sdp_mid is exposed for future proofing, see
    /// https://gitlab.freedesktop.org/gstreamer/gst-plugins-bad/-/issues/1174,
    /// at the moment sdp_m_line_index will always be Some and sdp_mid will always
    /// be None
    fn handle_ice(
        &mut self,
        element: &WebRTCSink,
        session_id: &str,
        candidate: &str,
        sdp_m_line_index: Option<u32>,
        sdp_mid: Option<String>,
    ) -> Result<(), Box<dyn Error>>;

    fn session_ended(&mut self, element: &WebRTCSink, session_id: &str);

    fn stop(&mut self, element: &WebRTCSink);
}

/// When providing a signaller, we expect it to both be a GObject
/// and be Signallable. This is arguably a bit strange, but exposing
/// a GInterface from rust is at the moment a bit awkward, so I went
/// for a rust interface for now. The reason the signaller needs to be
/// a GObject is to make its properties available through the GstChildProxy
/// interface.
pub trait SignallableObject: AsRef<glib::Object> + Signallable {}

impl<T: AsRef<glib::Object> + Signallable> SignallableObject for T {}

impl Default for WebRTCSink {
    fn default() -> Self {
        glib::Object::new::<Self>(&[])
    }
}

impl WebRTCSink {
    pub fn with_signaller(signaller: Box<dyn SignallableObject>) -> Self {
        let ret: WebRTCSink = glib::Object::new(&[]);

        let ws = ret.imp();
        ws.set_signaller(signaller).unwrap();

        ret
    }

    pub fn handle_sdp(
        &self,
        session_id: &str,
        sdp: &gst_webrtc::WebRTCSessionDescription,
    ) -> Result<(), WebRTCSinkError> {
        let ws = self.imp();
        ws.handle_sdp(self, session_id, sdp)
    }

    /// sdp_mid is exposed for future proofing, see
    /// https://gitlab.freedesktop.org/gstreamer/gst-plugins-bad/-/issues/1174,
    /// at the moment sdp_m_line_index must be Some
    pub fn handle_ice(
        &self,
        session_id: &str,
        sdp_m_line_index: Option<u32>,
        sdp_mid: Option<String>,
        candidate: &str,
    ) -> Result<(), WebRTCSinkError> {
        let ws = self.imp();
        ws.handle_ice(self, session_id, sdp_m_line_index, sdp_mid, candidate)
    }

    pub fn handle_signalling_error(&self, error: Box<dyn Error + Send + Sync>) {
        let ws = self.imp();
        ws.handle_signalling_error(self, anyhow::anyhow!(error));
    }

    pub fn start_session(&self, session_id: &str, peer_id: &str) -> Result<(), WebRTCSinkError> {
        let ws = self.imp();
        ws.start_session(self, session_id, peer_id)
    }

    pub fn end_session(&self, session_id: &str) -> Result<(), WebRTCSinkError> {
        let ws = self.imp();
        ws.remove_session(self, session_id, false)
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
    WebRTCSinkCongestionControl::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    gst::Element::register(
        Some(plugin),
        "webrtcsink",
        gst::Rank::None,
        WebRTCSink::static_type(),
    )
}
