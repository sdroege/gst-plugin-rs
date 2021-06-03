// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//               2016 Luis de Bethencourt <luisbg@osg.samsung.com>
//               2018 François Laignel <fengalin@free.fr>
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
    pub struct FileSink(ObjectSubclass<imp::FileSink>) @extends gst_base::BaseSink, gst::Element, gst::Object, @implements gst::URIHandler;
}

unsafe impl Send for FileSink {}
unsafe impl Sync for FileSink {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rsfilesink",
        gst::Rank::None,
        FileSink::static_type(),
    )
}
