// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![crate_type = "cdylib"]

extern crate byte_slice_cast;
#[macro_use]
extern crate glib;
#[macro_use]
extern crate gstreamer as gst;
extern crate gstreamer_audio as gst_audio;
extern crate gstreamer_base as gst_base;
extern crate num_traits;

mod audioecho;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    audioecho::register(plugin)
}

gst_plugin_define!(
    rsaudiofx,
    "Rust AudioFx Plugin",
    plugin_init,
    "1.0",
    "MIT/X11",
    "rsaudiofx",
    "rsaudiofx",
    "https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs",
    "2016-12-08"
);
