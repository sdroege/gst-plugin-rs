//
// Copyright (C) 2021 Bilal Elmoussaoui <bil.elmoussaoui@gmail.com>
// Copyright (C) 2021 Jordan Petridis <jordan@centricular.com>
// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gtk::glib;
use gtk::glib::prelude::*;

mod frame;
mod imp;
mod paintable;

enum SinkEvent {
    FrameChanged,
}

glib::wrapper! {
    pub struct PaintableSink(ObjectSubclass<imp::PaintableSink>)
        @extends gst_video::VideoSink, gst_base::BaseSink, gst::Element, gst::Object;
}

impl PaintableSink {
    pub fn new(name: Option<&str>) -> Self {
        glib::Object::builder().property("name", name).build()
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "gtk4paintablesink",
        gst::Rank::None,
        PaintableSink::static_type(),
    )
}
