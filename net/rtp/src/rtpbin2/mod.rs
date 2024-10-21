// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use std::sync::LazyLock;
mod config;
mod internal;
mod jitterbuffer;
mod rtprecv;
mod rtpsend;
mod session;
mod source;
mod sync;
mod time;

glib::wrapper! {
    pub struct RtpSend(ObjectSubclass<rtpsend::RtpSend>) @extends gst::Element, gst::Object;
}
glib::wrapper! {
    pub struct RtpRecv(ObjectSubclass<rtprecv::RtpRecv>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        crate::rtpbin2::sync::TimestampingMode::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
        crate::rtpbin2::config::Rtp2Session::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
        crate::rtpbin2::rtpsend::Profile::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }
    gst::Element::register(
        Some(plugin),
        "rtpsend",
        gst::Rank::NONE,
        RtpSend::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "rtprecv",
        gst::Rank::NONE,
        RtpRecv::static_type(),
    )
}

pub static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .worker_threads(1)
        .build()
        .unwrap()
});
