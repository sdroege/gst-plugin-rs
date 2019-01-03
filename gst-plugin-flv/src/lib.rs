// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
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
extern crate gstreamer_base as gst_base;

#[macro_use]
extern crate lazy_static;

mod bytes;
mod flvdemux;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    flvdemux::register(plugin)
}

gst_plugin_define!(
    "rsflv",
    "Rust FLV Plugin",
    plugin_init,
    "1.0",
    "MIT/X11",
    "rsflv",
    "rsflv",
    "https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs",
    "2016-12-08"
);
