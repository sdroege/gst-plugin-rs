// Copyright (C) 2024 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct Transcriber(ObjectSubclass<imp::Transcriber>) @extends gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
    pub struct TranscriberSrcPad(ObjectSubclass<imp::TranscriberSrcPad>) @extends gst::Pad, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        TranscriberSrcPad::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }
    gst::Element::register(
        Some(plugin),
        "speechmaticstranscriber",
        gst::Rank::NONE,
        Transcriber::static_type(),
    )
}
