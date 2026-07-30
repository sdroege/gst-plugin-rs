// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
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
    pub struct UdpSink(ObjectSubclass<imp::UdpSink>)
        @extends crate::baseudpsink::BaseUdpSink, gst_base::BaseSink, gst::Element, gst::Object,
        @implements gst::URIHandler;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "udpsink2",
        gst::Rank::NONE,
        UdpSink::static_type(),
    )
}
