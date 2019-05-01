// Copyright (C) 2019 Guillaume Desmottes <guillaume.desmottes@collabora.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![crate_type = "cdylib"]

#[macro_use]
extern crate glib;
#[macro_use]
extern crate gstreamer as gst;

mod cdgdec;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    cdgdec::register(plugin)
}

gst_plugin_define!(
    "cdg",
    "CDG Plugin",
    plugin_init,
    "1.0",
    "MIT/X11",
    "cdg",
    "cdg",
    "https://gitlab.freedesktop.org/gstreamer/gst-plugin-rs",
    "2019-05-01"
);
