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

use url::Url;

use utils::*;
use error::*;

#[derive(Debug)]
pub enum SinkError {
    Failure,
    OpenFailed,
    NotFound,
    WriteFailed,
    SeekFailed,
}

impl ToGError for SinkError {
    fn to_gerror(&self) -> (u32, i32) {
        match *self {
            SinkError::Failure => (gst_library_error_domain(), 1),
            SinkError::OpenFailed => (gst_resource_error_domain(), 6),
            SinkError::NotFound => (gst_resource_error_domain(), 3),
            SinkError::WriteFailed => (gst_resource_error_domain(), 10),
            SinkError::SeekFailed => (gst_resource_error_domain(), 11),
        }
    }
}

#[derive(Debug)]
pub struct SinkController {
    sink: *mut c_void,
}

impl SinkController {
    fn new(sink: *mut c_void) -> SinkController {
        SinkController { sink: sink }
    }

    pub fn error(&self, error: &ErrorMessage) {
        extern "C" {
            fn gst_rs_sink_error(sink: *mut c_void,
                                 error_domain: u32,
                                 error_code: i32,
                                 message: *const c_char,
                                 debug: *const c_char,
                                 filename: *const c_char,
                                 function: *const c_char,
                                 line: u32);
        }

        let ErrorMessage { error_domain,
                           error_code,
                           ref message,
                           ref debug,
                           filename,
                           function,
                           line } = *error;

        let message_cstr = message.as_ref().map(|m| CString::new(m.as_bytes()).unwrap());
        let message_ptr = message_cstr.as_ref().map_or(ptr::null(), |m| m.as_ptr());

        let debug_cstr = debug.as_ref().map(|m| CString::new(m.as_bytes()).unwrap());
        let debug_ptr = debug_cstr.as_ref().map_or(ptr::null(), |m| m.as_ptr());

        let file_cstr = CString::new(filename.as_bytes()).unwrap();
        let file_ptr = file_cstr.as_ptr();

        let function_cstr = CString::new(function.as_bytes()).unwrap();
        let function_ptr = function_cstr.as_ptr();

        unsafe {
            gst_rs_sink_error(self.sink,
                              error_domain,
                              error_code,
                              message_ptr,
                              debug_ptr,
                              file_ptr,
                              function_ptr,
                              line);
        }
    }
}

pub trait Sink: Sync + Send {
    fn get_controller(&self) -> &SinkController;

    // Called from any thread at any time
    fn set_uri(&self, uri: Option<Url>) -> Result<(), UriError>;
    fn get_uri(&self) -> Option<Url>;

    // Called from the streaming thread only
    fn start(&self) -> Result<(), ErrorMessage>;
    fn stop(&self) -> Result<(), ErrorMessage>;
    fn render(&self, data: &[u8]) -> Result<(), FlowError>;
}

#[no_mangle]
pub extern "C" fn sink_new(sink: *mut c_void,
                           create_instance: fn(controller: SinkController) -> Box<Sink>)
                           -> *mut Box<Sink> {
    Box::into_raw(Box::new(create_instance(SinkController::new(sink))))
}

#[no_mangle]
pub unsafe extern "C" fn sink_drop(ptr: *mut Box<Sink>) {
    Box::from_raw(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn sink_set_uri(ptr: *mut Box<Sink>,
                                      uri_ptr: *const c_char,
                                      cerr: *mut c_void)
                                      -> GBoolean {
    let sink: &mut Box<Sink> = &mut *ptr;

    if uri_ptr.is_null() {
        if let Err(err) = sink.set_uri(None) {
            err.into_gerror(cerr);
            GBoolean::False
        } else {
            GBoolean::True
        }
    } else {
        let uri_str = CStr::from_ptr(uri_ptr).to_str().unwrap();

        match Url::parse(uri_str) {
            Ok(uri) => {
                if let Err(err) = sink.set_uri(Some(uri)) {
                    err.into_gerror(cerr);
                    GBoolean::False
                } else {
                    GBoolean::True
                }
            }
            Err(err) => {
                let _ = sink.set_uri(None);
                UriError::new(UriErrorKind::BadUri,
                              Some(format!("Failed to parse URI '{}': {}", uri_str, err)))
                    .into_gerror(cerr);
                GBoolean::False
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sink_get_uri(ptr: *const Box<Sink>) -> *mut c_char {
    let sink: &Box<Sink> = &*ptr;

    match sink.get_uri() {
        Some(uri) => CString::new(uri.into_string().into_bytes()).unwrap().into_raw(),
        None => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sink_render(ptr: *mut Box<Sink>,
                                     data_ptr: *const u8,
                                     data_len: usize)
                                     -> GstFlowReturn {
    let sink: &mut Box<Sink> = &mut *ptr;
    let data = slice::from_raw_parts(data_ptr, data_len);

    match sink.render(data) {
        Ok(..) => GstFlowReturn::Ok,
        Err(flow_error) => {
            match flow_error {
                FlowError::NotNegotiated(ref msg) |
                FlowError::Error(ref msg) => sink.get_controller().error(msg),
                _ => (),
            }
            flow_error.to_native()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sink_start(ptr: *mut Box<Sink>) -> GBoolean {
    let sink: &mut Box<Sink> = &mut *ptr;

    match sink.start() {
        Ok(..) => GBoolean::True,
        Err(ref msg) => {
            sink.get_controller().error(msg);
            GBoolean::False
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sink_stop(ptr: *mut Box<Sink>) -> GBoolean {
    let sink: &mut Box<Sink> = &mut *ptr;

    match sink.stop() {
        Ok(..) => GBoolean::True,
        Err(ref msg) => {
            sink.get_controller().error(msg);
            GBoolean::False
        }
    }
}
