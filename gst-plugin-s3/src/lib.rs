// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
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

mod s3src;
mod s3url;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    s3src::register(plugin)
}

gst_plugin_define!(
    "s3src",
    "Amazon S3 Plugin",
    plugin_init,
    "1.0",
    "MIT/X11",
    "s3",
    "s3",
    "https://github.com/ford-prefect/gst-plugin-s3",
    "2017-04-17"
);
