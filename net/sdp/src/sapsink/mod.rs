// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#![allow(unused_doc_comments)]

use gst::glib;
use gst::prelude::*;

mod imp;
mod pad;

glib::wrapper! {
    pub struct SapSink(ObjectSubclass<imp::SapSink>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy;
}

/**
 * GstSapSinkPad:
 *
 * Request pad of a `sapsink` element, representing one announced media stream.
 *
 * Since: plugins-rs-0.16.0
 */
glib::wrapper! {
    pub struct SapSinkPad(ObjectSubclass<pad::SapSinkPad>)
        @extends gst::GhostPad, gst::ProxyPad, gst::Pad, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        imp::RtpProfile::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
        SapSinkPad::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }

    gst::Element::register(
        Some(plugin),
        "sapsink",
        gst::Rank::NONE,
        SapSink::static_type(),
    )
}
