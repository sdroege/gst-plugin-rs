// Copyright (C) 2022, Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-whepsrc
 *
 * Deprecated: plugins-rs-0.16.0: This element is replaced by `whepclientsrc`, which is more robust
 * and has inbuilt decoding and RTP depayloading capabilites
 *
 */
use gst::glib;
use gst::prelude::*;

pub mod imp;

glib::wrapper! {
    pub struct WhepSrc(ObjectSubclass<imp::WhepSrc>) @extends gst::Bin, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "whepsrc",
        gst::Rank::MARGINAL,
        WhepSrc::static_type(),
    )
}
