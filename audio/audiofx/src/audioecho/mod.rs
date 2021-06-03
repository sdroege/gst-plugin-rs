// Copyright (C) 2017,2018 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::prelude::*;

mod imp;
mod ring_buffer;

glib::wrapper! {
    pub struct AudioEcho(ObjectSubclass<imp::AudioEcho>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

unsafe impl Send for AudioEcho {}
unsafe impl Sync for AudioEcho {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rsaudioecho",
        gst::Rank::None,
        AudioEcho::static_type(),
    )
}
