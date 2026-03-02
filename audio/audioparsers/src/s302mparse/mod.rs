// Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/> .
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*};

mod imp;

glib::wrapper! {
    pub struct S302MParse(ObjectSubclass<imp::S302MParse>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "s302mparse",
        gst::Rank::PRIMARY + 1,
        S302MParse::static_type(),
    )
}
