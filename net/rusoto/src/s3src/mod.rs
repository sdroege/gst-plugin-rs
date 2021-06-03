// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
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
    pub struct S3Src(ObjectSubclass<imp::S3Src>) @extends gst_base::BaseSrc, gst::Element, gst::Object;
}

unsafe impl Send for S3Src {}
unsafe impl Sync for S3Src {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rusotos3src",
        gst::Rank::Primary,
        S3Src::static_type(),
    )
}
