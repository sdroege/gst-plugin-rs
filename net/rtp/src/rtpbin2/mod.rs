// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use once_cell::sync::Lazy;
mod imp;
mod jitterbuffer;
mod session;
mod source;
mod time;

glib::wrapper! {
    pub struct RtpBin2(ObjectSubclass<imp::RtpBin2>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rtpbin2",
        gst::Rank::NONE,
        RtpBin2::static_type(),
    )
}

pub static RUNTIME: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .worker_threads(1)
        .build()
        .unwrap()
});
