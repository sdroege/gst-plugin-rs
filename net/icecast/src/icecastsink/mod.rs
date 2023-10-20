// GStreamer Icecast Sink
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

mod client;
mod imp;
mod mediaformat;
mod utils; // FIXME: move those into mediaformat?

glib::wrapper! {
    pub struct IcecastSink(ObjectSubclass<imp::IcecastSink>) @extends gst_base::BaseSink, gst::Element, gst::Object, @implements gst::URIHandler;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "icecastsink",
        gst::Rank::NONE,
        IcecastSink::static_type(),
    )
}
