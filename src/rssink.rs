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

use std::sync::Mutex;

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

pub struct SinkWrapper {
    sink_raw: *mut c_void,
    uri: Mutex<(Option<Url>, bool)>,
    uri_validator: Box<UriValidator>,
    sink: Mutex<Box<Sink>>,
}

pub trait Sink {
    fn uri_validator(&self) -> Box<UriValidator>;

    fn start(&mut self, uri: &Url) -> Result<(), ErrorMessage>;
    fn stop(&mut self) -> Result<(), ErrorMessage>;

    fn render(&mut self, data: &[u8]) -> Result<(), FlowError>;
}

impl SinkWrapper {
    fn new(sink_raw: *mut c_void, sink: Box<Sink>) -> SinkWrapper {
        SinkWrapper {
            sink_raw: sink_raw,
            uri: Mutex::new((None, false)),
            uri_validator: sink.uri_validator(),
            sink: Mutex::new(sink),
        }
    }
}

#[no_mangle]
pub extern "C" fn sink_new(sink: *mut c_void,
                           create_instance: fn() -> Box<Sink>)
                           -> *mut SinkWrapper {
    Box::into_raw(Box::new(SinkWrapper::new(sink, create_instance())))
}

#[no_mangle]
pub unsafe extern "C" fn sink_drop(ptr: *mut SinkWrapper) {
    Box::from_raw(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn sink_set_uri(ptr: *mut SinkWrapper,
                                      uri_ptr: *const c_char,
                                      cerr: *mut c_void)
                                      -> GBoolean {
    let wrap: &mut SinkWrapper = &mut *ptr;
    let uri_storage = &mut wrap.uri.lock().unwrap();

    if uri_storage.1 {
        UriError::new(UriErrorKind::BadState, Some("Already started".to_string()))
            .into_gerror(cerr);
        return GBoolean::False;
    }

    uri_storage.0 = None;
    if uri_ptr.is_null() {
        GBoolean::True
    } else {
        let uri_str = CStr::from_ptr(uri_ptr).to_str().unwrap();

        match Url::parse(uri_str) {
            Ok(uri) => {
                if let Err(err) = (*wrap.uri_validator)(&uri) {
                    err.into_gerror(cerr);

                    GBoolean::False
                } else {
                    uri_storage.0 = Some(uri);

                    GBoolean::True
                }
            }
            Err(err) => {
                UriError::new(UriErrorKind::BadUri,
                              Some(format!("Failed to parse URI '{}': {}", uri_str, err)))
                    .into_gerror(cerr);

                GBoolean::False
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sink_get_uri(ptr: *const SinkWrapper) -> *mut c_char {
    let wrap: &SinkWrapper = &*ptr;
    let uri_storage = &mut wrap.uri.lock().unwrap();

    match uri_storage.0 {
        Some(ref uri) => CString::new(uri.as_ref().as_bytes()).unwrap().into_raw(),
        None => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sink_render(ptr: *mut SinkWrapper,
                                     data_ptr: *const u8,
                                     data_len: usize)
                                     -> GstFlowReturn {
    let wrap: &mut SinkWrapper = &mut *ptr;
    let sink = &mut wrap.sink.lock().unwrap();
    let data = slice::from_raw_parts(data_ptr, data_len);

    match sink.render(data) {
        Ok(..) => GstFlowReturn::Ok,
        Err(flow_error) => {
            match flow_error {
                FlowError::NotNegotiated(ref msg) |
                FlowError::Error(ref msg) => msg.post(wrap.sink_raw),
                _ => (),
            }
            flow_error.to_native()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sink_start(ptr: *mut SinkWrapper) -> GBoolean {
    let wrap: &mut SinkWrapper = &mut *ptr;
    let sink = &mut wrap.sink.lock().unwrap();
    let uri_storage = &mut wrap.uri.lock().unwrap();

    let (uri, started) = match **uri_storage {
        (Some(ref uri), ref mut started) => (uri, started),
        (None, _) => {
            error_msg!(SinkError::OpenFailed, ["No URI given"]).post(wrap.sink_raw);
            return GBoolean::False;
        }
    };

    match sink.start(uri) {
        Ok(..) => {
            *started = true;

            GBoolean::True
        }
        Err(ref msg) => {
            msg.post(wrap.sink_raw);
            GBoolean::False
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sink_stop(ptr: *mut SinkWrapper) -> GBoolean {
    let wrap: &mut SinkWrapper = &mut *ptr;
    let sink = &mut wrap.sink.lock().unwrap();
    let uri_storage = &mut wrap.uri.lock().unwrap();

    match sink.stop() {
        Ok(..) => {
            uri_storage.1 = false;
            GBoolean::True
        }
        Err(ref msg) => {
            msg.post(wrap.sink_raw);
            GBoolean::False
        }
    }
}
