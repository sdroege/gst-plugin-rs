// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
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
    pub struct LewtonDec(ObjectSubclass<imp::LewtonDec>) @extends gst_audio::AudioDecoder, gst::Element, gst::Object;
}

unsafe impl Send for LewtonDec {}
unsafe impl Sync for LewtonDec {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "lewtondec",
        gst::Rank::Marginal,
        LewtonDec::static_type(),
    )
}
