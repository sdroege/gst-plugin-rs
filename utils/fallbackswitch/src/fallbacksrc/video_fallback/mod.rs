// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2020 Seungha Yang <seungha@centricular.com>
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

use gst::glib;

mod imp;

glib::wrapper! {
    pub struct VideoFallbackSource(ObjectSubclass<imp::VideoFallbackSource>) @extends gst::Bin, gst::Element, gst::Object;
}

// GStreamer elements need to be thread-safe. For the private implementation this is automatically
// enforced but for the public wrapper type we need to specify this manually.
unsafe impl Send for VideoFallbackSource {}
unsafe impl Sync for VideoFallbackSource {}

impl VideoFallbackSource {
    pub fn new(uri: Option<&str>, min_latency: gst::ClockTime) -> VideoFallbackSource {
        glib::Object::new(&[("uri", &uri), ("min-latency", &min_latency.nseconds())]).unwrap()
    }
}
