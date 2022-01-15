// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;

mod imp;

glib::wrapper! {
    pub struct CustomSource(ObjectSubclass<imp::CustomSource>) @extends gst::Bin, gst::Element, gst::Object;
}

// GStreamer elements need to be thread-safe. For the private implementation this is automatically
// enforced but for the public wrapper type we need to specify this manually.
unsafe impl Send for CustomSource {}
unsafe impl Sync for CustomSource {}

impl CustomSource {
    pub fn new(source: &gst::Element) -> CustomSource {
        glib::Object::new(&[("source", source)]).unwrap()
    }
}
