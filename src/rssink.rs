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
use std::ptr;

use std::panic::{self, AssertUnwindSafe};

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

use utils::*;
use error::*;
use buffer::*;

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
    raw: *mut c_void,
    uri: Mutex<(Option<Url>, bool)>,
    uri_validator: Box<UriValidator>,
    sink: Mutex<Box<Sink>>,
    panicked: AtomicBool,
}

pub trait Sink {
    fn uri_validator(&self) -> Box<UriValidator>;

    fn start(&mut self, uri: Url) -> Result<(), ErrorMessage>;
    fn stop(&mut self) -> Result<(), ErrorMessage>;

    fn render(&mut self, buffer: &Buffer) -> Result<(), FlowError>;
}

impl SinkWrapper {
    fn new(raw: *mut c_void, sink: Box<Sink>) -> SinkWrapper {
        SinkWrapper {
            raw: raw,
            uri: Mutex::new((None, false)),
            uri_validator: sink.uri_validator(),
            sink: Mutex::new(sink),
            panicked: AtomicBool::new(false),
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
pub unsafe extern "C" fn sink_set_uri(ptr: *const SinkWrapper,
                                      uri_ptr: *const c_char,
                                      cerr: *mut c_void)
                                      -> GBoolean {
    let wrap: &SinkWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
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
    })
}

#[no_mangle]
pub unsafe extern "C" fn sink_get_uri(ptr: *const SinkWrapper) -> *mut c_char {
    let wrap: &SinkWrapper = &*ptr;

    panic_to_error!(wrap, ptr::null_mut(), {
        let uri_storage = &mut wrap.uri.lock().unwrap();

        match uri_storage.0 {
            Some(ref uri) => CString::new(uri.as_ref().as_bytes()).unwrap().into_raw(),
            None => ptr::null_mut(),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sink_start(ptr: *const SinkWrapper) -> GBoolean {
    let wrap: &SinkWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
        let sink = &mut wrap.sink.lock().unwrap();

        let uri = match *wrap.uri.lock().unwrap() {
            (Some(ref uri), ref mut started) => {
                *started = true;

                uri.clone()
            }
            (None, _) => {
                error_msg!(SinkError::OpenFailed, ["No URI given"]).post(wrap.raw);
                return GBoolean::False;
            }
        };

        match sink.start(uri) {
            Ok(..) => GBoolean::True,
            Err(ref msg) => {
                wrap.uri.lock().unwrap().1 = false;
                msg.post(wrap.raw);
                GBoolean::False
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sink_stop(ptr: *const SinkWrapper) -> GBoolean {
    let wrap: &SinkWrapper = &*ptr;
    panic_to_error!(wrap, GBoolean::False, {
        let sink = &mut wrap.sink.lock().unwrap();

        match sink.stop() {
            Ok(..) => {
                wrap.uri.lock().unwrap().1 = false;
                GBoolean::True
            }
            Err(ref msg) => {
                msg.post(wrap.raw);
                GBoolean::False
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sink_render(ptr: *const SinkWrapper,
                                     buffer: ScopedBufferPtr)
                                     -> GstFlowReturn {
    let wrap: &SinkWrapper = &*ptr;
    panic_to_error!(wrap, GstFlowReturn::Error, {
        let sink = &mut wrap.sink.lock().unwrap();
        let buffer = ScopedBuffer::new(&buffer);

        match sink.render(&buffer) {
            Ok(..) => GstFlowReturn::Ok,
            Err(flow_error) => {
                match flow_error {
                    FlowError::NotNegotiated(ref msg) |
                    FlowError::Error(ref msg) => msg.post(wrap.raw),
                    _ => (),
                }
                flow_error.to_native()
            }
        }
    })
}
