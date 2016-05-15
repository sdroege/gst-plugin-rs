use libc::{c_char};
use std::ffi::{CStr, CString};
use std::slice;
use std::ptr;

use utils::*;

pub trait Source: Sync + Send {
    fn set_uri(&mut self, uri_str: &Option<String>) -> bool;
    fn get_uri(&self) -> Option<String>;
    fn is_seekable(&self) -> bool;
    fn get_size(&self) -> u64;
    fn start(&mut self) -> bool;
    fn stop(&mut self) -> bool;
    fn fill(&mut self, offset: u64, data: &mut [u8]) -> Result<usize, GstFlowReturn>;
    fn do_seek(&mut self, start: u64, stop: u64) -> bool {
        return true;
    }
}

#[no_mangle]
pub extern "C" fn source_drop(ptr: *mut Box<Source>) {
    unsafe { Box::from_raw(ptr) };
}

#[no_mangle]
pub extern "C" fn source_set_uri(ptr: *mut Box<Source>, uri_ptr: *const c_char) -> GBoolean{
    let source: &mut Box<Source> = unsafe { &mut *ptr };

    if uri_ptr.is_null() {
        GBoolean::from_bool(source.set_uri(&None))
    } else {
        let uri = unsafe { CStr::from_ptr(uri_ptr) };
        GBoolean::from_bool(source.set_uri(&Some(String::from(uri.to_str().unwrap()))))
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
pub extern "C" fn source_get_size(ptr: *mut Box<Source>) -> u64 {
    let source: &mut Box<Source> = unsafe { &mut *ptr };

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
pub extern "C" fn source_is_seekable(ptr: *mut Box<Source>) -> GBoolean {
    let source: &mut Box<Source> = unsafe { &mut *ptr };

    GBoolean::from_bool(source.is_seekable())
}

#[no_mangle]
pub extern "C" fn source_do_seek(ptr: *mut Box<Source>, start: u64, stop: u64) -> GBoolean {
    let source: &mut Box<Source> = unsafe { &mut *ptr };

    GBoolean::from_bool(source.do_seek(start, stop))
}

