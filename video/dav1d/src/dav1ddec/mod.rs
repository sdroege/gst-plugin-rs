// Copyright (C) 2019 Philippe Normand <philn@igalia.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct Dav1dDec(ObjectSubclass<imp::Dav1dDec>) @extends gst_video::VideoDecoder, gst::Element, gst::Object;
}

// GStreamer elements need to be thread-safe. For the private implementation this is automatically
// enforced but for the public wrapper type we need to specify this manually.
unsafe impl Send for Dav1dDec {}
unsafe impl Sync for Dav1dDec {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rsdav1ddec",
        gst::Rank::Primary + 1,
        Dav1dDec::static_type(),
    )
}
