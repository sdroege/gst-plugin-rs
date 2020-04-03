// Copyright (C) 2019 Philippe Normand <philn@igalia.com>
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
extern crate dav1d;
extern crate gstreamer_base as gst_base;
extern crate gstreamer_video as gst_video;
#[macro_use]
extern crate lazy_static;

mod dav1ddec;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    dav1ddec::register(plugin)?;
    Ok(())
}

gst_plugin_define!(
    rsdav1d,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
