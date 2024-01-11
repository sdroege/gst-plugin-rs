// GStreamer RTSP Source v2
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod body;
mod imp;
mod sdp;
mod tcp_message;
mod transport;

glib::wrapper! {
    pub struct RtspSrc(ObjectSubclass<imp::RtspSrc>) @extends gst::Bin, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rtspsrc2",
        gst::Rank::NONE,
        RtspSrc::static_type(),
    )
}
