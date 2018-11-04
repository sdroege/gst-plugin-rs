// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![crate_type = "cdylib"]

extern crate glib;
#[macro_use]
extern crate gst_plugin;
extern crate gst_plugin_simple;
#[macro_use]
extern crate gstreamer as gst;
extern crate reqwest;
extern crate url;

use gst_plugin_simple::source::*;

mod httpsrc;

use httpsrc::HttpSrc;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    source_register(
        plugin,
        SourceInfo {
            name: "rshttpsrc".into(),
            long_name: "HTTP/HTTPS Source".into(),
            description: "Reads HTTP/HTTPS streams".into(),
            classification: "Source/File".into(),
            author: "Sebastian Dröge <sebastian@centricular.com>".into(),
            rank: 256 + 100,
            create_instance: HttpSrc::new_boxed,
            protocols: vec!["http".into(), "https".into()],
            push_only: true,
        },
    );

    Ok(())
}

plugin_define!(
    b"rshttp\0",
    b"Rust HTTP Plugin\0",
    plugin_init,
    b"1.0\0",
    b"MIT/X11\0",
    b"rshttp\0",
    b"rshttp\0",
    b"https://gitlab.freedesktop.org/gstreamer/gst-plugin-rs\0",
    b"2016-12-08\0"
);
