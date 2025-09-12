// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-webrtcsend
 *
 * Since: plugins-rs-0.16
 */
use gst::glib;
use gst::prelude::*;

mod imp;
pub mod pad;

glib::wrapper! {
    pub struct WebRTCSend(ObjectSubclass<imp::WebRTCSend>) @extends gst::Bin, gst::Element, gst::Object;
}
glib::wrapper! {
    pub struct WebRTCSendSinkPad(ObjectSubclass<pad::WebRTCSendSinkPad>) @extends gst::GhostPad, gst::ProxyPad, gst::Pad, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "webrtcsend",
        gst::Rank::NONE,
        WebRTCSend::static_type(),
    )?;

    Ok(())
}
