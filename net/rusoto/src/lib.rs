// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;

mod aws_transcriber;
mod s3sink;
mod s3src;
mod s3url;
mod s3utils;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    s3sink::register(plugin)?;
    s3src::register(plugin)?;
    aws_transcriber::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    rusoto,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
