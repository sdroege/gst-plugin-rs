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
use std::ffi::{CStr, CString};
use std::slice;
use std::ptr;

use utils::*;

pub trait Source: Sync + Send {
    fn set_uri(&mut self, uri_str: Option<&str>) -> bool;
    fn get_uri(&self) -> Option<String>;
    fn is_seekable(&self) -> bool;
    fn get_size(&self) -> u64;
    fn start(&mut self) -> bool;
    fn stop(&mut self) -> bool;
    fn fill(&mut self, offset: u64, data: &mut [u8]) -> Result<usize, GstFlowReturn>;
    fn do_seek(&mut self, start: u64, stop: u64) -> bool;
}

#[no_mangle]
pub extern "C" fn source_drop(ptr: *mut Box<Source>) {
    unsafe { Box::from_raw(ptr) };
}

#[no_mangle]
pub extern "C" fn source_set_uri(ptr: *mut Box<Source>, uri_ptr: *const c_char) -> GBoolean{
    let source: &mut Box<Source> = unsafe { &mut *ptr };

    if uri_ptr.is_null() {
        GBoolean::from_bool(source.set_uri(None))
    } else {
        let uri = unsafe { CStr::from_ptr(uri_ptr) };
        GBoolean::from_bool(source.set_uri(Some(uri.to_str().unwrap())))
    }
}

#[no_mangle]
pub extern "C" fn source_get_uri(ptr: *mut Box<Source>) -> *mut c_char {
    let source: &mut Box<Source> = unsafe { &mut *ptr };

    match source.get_uri() {
        Some(ref uri) =>
            CString::new(uri.clone().into_bytes()).unwrap().into_raw(),
        None =>
            ptr::null_mut()
    }
}

#[no_mangle]
pub extern "C" fn source_fill(ptr: *mut Box<Source>, offset: u64, data_ptr: *mut u8, data_len_ptr: *mut usize) -> GstFlowReturn {
    let source: &mut Box<Source> = unsafe { &mut *ptr };

    let mut data_len: &mut usize = unsafe { &mut *data_len_ptr };
    let mut data = unsafe { slice::from_raw_parts_mut(data_ptr, *data_len) };
    match source.fill(offset, data) {
        Ok(actual_len) => {
            *data_len = actual_len;
            GstFlowReturn::Ok
        },
        Err(ret) => ret,
    }
}

#[no_mangle]
pub extern "C" fn source_get_size(ptr: *const Box<Source>) -> u64 {
    let source: &Box<Source> = unsafe { & *ptr };

    return source.get_size();
}

#[no_mangle]
pub extern "C" fn source_start(ptr: *mut Box<Source>) -> GBoolean {
    let source: &mut Box<Source> = unsafe { &mut *ptr };

    GBoolean::from_bool(source.start())
}

#[no_mangle]
pub extern "C" fn source_stop(ptr: *mut Box<Source>) -> GBoolean {
    let source: &mut Box<Source> = unsafe { &mut *ptr };

    GBoolean::from_bool(source.stop())
}

#[no_mangle]
pub extern "C" fn source_is_seekable(ptr: *const Box<Source>) -> GBoolean {
    let source: &Box<Source> = unsafe { & *ptr };

    GBoolean::from_bool(source.is_seekable())
}

#[no_mangle]
pub extern "C" fn source_do_seek(ptr: *mut Box<Source>, start: u64, stop: u64) -> GBoolean {
    let source: &mut Box<Source> = unsafe { &mut *ptr };

    GBoolean::from_bool(source.do_seek(start, stop))
}

