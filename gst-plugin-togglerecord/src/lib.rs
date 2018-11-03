// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
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

#![crate_type = "cdylib"]

extern crate glib;
extern crate gobject_subclass;
#[macro_use]
extern crate gst_plugin;
#[macro_use]
extern crate gstreamer as gst;
extern crate gstreamer_video as gst_video;

mod togglerecord;

fn plugin_init(plugin: &gst::Plugin) -> bool {
    togglerecord::register(plugin);
    true
}

plugin_define!(
    b"togglerecord\0",
    b"Toggle Record Plugin\0",
    plugin_init,
    b"0.1.0\0",
    b"LGPL\0",
    b"togglerecord\0",
    b"togglerecord\0",
    b"https://gitlab.freedesktop.org/gstreamer/gst-plugin-rs\0",
    b"2017-12-04\0"
);
