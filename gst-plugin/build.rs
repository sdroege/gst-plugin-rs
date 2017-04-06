// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

extern crate gcc;
extern crate pkg_config;

fn main() {
    let gstreamer = pkg_config::probe_library("gstreamer-1.0").unwrap();
    let gstbase = pkg_config::probe_library("gstreamer-base-1.0").unwrap();
    let includes = [gstreamer.include_paths, gstbase.include_paths];

    let files = ["src/error.c", "src/log.c", "src/source.c", "src/sink.c", "src/demuxer.c"];

    let mut config = gcc::Config::new();
    config.include("src");

    for f in files.iter() {
        config.file(f);
    }

    for p in includes.iter().flat_map(|i| i) {
        config.include(p);
    }

    config.compile("librsplugin-c.a");
}
