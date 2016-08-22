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

use libc::c_char;
use std::os::raw::c_void;
use std::ffi::{CStr, CString};
use std::slice;
use std::ptr;
use std::io::Write;

use url::Url;

use utils::*;

#[derive(Debug)]
pub struct SinkController {
    sink: *mut c_void,
}

impl SinkController {
    fn new(sink: *mut c_void) -> SinkController {
        SinkController { sink: sink }
    }
}

pub trait Sink: Sync + Send {
    // Called from any thread at any time
    fn set_uri(&self, uri: Option<Url>) -> bool;
    fn get_uri(&self) -> Option<Url>;

    // Called from the streaming thread only
    fn start(&self) -> bool;
    fn stop(&self) -> bool;
    fn render(&self, data: &[u8]) -> GstFlowReturn;
}

#[no_mangle]
pub extern "C" fn sink_new(sink: *mut c_void,
                           create_instance: fn(controller: SinkController) -> Box<Sink>)
                           -> *mut Box<Sink> {
    Box::into_raw(Box::new(create_instance(SinkController::new(sink))))
}

#[no_mangle]
pub extern "C" fn sink_drop(sink: *mut c_void, ptr: *mut Box<Sink>) {
    unsafe { Box::from_raw(ptr) };
}

#[no_mangle]
pub extern "C" fn sink_set_uri(sink: *mut c_void,
                               ptr: *mut Box<Sink>,
                               uri_ptr: *const c_char)
                               -> GBoolean {
    let sink: &mut Box<Sink> = unsafe { &mut *ptr };

    if uri_ptr.is_null() {
        GBoolean::from_bool(sink.set_uri(None))
    } else {
        let uri_str = unsafe { CStr::from_ptr(uri_ptr) }.to_str().unwrap();
        match Url::parse(uri_str) {
            Ok(uri) => GBoolean::from_bool(sink.set_uri(Some(uri))),
            Err(err) => {
                sink.set_uri(None);
                println_err!("Failed to parse URI '{}': {}", uri_str, err);
                GBoolean::False
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn sink_get_uri(sink: *mut c_void, ptr: *const Box<Sink>) -> *mut c_char {
    let sink: &Box<Sink> = unsafe { &*ptr };

    match sink.get_uri() {
        Some(uri) => CString::new(uri.into_string().into_bytes()).unwrap().into_raw(),
        None => ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn sink_render(sink: *mut c_void,
                              ptr: *mut Box<Sink>,
                              data_ptr: *const u8,
                              data_len: usize)
                              -> GstFlowReturn {
    let sink: &mut Box<Sink> = unsafe { &mut *ptr };

    let data = unsafe { slice::from_raw_parts(data_ptr, data_len) };
    sink.render(data)
}

#[no_mangle]
pub extern "C" fn sink_start(sink: *mut c_void, ptr: *mut Box<Sink>) -> GBoolean {
    let sink: &mut Box<Sink> = unsafe { &mut *ptr };

    GBoolean::from_bool(sink.start())
}

#[no_mangle]
pub extern "C" fn sink_stop(sink: *mut c_void, ptr: *mut Box<Sink>) -> GBoolean {
    let sink: &mut Box<Sink> = unsafe { &mut *ptr };

    GBoolean::from_bool(sink.stop())
}
