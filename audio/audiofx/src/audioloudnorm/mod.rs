// Copyright (C) 2019-2020 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct AudioLoudNorm(ObjectSubclass<imp::AudioLoudNorm>) @extends gst::Element, gst::Object;
}

unsafe impl Send for AudioLoudNorm {}
unsafe impl Sync for AudioLoudNorm {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rsaudioloudnorm",
        gst::Rank::None,
        AudioLoudNorm::static_type(),
    )
}
