use libc::{c_char};
use std::ffi::{CStr, CString};
use std::ptr;

use utils::*;

pub trait Sink {
    fn set_uri(&mut self, uri_str: &Option<String>) -> bool;
    fn get_uri(&self) -> Option<String>;
    fn start(&mut self) -> bool;
    fn stop(&mut self) -> bool;
    fn render(&mut self) -> Result<usize, GstFlowReturn>;
}

#[no_mangle]
pub extern "C" fn sink_set_uri(ptr: *mut Box<Sink>, uri_ptr: *const c_char) -> GBoolean{
    let source: &mut Box<Sink> = unsafe { &mut *ptr };

    if uri_ptr.is_null() {
        GBoolean::from_bool(source.set_uri(&None))
    } else {
        let uri = unsafe { CStr::from_ptr(uri_ptr) };
        GBoolean::from_bool(source.set_uri(&Some(String::from(uri.to_str().unwrap()))))
    }
}

#[no_mangle]
pub extern "C" fn sink_get_uri(ptr: *mut Box<Sink>) -> *mut c_char {
    let source: &mut Box<Sink> = unsafe { &mut *ptr };

    match source.get_uri() {
        Some(ref uri) =>
            CString::new(uri.clone().into_bytes()).unwrap().into_raw(),
        None =>
            ptr::null_mut()
    }
}

#[no_mangle]
pub extern "C" fn sink_render(ptr: *mut Box<Sink>) -> GstFlowReturn {
    let source: &mut Box<Sink> = unsafe { &mut *ptr };

    match source.render() {
        Ok(data) => {
            GstFlowReturn::Ok
        },
        Err(ret) => ret,
    }
}

#[no_mangle]
pub extern "C" fn sink_start(ptr: *mut Box<Sink>) -> GBoolean {
    let source: &mut Box<Sink> = unsafe { &mut *ptr };

    GBoolean::from_bool(source.start())
}

#[no_mangle]
pub extern "C" fn sink_stop(ptr: *mut Box<Sink>) -> GBoolean {
    let source: &mut Box<Sink> = unsafe { &mut *ptr };

    GBoolean::from_bool(source.stop())
}
