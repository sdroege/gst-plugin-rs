// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-webrtcrecv
 *
 * Since: plugins-rs-0.16
 */
use gst::glib;
use gst::prelude::*;

mod imp;
mod pad;

glib::wrapper! {
    pub struct WebRTCRecv(ObjectSubclass<imp::WebRTCRecv>) @extends gst::Bin, gst::Element, gst::Object;
}

glib::wrapper! {
    pub struct WebRTCRecvSrcPad(ObjectSubclass<pad::WebRTCRecvSrcPad>) @extends gst::Pad, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "webrtcrecv",
        gst::Rank::NONE,
        WebRTCRecv::static_type(),
    )?;

    Ok(())
}
