// GStreamer RTP Raw Video Payloader
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#![allow(clippy::new_without_default)]

use gst::glib;
use gst::prelude::*;

mod packing_template;

pub mod imp;

glib::wrapper! {
    pub struct RtpRawVideoPay(ObjectSubclass<imp::RtpRawVideoPay>)
        @extends crate::basepay::RtpBasePay2, gst::Element, gst::Object;
}

impl RtpRawVideoPay {
    pub fn new() -> Self {
        glib::Object::new()
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rtpvrawpay2",
        gst::Rank::MARGINAL,
        RtpRawVideoPay::static_type(),
    )
}
