// GStreamer RTP Opus Depayloader
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

pub mod imp;

glib::wrapper! {
    pub struct RtpOpusDepay(ObjectSubclass<imp::RtpOpusDepay>)
        @extends crate::basedepay::RtpBaseDepay2, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rtpopusdepay2",
        // +1 because unlike all other (de)payloaders, Opus was using
        // PRIMARY instead of SECONDARY. See:
        // https://gitlab.freedesktop.org/gstreamer/gstreamer/-/merge_requests/10940
        gst::Rank::PRIMARY + 1,
        RtpOpusDepay::static_type(),
    )
}
