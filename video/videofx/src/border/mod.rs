// Copyright (C) 2021 Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

pub mod roundedcorners;

glib::wrapper! {
    pub struct RoundedCorners(ObjectSubclass<roundedcorners::RoundedCorners>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

unsafe impl Send for RoundedCorners {}
unsafe impl Sync for RoundedCorners {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "roundedcorners",
        gst::Rank::None,
        RoundedCorners::static_type(),
    )
}
