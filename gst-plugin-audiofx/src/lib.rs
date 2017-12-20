// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![crate_type = "cdylib"]

extern crate byte_slice_cast;
extern crate glib;
#[macro_use]
extern crate gst_plugin;
extern crate gstreamer as gst;
extern crate gstreamer_audio as gst_audio;
extern crate gstreamer_base as gst_base;
extern crate num_traits;

mod audioecho;

fn plugin_init(plugin: &gst::Plugin) -> bool {
    audioecho::register(plugin);
    true
}

plugin_define!(
    b"rsaudiofx\0",
    b"Rust AudioFx Plugin\0",
    plugin_init,
    b"1.0\0",
    b"MIT/X11\0",
    b"rsaudiofx\0",
    b"rsaudiofx\0",
    b"https://github.com/sdroege/rsplugin\0",
    b"2016-12-08\0"
);
