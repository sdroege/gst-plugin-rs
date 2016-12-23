//  Copyright (C) 2016 Sebastian Dröge <sebastian@centricular.com>
//                2016 Luis de Bethencourt <luisbg@osg.samsung.com>
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

extern crate libc;
extern crate url;
extern crate reqwest;
#[macro_use]
extern crate bitflags;
#[macro_use]
extern crate nom;
extern crate flavors;

#[macro_use]
pub mod utils;
#[macro_use]
pub mod error;
pub mod buffer;
pub mod adapter;
#[macro_use]
pub mod plugin;
pub mod rssource;
pub mod rssink;
pub mod rsfilesrc;
pub mod rshttpsrc;
pub mod rsfilesink;
pub mod rsdemuxer;
pub mod flvdemux;

use plugin::*;
use rssource::*;
use rsfilesrc::FileSrc;
use rshttpsrc::HttpSrc;
use rssink::*;
use rsfilesink::FileSink;
use rsdemuxer::*;
use flvdemux::FlvDemux;

fn plugin_init(plugin: &Plugin) -> bool {
    source_register(plugin,
                    &SourceInfo {
                        name: "rsfilesrc",
                        long_name: "File Source",
                        description: "Reads local files",
                        classification: "Source/File",
                        author: "Sebastian Dröge <sebastian@centricular.com>",
                        rank: 256 + 100,
                        create_instance: FileSrc::new_boxed,
                        protocols: "file",
                        push_only: false,
                    });

    source_register(plugin,
                    &SourceInfo {
                        name: "rshttpsrc",
                        long_name: "HTTP Source",
                        description: "Reads HTTP/HTTPS streams",
                        classification: "Source/Network/HTTP",
                        author: "Sebastian Dröge <sebastian@centricular.com>",
                        rank: 256 + 100,
                        create_instance: HttpSrc::new_boxed,
                        protocols: "http:https",
                        push_only: true,
                    });

    sink_register(plugin,
                  &SinkInfo {
                      name: "rsfilesink",
                      long_name: "File Sink",
                      description: "Writes to local files",
                      classification: "Sink/File",
                      author: "Luis de Bethencourt <luisbg@osg.samsung.com>",
                      rank: 256 + 100,
                      create_instance: FileSink::new_boxed,
                      protocols: "file",
                  });

    demuxer_register(plugin,
                     &DemuxerInfo {
                         name: "rsflvdemux",
                         long_name: "FLV Demuxer",
                         description: "Demuxes FLV Streams",
                         classification: "Codec/Demuxer",
                         author: "Sebastian Dröge <sebastian@centricular.com>",
                         rank: 256 + 100,
                         create_instance: FlvDemux::new_boxed,
                         input_formats: "video/x-flv",
                         output_formats: "ANY",
                     });

    true
}

plugin_define!(b"rsplugin\0",
               b"Rust Plugin\0",
               plugin_init,
               b"1.0\0",
               b"LGPL\0",
               b"rsplugin\0",
               b"rsplugin\0",
               b"https://github.com/sdroege/rsplugin\0",
               b"2016-12-08\0");
