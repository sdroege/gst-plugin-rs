// Copyright (C) 2021 Jan Schmidt <jan@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::prelude::*;

mod fku;
mod imp;

glib::wrapper! {
    pub struct JsonToVtt(ObjectSubclass<imp::JsonToVtt>) @extends gst::Element, gst::Object;
}

// GStreamer elements need to be thread-safe. For the private implementation this is automatically
// enforced but for the public wrapper type we need to specify this manually.
unsafe impl Send for JsonToVtt {}
unsafe impl Sync for JsonToVtt {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "jsontovtt",
        gst::Rank::None,
        JsonToVtt::static_type(),
    )
}
