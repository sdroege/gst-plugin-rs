#![crate_type="dylib"]

extern crate libc;
extern crate url;
extern crate hyper;

#[macro_use]
pub mod utils;
pub mod rssource;
pub mod rssink;
pub mod rsfilesrc;
pub mod rshttpsrc;
pub mod rsfilesink;

use utils::*;
use rssource::Source;
use rsfilesrc::FileSrc;
use rshttpsrc::HttpSrc;
use rssink::Sink;
use rsfilesink::FileSink;

use std::os::raw::c_void;
use libc::{c_char};
use std::ffi::CString;

extern "C" {
    fn gst_rs_source_register(plugin: *const c_void,
        name: *const c_char,
        long_name: *const c_char,
        description: *const c_char,
        classification: *const c_char,
        author: *const c_char,
        rank: i32,
        create_instance: extern fn() -> *mut Box<Source>,
        protocols: *const c_char,
        push_only: GBoolean) -> GBoolean;
}

extern "C" {
    fn gst_rs_sink_register(plugin: *const c_void,
        name: *const c_char,
        long_name: *const c_char,
        description: *const c_char,
        classification: *const c_char,
        author: *const c_char,
        rank: i32,
        create_instance: extern fn() -> *mut Box<Sink>,
        protocols: *const c_char) -> GBoolean;
}

#[no_mangle]
pub extern "C" fn sources_register(plugin: *const c_void) -> GBoolean {

    unsafe {
        gst_rs_source_register(plugin,
            CString::new("rsfilesrc").unwrap().as_ptr(),
            CString::new("File Source").unwrap().as_ptr(),
            CString::new("Reads local files").unwrap().as_ptr(),
            CString::new("Source/File").unwrap().as_ptr(),
            CString::new("Sebastian Dröge <sebastian@centricular.com>").unwrap().as_ptr(),
            256 + 100,
            FileSrc::new_ptr,
            CString::new("file").unwrap().as_ptr(),
            GBoolean::False);

        gst_rs_source_register(plugin,
            CString::new("rshttpsrc").unwrap().as_ptr(),
            CString::new("HTTP Source").unwrap().as_ptr(),
            CString::new("Read HTTP/HTTPS files").unwrap().as_ptr(),
            CString::new("Source/Network/HTTP").unwrap().as_ptr(),
            CString::new("Sebastian Dröge <sebastian@centricular.com>").unwrap().as_ptr(),
            256 + 100,
            HttpSrc::new_ptr,
            CString::new("http:https").unwrap().as_ptr(),
            GBoolean::True);
    }

    return GBoolean::True;
}

#[no_mangle]
pub extern "C" fn sinks_register(plugin: *const c_void) -> GBoolean {

    unsafe {
        gst_rs_sink_register(plugin,
            CString::new("rsfilesink").unwrap().as_ptr(),
            CString::new("File Sink").unwrap().as_ptr(),
            CString::new("Writes to local files").unwrap().as_ptr(),
            CString::new("Sink/File").unwrap().as_ptr(),
            CString::new("Luis de Bethencourt <luisbg@osg.samsung.com>").unwrap().as_ptr(),
            256 + 100,
            FileSink::new_ptr,
            CString::new("file").unwrap().as_ptr());
    }
    return GBoolean::True;
}
