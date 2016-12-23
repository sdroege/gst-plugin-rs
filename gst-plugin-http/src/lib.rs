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

#![crate_type="cdylib"]

extern crate url;
#[macro_use]
extern crate gst_plugin;
extern crate reqwest;

use gst_plugin::plugin::*;
use gst_plugin::source::*;

mod httpsrc;

use httpsrc::HttpSrc;

fn plugin_init(plugin: &Plugin) -> bool {
    source_register(plugin,
                    &SourceInfo {
                        name: "rshttpsrc",
                        long_name: "HTTP/HTTPS Source",
                        description: "Reads HTTP/HTTPS streams",
                        classification: "Source/File",
                        author: "Sebastian Dröge <sebastian@centricular.com>",
                        rank: 256 + 100,
                        create_instance: HttpSrc::new_boxed,
                        protocols: "http:https",
                        push_only: true,
                    });

    true
}

plugin_define!(b"rshttp\0",
               b"Rust HTTP Plugin\0",
               plugin_init,
               b"1.0\0",
               b"LGPL\0",
               b"rshttp\0",
               b"rshttp\0",
               b"https://github.com/sdroege/rsplugin\0",
               b"2016-12-08\0");
