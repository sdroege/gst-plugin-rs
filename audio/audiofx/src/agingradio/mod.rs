// Copyright (C) 2026 Vivia Nikolaidou <vivia@ahiru.eu>
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
    pub struct AgingRadio(ObjectSubclass<imp::AgingRadio>) @extends gst_audio::AudioFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "agingradio",
        gst::Rank::NONE,
        AgingRadio::static_type(),
    )
}
