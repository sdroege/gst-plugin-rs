// SPDX-License-Identifier: MPL-2.0

use crate::webrtcsink::{Signallable, WebRTCSink};
use gst::glib;
use gst::subclass::prelude::*;
use std::error::Error;

mod imp;
mod protocol;

glib::wrapper! {
    pub struct AwsKvsSignaller(ObjectSubclass<imp::Signaller>);
}

unsafe impl Send for AwsKvsSignaller {}
unsafe impl Sync for AwsKvsSignaller {}

impl Signallable for AwsKvsSignaller {
    fn start(&mut self, element: &WebRTCSink) -> Result<(), Box<dyn Error>> {
        let signaller = self.imp();
        signaller.start(element);

        Ok(())
    }

    fn handle_sdp(
        &mut self,
        element: &WebRTCSink,
        peer_id: &str,
        sdp: &gst_webrtc::WebRTCSessionDescription,
    ) -> Result<(), Box<dyn Error>> {
        let signaller = self.imp();
        signaller.handle_sdp(element, peer_id, sdp);
        Ok(())
    }

    fn handle_ice(
        &mut self,
        element: &WebRTCSink,
        session_id: &str,
        candidate: &str,
        sdp_mline_index: Option<u32>,
        sdp_mid: Option<String>,
    ) -> Result<(), Box<dyn Error>> {
        let signaller = self.imp();
        signaller.handle_ice(element, session_id, candidate, sdp_mline_index, sdp_mid);
        Ok(())
    }

    fn stop(&mut self, element: &WebRTCSink) {
        let signaller = self.imp();
        signaller.stop(element);
    }

    fn session_ended(&mut self, element: &WebRTCSink, session_id: &str) {
        let signaller = self.imp();
        signaller.end_session(element, session_id);
    }
}

impl Default for AwsKvsSignaller {
    fn default() -> Self {
        glib::Object::new()
    }
}
