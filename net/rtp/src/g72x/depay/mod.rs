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

glib::wrapper! {
    pub struct RtpG722Depay(ObjectSubclass<g722::RtpG722Depay>)
        @extends crate::basedepay::RtpBaseDepay2, gst::Element, gst::Object;
}

glib::wrapper! {
    pub struct RtpG723Depay(ObjectSubclass<g723::RtpG723Depay>)
        @extends crate::basedepay::RtpBaseDepay2, gst::Element, gst::Object;
}

glib::wrapper! {
    pub struct RtpG726Depay(ObjectSubclass<g726::RtpG726Depay>)
        @extends crate::basedepay::RtpBaseDepay2, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rtpg722depay2",
        gst::Rank::PRIMARY,
        RtpG722Depay::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "rtpg723depay2",
        gst::Rank::PRIMARY,
        RtpG723Depay::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "rtpg726depay2",
        gst::Rank::PRIMARY,
        RtpG726Depay::static_type(),
    )
}
