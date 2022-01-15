// Copyright (C) 2021 Jan Schmidt <jan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

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
