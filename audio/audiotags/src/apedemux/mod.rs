// Copyright (C) 2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/> .
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*};

#[allow(clippy::module_inception)]
mod apedemux;
mod apetag;
mod utils;

glib::wrapper! {
    pub struct ApeTagDemux(ObjectSubclass<apedemux::ApeTagDemux>) @extends gst_tag::TagDemux, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "apetagdemux",
        gst::Rank::PRIMARY + 1,
        ApeTagDemux::static_type(),
    )
}

#[allow(clippy::module_inception)]
#[cfg(test)]
mod tests;
