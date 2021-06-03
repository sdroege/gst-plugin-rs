// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
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
    pub struct EbuR128Level(ObjectSubclass<imp::EbuR128Level>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

unsafe impl Send for EbuR128Level {}
unsafe impl Sync for EbuR128Level {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ebur128level",
        gst::Rank::None,
        EbuR128Level::static_type(),
    )
}
