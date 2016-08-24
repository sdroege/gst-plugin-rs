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

use libc::c_char;
use std::os::raw::c_void;
use std::ffi::{CStr, CString};
use std::slice;
use std::ptr;

use url::Url;

use utils::*;

#[derive(Debug)]
pub struct SourceController {
    source: *mut c_void,
}

impl SourceController {
    fn new(source: *mut c_void) -> SourceController {
        SourceController { source: source }
    }
}

pub trait Source: Sync + Send {
    fn get_controller(&self) -> &SourceController;

    // Called from any thread at any time
    fn set_uri(&self, uri: Option<Url>) -> Result<(), UriError>;
    fn get_uri(&self) -> Option<Url>;

    // Called from any thread between start/stop
    fn is_seekable(&self) -> bool;

    // Called from the streaming thread only
    fn start(&self) -> bool;
    fn stop(&self) -> bool;
    fn fill(&self, offset: u64, data: &mut [u8]) -> Result<usize, GstFlowReturn>;
    fn do_seek(&self, start: u64, stop: u64) -> bool;
    fn get_size(&self) -> u64;
}

#[no_mangle]
pub extern "C" fn source_new(source: *mut c_void,
                             create_instance: fn(controller: SourceController) -> Box<Source>)
                             -> *mut Box<Source> {
    Box::into_raw(Box::new(create_instance(SourceController::new(source))))
}

#[no_mangle]
pub unsafe extern "C" fn source_drop(ptr: *mut Box<Source>) {
    Box::from_raw(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn source_set_uri(ptr: *mut Box<Source>,
                                        uri_ptr: *const c_char,
                                        cerr: *mut c_void)
                                        -> GBoolean {
    let source: &mut Box<Source> = &mut *ptr;

    if uri_ptr.is_null() {
        if let Err(err) = source.set_uri(None) {
            err.into_gerror(cerr);
            GBoolean::False
        } else {
            GBoolean::True
        }
    } else {
        let uri_str = CStr::from_ptr(uri_ptr).to_str().unwrap();

        match Url::parse(uri_str) {
            Ok(uri) => {
                if let Err(err) = source.set_uri(Some(uri)) {
                    err.into_gerror(cerr);
                    GBoolean::False
                } else {
                    GBoolean::True
                }
            }
            Err(err) => {
                let _ = source.set_uri(None);
                UriError::new(UriErrorKind::BadUri,
                              Some(format!("Failed to parse URI '{}': {}", uri_str, err)))
                    .into_gerror(cerr);
                GBoolean::False
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn source_get_uri(ptr: *mut Box<Source>) -> *mut c_char {
    let source: &mut Box<Source> = &mut *ptr;

    match source.get_uri() {
        Some(uri) => CString::new(uri.into_string().into_bytes()).unwrap().into_raw(),
        None => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn source_fill(ptr: *mut Box<Source>,
                                     offset: u64,
                                     data_ptr: *mut u8,
                                     data_len_ptr: *mut usize)
                                     -> GstFlowReturn {
    let source: &mut Box<Source> = &mut *ptr;

    let mut data_len: &mut usize = &mut *data_len_ptr;
    let mut data = slice::from_raw_parts_mut(data_ptr, *data_len);

    match source.fill(offset, data) {
        Ok(actual_len) => {
            *data_len = actual_len;
            GstFlowReturn::Ok
        }
        Err(ret) => ret,
    }
}

#[no_mangle]
pub unsafe extern "C" fn source_get_size(ptr: *const Box<Source>) -> u64 {
    let source: &Box<Source> = &*ptr;

    source.get_size()
}

#[no_mangle]
pub unsafe extern "C" fn source_start(ptr: *mut Box<Source>) -> GBoolean {
    let source: &mut Box<Source> = &mut *ptr;

    GBoolean::from_bool(source.start())
}

#[no_mangle]
pub unsafe extern "C" fn source_stop(ptr: *mut Box<Source>) -> GBoolean {
    let source: &mut Box<Source> = &mut *ptr;

    GBoolean::from_bool(source.stop())
}

#[no_mangle]
pub unsafe extern "C" fn source_is_seekable(ptr: *const Box<Source>) -> GBoolean {
    let source: &Box<Source> = &*ptr;

    GBoolean::from_bool(source.is_seekable())
}

#[no_mangle]
pub unsafe extern "C" fn source_do_seek(ptr: *mut Box<Source>, start: u64, stop: u64) -> GBoolean {
    let source: &mut Box<Source> = &mut *ptr;

    GBoolean::from_bool(source.do_seek(start, stop))
}
