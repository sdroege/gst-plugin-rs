// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
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

mod rav1enc;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    rav1enc::register(plugin)
}

gst_plugin_define!(
    "rav1e",
    "rav1e AV1 Encoder Plugin",
    plugin_init,
    "1.0",
    "MIT/X11",
    "rav1e",
    "rav1e",
    "https://gitlab.freedesktop.org/gstreamer/gst-plugin-rs",
    "2019-05-25"
);
