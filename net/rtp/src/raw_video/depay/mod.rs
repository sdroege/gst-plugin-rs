// GStreamer RTP Raw Video Depayloader
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

#[derive(Copy, Clone, Debug, PartialEq, Eq, glib::Enum, Default)]
#[enum_type(name = "GstRtpRawVideoDepay2ConcealmentMethod")]
#[repr(i32)]
pub enum ConcealmentMethod {
    #[enum_value(name = "Black", nick = "black")]
    Black,
    #[default]
    #[enum_value(name = "Last Frame", nick = "last-frame")]
    LastFrame,
}

glib::wrapper! {
    pub struct RtpRawVideoDepay(ObjectSubclass<imp::RtpRawVideoDepay>)
        @extends crate::basedepay::RtpBaseDepay2, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        ConcealmentMethod::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }

    gst::Element::register(
        Some(plugin),
        "rtpvrawdepay2",
        gst::Rank::MARGINAL,
        RtpRawVideoDepay::static_type(),
    )
}
