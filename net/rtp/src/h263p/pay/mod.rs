// GStreamer RTP H.263+ Payloader
//
// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
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
    pub struct RtpH263PPay(ObjectSubclass<imp::RtpH263PPay>)
        @extends crate::basepay::RtpBasePay2, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        FragmentationMode::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }

    gst::Element::register(
        Some(plugin),
        "rtph263ppay2",
        gst::Rank::PRIMARY,
        RtpH263PPay::static_type(),
    )
}

/// How to fragment frames that are larger than the MTU.
#[derive(Copy, Clone, Debug, PartialEq, Eq, glib::Enum, Default)]
#[enum_type(name = "GstRtpH263PPay2FragmentationMode")]
#[repr(i32)]
pub enum FragmentationMode {
    /// Fragment with follow-on packets (RFC 4629 section 6.2). Packets after
    /// the first one of a frame carry no start code and can only be decoded
    /// together with the preceding packets of the same frame.
    #[default]
    #[enum_value(name = "Follow-on packets", nick = "normal")]
    Normal,
    /// Fragment at segment boundaries (picture, GOB, slice, EOS, EOSBS start
    /// codes, RFC 4629 section 6.1). Each packet starts at a resynchronization
    /// point, letting a receiver resume decoding at the next packet after a
    /// loss; a segment larger than the MTU is still continued with follow-on
    /// packets.
    #[enum_value(name = "Fragment at segment boundaries", nick = "sync")]
    Sync,
}
