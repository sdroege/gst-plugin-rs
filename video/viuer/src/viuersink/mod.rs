// Copyright (C) 2026 Zeeshan Ali <zeenix@gmail.com>
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
    pub struct ViuerSink(ObjectSubclass<imp::ViuerSink>)
        @extends gst_video::VideoSink, gst_base::BaseSink, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "viuersink",
        gst::Rank::NONE,
        ViuerSink::static_type(),
    )
}
