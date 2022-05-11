//
// Copyright (C) 2022 Vivienne Watermeier <vwatermeier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(clippy::new_without_default)]

use glib::Object;
use gst::glib;
use gst::prelude::*;

pub mod imp;

glib::wrapper! {
    pub struct RTPAv1Pay(ObjectSubclass<imp::RTPAv1Pay>)
        @extends gst_rtp::RTPBasePayload, gst::Element, gst::Object;
}

impl RTPAv1Pay {
    pub fn new() -> Self {
        Object::new(&[]).expect("Failed to create AV1 payloader")
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rtpav1pay",
        gst::Rank::Marginal,
        RTPAv1Pay::static_type(),
    )
}
