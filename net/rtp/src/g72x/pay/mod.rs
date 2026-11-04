//
// Copyright (C) 2026 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*};

pub mod g722;
pub mod g723;
pub mod g726;
pub mod g729;

glib::wrapper! {
    pub struct RtpG722Pay(ObjectSubclass<g722::RtpG722Pay>)
        @extends crate::baseaudiopay::RtpBaseAudioPay2, crate::basepay::RtpBasePay2, gst::Element, gst::Object;
}

glib::wrapper! {
    pub struct RtpG723Pay(ObjectSubclass<g723::RtpG723Pay>)
        @extends crate::basepay::RtpBasePay2, gst::Element, gst::Object;
}

glib::wrapper! {
    pub struct RtpG726Pay(ObjectSubclass<g726::RtpG726Pay>)
        @extends crate::baseaudiopay::RtpBaseAudioPay2, crate::basepay::RtpBasePay2, gst::Element, gst::Object;
}

glib::wrapper! {
    pub struct RtpG729Pay(ObjectSubclass<g729::RtpG729Pay>)
        @extends crate::basepay::RtpBasePay2, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rtpg722pay2",
        gst::Rank::PRIMARY,
        RtpG722Pay::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "rtpg723pay2",
        gst::Rank::PRIMARY,
        RtpG723Pay::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "rtpg726pay2",
        gst::Rank::PRIMARY,
        RtpG726Pay::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "rtpg729pay2",
        gst::Rank::PRIMARY,
        RtpG729Pay::static_type(),
    )
}
