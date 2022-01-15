// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
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
