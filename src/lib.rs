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
pub mod rssource;
pub mod rssink;
pub mod rsfilesrc;
pub mod rshttpsrc;
pub mod rsfilesink;
pub mod rsdemuxer;
pub mod flvdemux;

use utils::*;
use rsfilesrc::FileSrc;
use rshttpsrc::HttpSrc;
use rsfilesink::FileSink;
use flvdemux::FlvDemux;

use std::os::raw::c_void;
use libc::c_char;
use std::ffi::CString;

unsafe fn source_register(plugin: *const c_void,
                          name: &str,
                          long_name: &str,
                          description: &str,
                          classification: &str,
                          author: &str,
                          rank: i32,
                          create_instance: *const c_void,
                          protocols: &str,
                          push_only: bool) {

    extern "C" {
        fn gst_rs_source_register(plugin: *const c_void,
                                  name: *const c_char,
                                  long_name: *const c_char,
                                  description: *const c_char,
                                  classification: *const c_char,
                                  author: *const c_char,
                                  rank: i32,
                                  create_instance: *const c_void,
                                  protocols: *const c_char,
                                  push_only: GBoolean)
                                  -> GBoolean;
    }

    let cname = CString::new(name).unwrap();
    let clong_name = CString::new(long_name).unwrap();
    let cdescription = CString::new(description).unwrap();
    let cclassification = CString::new(classification).unwrap();
    let cauthor = CString::new(author).unwrap();
    let cprotocols = CString::new(protocols).unwrap();

    gst_rs_source_register(plugin,
                           cname.as_ptr(),
                           clong_name.as_ptr(),
                           cdescription.as_ptr(),
                           cclassification.as_ptr(),
                           cauthor.as_ptr(),
                           rank,
                           create_instance,
                           cprotocols.as_ptr(),
                           GBoolean::from_bool(push_only));

}

unsafe fn sources_register(plugin: *const c_void) -> GBoolean {
    source_register(plugin,
                    "rsfilesrc",
                    "File Source",
                    "Reads local files",
                    "Source/File",
                    "Sebastian Dröge <sebastian@centricular.com>",
                    256 + 100,
                    FileSrc::new_boxed as *const c_void,
                    "file",
                    false);

    source_register(plugin,
                    "rshttpsrc",
                    "HTTP Source",
                    "Read HTTP/HTTPS files",
                    "Source/Network/HTTP",
                    "Sebastian Dröge <sebastian@centricular.com>",
                    256 + 100,
                    HttpSrc::new_boxed as *const c_void,
                    "http:https",
                    true);

    GBoolean::True
}

unsafe fn sink_register(plugin: *const c_void,
                        name: &str,
                        long_name: &str,
                        description: &str,
                        classification: &str,
                        author: &str,
                        rank: i32,
                        create_instance: *const c_void,
                        protocols: &str) {
    extern "C" {
        fn gst_rs_sink_register(plugin: *const c_void,
                                name: *const c_char,
                                long_name: *const c_char,
                                description: *const c_char,
                                classification: *const c_char,
                                author: *const c_char,
                                rank: i32,
                                create_instance: *const c_void,
                                protocols: *const c_char)
                                -> GBoolean;
    }

    let cname = CString::new(name).unwrap();
    let clong_name = CString::new(long_name).unwrap();
    let cdescription = CString::new(description).unwrap();
    let cclassification = CString::new(classification).unwrap();
    let cauthor = CString::new(author).unwrap();
    let cprotocols = CString::new(protocols).unwrap();

    gst_rs_sink_register(plugin,
                         cname.as_ptr(),
                         clong_name.as_ptr(),
                         cdescription.as_ptr(),
                         cclassification.as_ptr(),
                         cauthor.as_ptr(),
                         rank,
                         create_instance,
                         cprotocols.as_ptr());
}

unsafe fn sinks_register(plugin: *const c_void) -> GBoolean {
    sink_register(plugin,
                  "rsfilesink",
                  "File Sink",
                  "Writes to local files",
                  "Sink/File",
                  "Luis de Bethencourt <luisbg@osg.samsung.com>",
                  256 + 100,
                  FileSink::new_boxed as *const c_void,
                  "file");

    GBoolean::True
}

unsafe fn demuxer_register(plugin: *const c_void,
                           name: &str,
                           long_name: &str,
                           description: &str,
                           classification: &str,
                           author: &str,
                           rank: i32,
                           create_instance: *const c_void,
                           input_format: &str,
                           output_formats: &str) {
    extern "C" {
        fn gst_rs_demuxer_register(plugin: *const c_void,
                                   name: *const c_char,
                                   long_name: *const c_char,
                                   description: *const c_char,
                                   classification: *const c_char,
                                   author: *const c_char,
                                   rank: i32,
                                   create_instance: *const c_void,
                                   input_format: *const c_char,
                                   output_formats: *const c_char)
                                   -> GBoolean;
    }

    let cname = CString::new(name).unwrap();
    let clong_name = CString::new(long_name).unwrap();
    let cdescription = CString::new(description).unwrap();
    let cclassification = CString::new(classification).unwrap();
    let cauthor = CString::new(author).unwrap();
    let cinput_format = CString::new(input_format).unwrap();
    let coutput_formats = CString::new(output_formats).unwrap();

    gst_rs_demuxer_register(plugin,
                            cname.as_ptr(),
                            clong_name.as_ptr(),
                            cdescription.as_ptr(),
                            cclassification.as_ptr(),
                            cauthor.as_ptr(),
                            rank,
                            create_instance,
                            cinput_format.as_ptr(),
                            coutput_formats.as_ptr());
}

unsafe fn demuxers_register(plugin: *const c_void) -> GBoolean {
    demuxer_register(plugin,
                     "rsflvdemux",
                     "FLV Demuxer",
                     "Demuxes FLV Streams",
                     "Codec/Demuxer",
                     "Sebastian Dröge <sebastian@centricular.com>",
                     256 + 100,
                     FlvDemux::new_boxed as *const c_void,
                     "video/x-flv",
                     "ANY");

    GBoolean::True
}

#[repr(C)]
pub struct GstPluginDesc {
    major_version: i32,
    minor_version: i32,
    name: *const u8,
    description: *const u8,
    plugin_init: extern "C" fn(plugin: *const c_void) -> GBoolean,
    version: *const u8,
    license: *const u8,
    source: *const u8,
    package: *const u8,
    origin: *const u8,
    release_datetime: *const u8,
    _gst_reserved: [usize; 4],
}

unsafe impl Sync for GstPluginDesc {}

extern "C" fn plugin_init(plugin: *const c_void) -> GBoolean {
    unsafe {
        sources_register(plugin);
        sinks_register(plugin);
        demuxers_register(plugin);
    }

    GBoolean::True
}

#[no_mangle]
#[allow(non_upper_case_globals)]
pub static gst_plugin_desc: GstPluginDesc = GstPluginDesc {
    major_version: 1,
    minor_version: 10,
    name: b"rsplugin\0" as *const u8,
    description: b"Rust Plugin\0" as *const u8,
    plugin_init: plugin_init,
    version: b"1.0\0" as *const u8,
    license: b"LGPL\0" as *const u8,
    source: b"rsplugin\0" as *const u8,
    package: b"rsplugin\0" as *const u8,
    origin: b"https://github.com/sdroege/rsplugin\0" as *const u8,
    release_datetime: b"2016-12-08\0" as *const u8,
    _gst_reserved: [0, 0, 0, 0],
};
