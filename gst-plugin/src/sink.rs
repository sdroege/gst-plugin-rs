// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//               2016 Luis de Bethencourt <luisbg@osg.samsung.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use libc::c_char;
use std::os::raw::c_void;
use std::ffi::{CStr, CString};
use std::ptr;

use std::panic::{self, AssertUnwindSafe};

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

use slog::*;

use utils::*;
use error::*;
use buffer::*;
use miniobject::*;
use log::*;
use plugin::Plugin;

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
    logger: Logger,
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
            logger: Logger::root(GstDebugDrain::new(Some(unsafe { &Element::new(raw) }),
                                                    "rssink",
                                                    0,
                                                    "Rust sink base class"),
                                 None),
            uri: Mutex::new((None, false)),
            uri_validator: sink.uri_validator(),
            sink: Mutex::new(sink),
            panicked: AtomicBool::new(false),
        }
    }

    fn set_uri(&self, uri_str: Option<&str>) -> Result<(), UriError> {
        let uri_storage = &mut self.uri.lock().unwrap();

        debug!(self.logger, "Setting URI {:?}", uri_str);

        if uri_storage.1 {
            return Err(UriError::new(UriErrorKind::BadState, Some("Already started".to_string())));
        }

        uri_storage.0 = None;

        if let Some(uri_str) = uri_str {
            match Url::parse(uri_str) {
                Ok(uri) => {
                    try!((self.uri_validator)(&uri));
                    uri_storage.0 = Some(uri);
                    Ok(())
                }
                Err(err) => {
                    Err(UriError::new(UriErrorKind::BadUri,
                                      Some(format!("Failed to parse URI '{}': {}", uri_str, err))))
                }
            }
        } else {
            Ok(())
        }
    }

    fn get_uri(&self) -> Option<String> {
        let uri_storage = &self.uri.lock().unwrap();
        uri_storage.0.as_ref().map(|uri| String::from(uri.as_str()))
    }

    fn start(&self) -> bool {
        debug!(self.logger, "Starting");

        // Don't keep the URI locked while we call start later
        let uri = match *self.uri.lock().unwrap() {
            (Some(ref uri), ref mut started) => {
                *started = true;
                uri.clone()
            }
            (None, _) => {
                error!(self.logger, "No URI given");
                self.post_message(&error_msg!(SinkError::OpenFailed, ["No URI given"]));
                return false;
            }
        };

        let sink = &mut self.sink.lock().unwrap();
        match sink.start(uri) {
            Ok(..) => {
                trace!(self.logger, "Started successfully");
                true
            }
            Err(ref msg) => {
                error!(self.logger, "Failed to start: {:?}", msg);

                self.uri.lock().unwrap().1 = false;
                self.post_message(msg);
                false
            }
        }
    }

    fn stop(&self) -> bool {
        let sink = &mut self.sink.lock().unwrap();

        debug!(self.logger, "Stopping");

        match sink.stop() {
            Ok(..) => {
                trace!(self.logger, "Stopped successfully");
                self.uri.lock().unwrap().1 = false;
                true
            }
            Err(ref msg) => {
                error!(self.logger, "Failed to stop: {:?}", msg);

                self.post_message(msg);
                false
            }
        }
    }

    fn render(&self, buffer: &Buffer) -> GstFlowReturn {
        let sink = &mut self.sink.lock().unwrap();

        trace!(self.logger, "Rendering buffer {:?}", buffer);

        match sink.render(buffer) {
            Ok(..) => GstFlowReturn::Ok,
            Err(flow_error) => {
                error!(self.logger, "Failed to render: {:?}", flow_error);
                match flow_error {
                    FlowError::NotNegotiated(ref msg) |
                    FlowError::Error(ref msg) => self.post_message(msg),
                    _ => (),
                }
                flow_error.to_native()
            }
        }
    }

    fn post_message(&self, msg: &ErrorMessage) {
        unsafe {
            msg.post(self.raw);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sink_new(sink: *mut c_void,
                                  create_instance: fn(Element) -> Box<Sink>)
                                  -> *mut SinkWrapper {
    let instance = create_instance(Element::new(sink));
    Box::into_raw(Box::new(SinkWrapper::new(sink, instance)))
}

#[no_mangle]
pub unsafe extern "C" fn sink_drop(ptr: *mut SinkWrapper) {
    let _ = Box::from_raw(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn sink_set_uri(ptr: *const SinkWrapper,
                                      uri_ptr: *const c_char,
                                      cerr: *mut c_void)
                                      -> GBoolean {
    let wrap: &SinkWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
        let uri_str = if uri_ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(uri_ptr).to_str().unwrap())
        };

        match wrap.set_uri(uri_str) {
            Err(err) => {
                error!(wrap.logger, "Failed to set URI {:?}", err);
                err.into_gerror(cerr);
                GBoolean::False
            }
            Ok(_) => GBoolean::True,
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sink_get_uri(ptr: *const SinkWrapper) -> *mut c_char {
    let wrap: &SinkWrapper = &*ptr;

    panic_to_error!(wrap, ptr::null_mut(), {
        match wrap.get_uri() {
            Some(uri_str) => CString::new(uri_str).unwrap().into_raw(),
            None => ptr::null_mut(),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sink_start(ptr: *const SinkWrapper) -> GBoolean {
    let wrap: &SinkWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
        GBoolean::from_bool(wrap.start())
    })
}

#[no_mangle]
pub unsafe extern "C" fn sink_stop(ptr: *const SinkWrapper) -> GBoolean {
    let wrap: &SinkWrapper = &*ptr;
    panic_to_error!(wrap, GBoolean::True, {
        GBoolean::from_bool(wrap.stop())
    })
}

#[no_mangle]
pub unsafe extern "C" fn sink_render(ptr: *const SinkWrapper, buffer: GstRefPtr) -> GstFlowReturn {
    let wrap: &SinkWrapper = &*ptr;
    panic_to_error!(wrap, GstFlowReturn::Error, {
        let buffer: GstRef<Buffer> = GstRef::new(&buffer);
        wrap.render(buffer.as_ref())
    })
}

pub struct SinkInfo<'a> {
    pub name: &'a str,
    pub long_name: &'a str,
    pub description: &'a str,
    pub classification: &'a str,
    pub author: &'a str,
    pub rank: i32,
    pub create_instance: fn(Element) -> Box<Sink>,
    pub protocols: &'a str,
}

pub fn sink_register(plugin: &Plugin, sink_info: &SinkInfo) {
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

    let cname = CString::new(sink_info.name).unwrap();
    let clong_name = CString::new(sink_info.long_name).unwrap();
    let cdescription = CString::new(sink_info.description).unwrap();
    let cclassification = CString::new(sink_info.classification).unwrap();
    let cauthor = CString::new(sink_info.author).unwrap();
    let cprotocols = CString::new(sink_info.protocols).unwrap();

    unsafe {
        gst_rs_sink_register(plugin.as_ptr(),
                             cname.as_ptr(),
                             clong_name.as_ptr(),
                             cdescription.as_ptr(),
                             cclassification.as_ptr(),
                             cauthor.as_ptr(),
                             sink_info.rank,
                             sink_info.create_instance as *const c_void,
                             cprotocols.as_ptr());
    }
}
