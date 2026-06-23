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

#[derive(Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstWebRTCSendEarlyDataMode")]
/// WebRTCSend behaviour while connection is pending.
pub enum WebRTCSendEarlyDataMode {
    #[default]
    #[enum_value(name = "Block buffers", nick = "block")]
    Block,
    #[enum_value(name = "Drop buffers", nick = "drop")]
    Drop,
}

impl WebRTCSendEarlyDataMode {
    pub fn is_block(self) -> bool {
        matches!(self, WebRTCSendEarlyDataMode::Block)
    }

    pub fn is_drop(self) -> bool {
        matches!(self, WebRTCSendEarlyDataMode::Drop)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    WebRTCSendEarlyDataMode::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    gst::Element::register(
        Some(plugin),
        "webrtcsend",
        gst::Rank::NONE,
        WebRTCSend::static_type(),
    )?;

    Ok(())
}
