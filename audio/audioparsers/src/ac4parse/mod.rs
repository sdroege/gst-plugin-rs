// Copyright (C) 2026 Fluendo S.A.
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/> .
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*};

mod imp;

glib::wrapper! {
    pub struct Ac4Parse(ObjectSubclass<imp::Ac4Parse>) @extends gst_base::BaseParse, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ac4parse",
        gst::Rank::PRIMARY,
        Ac4Parse::static_type(),
    )
}
