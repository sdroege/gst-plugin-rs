// SPDX-License-Identifier: MPL-2.0
#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

/**
 * plugin-rswebrtcbin2:
 * @title: Rust WebRTC elements
 * @short_description: A collection of low level WebRTC elements
 *
 * {{ net/webrtcbin2/README.md[2:233] }}
 *
 * Since: plugins-rs-0.16
 */
use gst::glib;
use std::sync::LazyLock;

pub mod transceiver;
mod utils;
mod webrtcrecv;
mod webrtcsend;
pub mod webrtcsession;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        use gst::{glib::types::StaticType, prelude::*};

        crate::webrtcrecv::WebRTCRecvSrcPad::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
        crate::webrtcsend::WebRTCSendSinkPad::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
        crate::webrtcsession::WebRTCSession::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
        crate::webrtcsession::SignalingStatus::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
        crate::transceiver::Transceiver::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }
    webrtcsend::register(plugin)?;
    webrtcrecv::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    rswebrtcbin2,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);

pub static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .enable_io()
        .worker_threads(1)
        .build()
        .unwrap()
});
