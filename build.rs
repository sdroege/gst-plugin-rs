//  Copyright (C) 2016 Sebastian Dröge <sebastian@centricular.com>
//
//  This library is free software; you can redistribute it and/or
//  modify it under the terms of the GNU Library General Public
//  License as published by the Free Software Foundation; either
//  version 2 of the License, or (at your option) any later version.
//
//  This library is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
//  Library General Public License for more details.
//
//  You should have received a copy of the GNU Library General Public
//  License along with this library; if not, write to the
//  Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
//  Boston, MA 02110-1301, USA.

extern crate gcc;
extern crate pkg_config;

fn main() {
    let gstreamer = pkg_config::probe_library("gstreamer-1.0").unwrap();
    let gstbase = pkg_config::probe_library("gstreamer-base-1.0").unwrap();
    let includes = [gstreamer.include_paths, gstbase.include_paths];

    let files = ["src/plugin.c", "src/rssource.c", "src/rssink.c", "src/rsdemuxer.c"];

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
