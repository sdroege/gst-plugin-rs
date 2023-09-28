use gst::glib;
use gst::subclass::prelude::*;
use gst_webrtc::WebRTCSessionDescription;

use gstrswebrtc::signaller::{Signallable, SignallableImpl};

#[derive(Default)]
pub struct Signaller {}

impl Signaller {}

impl SignallableImpl for Signaller {
    fn start(&self) {
        unimplemented!()
    }

    fn stop(&self) {
        unimplemented!()
    }

    fn send_sdp(&self, _session_id: &str, _sdp: &WebRTCSessionDescription) {
        unimplemented!()
    }

    fn add_ice(
        &self,
        _session_id: &str,
        _candidate: &str,
        _sdp_m_line_index: u32,
        _sdp_mid: Option<String>,
    ) {
        unimplemented!()
    }

    fn end_session(&self, _session_id: &str) {
        unimplemented!()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Signaller {
    const NAME: &'static str = "MyCustomWebRTCSinkSignaller";
    type Type = super::MyCustomSignaller;
    type ParentType = glib::Object;
    type Interfaces = (Signallable,);
}

impl ObjectImpl for Signaller {}
