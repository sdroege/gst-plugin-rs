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

#[macro_use]
extern crate lazy_static;

mod audioecho;
mod audioloudnorm;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    audioecho::register(plugin)?;
    audioloudnorm::register(plugin)
}

gst_plugin_define!(
    rsaudiofx,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
