// SPDX-License-Identifier: LGPL-2.1-or-later

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct BlockingAdapter(ObjectSubclass<imp::BlockingAdapter>)
        @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-blocking-adapter",
        gst::Rank::NONE,
        BlockingAdapter::static_type(),
    )
}
