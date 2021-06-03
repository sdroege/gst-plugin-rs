// Copyright (C) 2016-2018 Sebastian Dröge <sebastian@centricular.com>
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
    pub struct ReqwestHttpSrc(ObjectSubclass<imp::ReqwestHttpSrc>) @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object, @implements gst::URIHandler;
}

unsafe impl Send for ReqwestHttpSrc {}
unsafe impl Sync for ReqwestHttpSrc {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "reqwesthttpsrc",
        gst::Rank::Marginal,
        ReqwestHttpSrc::static_type(),
    )
}
