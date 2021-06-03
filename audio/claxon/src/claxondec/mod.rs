// Copyright (C) 2019 Ruben Gonzalez <rgonzalez@fluendo.com>
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
    pub struct ClaxonDec(ObjectSubclass<imp::ClaxonDec>) @extends gst_audio::AudioDecoder, gst::Element, gst::Object;
}

unsafe impl Send for ClaxonDec {}
unsafe impl Sync for ClaxonDec {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "claxondec",
        gst::Rank::Marginal,
        ClaxonDec::static_type(),
    )
}
