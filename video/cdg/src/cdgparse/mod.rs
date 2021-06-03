// Copyright (C) 2019 Guillaume Desmottes <guillaume.desmottes@collabora.com>
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
    pub struct CdgParse(ObjectSubclass<imp::CdgParse>) @extends gst_base::BaseParse, gst::Element, gst::Object;
}

// GStreamer elements need to be thread-safe. For the private implementation this is automatically
// enforced but for the public wrapper type we need to specify this manually.
unsafe impl Send for CdgParse {}
unsafe impl Sync for CdgParse {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "cdgparse",
        gst::Rank::Primary,
        CdgParse::static_type(),
    )
}
