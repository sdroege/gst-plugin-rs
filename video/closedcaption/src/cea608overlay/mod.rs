// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

// Example command-line:
//
// gst-launch-1.0 cccombiner name=ccc ! cea608overlay ! autovideosink \
//   videotestsrc ! video/x-raw, width=1280, height=720 ! queue ! ccc.sink \
//   filesrc location=input.srt ! subparse ! tttocea608 ! queue ! ccc.caption

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct Cea608Overlay(ObjectSubclass<imp::Cea608Overlay>) @extends gst::Element, gst::Object;
}

// GStreamer elements need to be thread-safe. For the private implementation this is automatically
// enforced but for the public wrapper type we need to specify this manually.
unsafe impl Send for Cea608Overlay {}
unsafe impl Sync for Cea608Overlay {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "cea608overlay",
        gst::Rank::Primary,
        Cea608Overlay::static_type(),
    )
}
