// Copyright (C) 2021 Mathieu Duponchelle <mathieu@centricular.com>
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
// gst-launch-1.0 filesrc location=animated.webp ! webpdec-rs ! videoconvert ! autovideosink

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct WebPDec(ObjectSubclass<imp::WebPDec>) @extends gst::Element, gst::Object;
}

// GStreamer elements need to be thread-safe. For the private implementation this is automatically
// enforced but for the public wrapper type we need to specify this manually.
unsafe impl Send for WebPDec {}
unsafe impl Sync for WebPDec {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "webpdec-rs",
        gst::Rank::Primary,
        WebPDec::static_type(),
    )
}
