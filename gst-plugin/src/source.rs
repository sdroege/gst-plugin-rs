// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
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
use std::u64;

use std::panic::{self, AssertUnwindSafe};

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

use slog::*;

use plugin::Plugin;
use utils::*;
use error::*;
use buffer::*;
use miniobject::*;
use log::*;

use glib;
use gst;

#[derive(Debug)]
pub enum SourceError {
    Failure,
    OpenFailed,
    NotFound,
    ReadFailed,
    SeekFailed,
}

impl ToGError for SourceError {
    fn to_gerror(&self) -> (u32, i32) {
        match *self {
            SourceError::Failure => (gst_library_error_domain(), 1),
            SourceError::OpenFailed => (gst_resource_error_domain(), 5),
            SourceError::NotFound => (gst_resource_error_domain(), 3),
            SourceError::ReadFailed => (gst_resource_error_domain(), 9),
            SourceError::SeekFailed => (gst_resource_error_domain(), 11),
        }
    }
}

pub struct SourceWrapper {
    raw: *mut gst::GstElement,
    logger: Logger,
    uri: Mutex<(Option<Url>, bool)>,
    uri_validator: Box<UriValidator>,
    source: Mutex<Box<Source>>,
    panicked: AtomicBool,
}

pub trait Source {
    fn uri_validator(&self) -> Box<UriValidator>;

    fn is_seekable(&self) -> bool;
    fn get_size(&self) -> Option<u64>;

    fn start(&mut self, uri: Url) -> Result<(), ErrorMessage>;
    fn stop(&mut self) -> Result<(), ErrorMessage>;
    fn fill(&mut self, offset: u64, length: u32, buffer: &mut Buffer) -> Result<(), FlowError>;
    fn seek(&mut self, start: u64, stop: Option<u64>) -> Result<(), ErrorMessage>;
}

impl SourceWrapper {
    fn new(raw: *mut gst::GstElement, source: Box<Source>) -> SourceWrapper {
        SourceWrapper {
            raw: raw,
            logger: Logger::root(GstDebugDrain::new(Some(unsafe { &Element::new(raw) }),
                                                    "rssrc",
                                                    0,
                                                    "Rust source base class"),
                                 None),
            uri: Mutex::new((None, false)),
            uri_validator: source.uri_validator(),
            source: Mutex::new(source),
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

    fn is_seekable(&self) -> bool {
        let source = &self.source.lock().unwrap();
        source.is_seekable()
    }

    fn get_size(&self) -> u64 {
        let source = &self.source.lock().unwrap();
        source.get_size().unwrap_or(u64::MAX)
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
                self.post_message(&error_msg!(SourceError::OpenFailed, ["No URI given"]));
                return false;
            }
        };

        let source = &mut self.source.lock().unwrap();
        match source.start(uri) {
            Ok(..) => {
                trace!(self.logger, "Started successfully");
                true
            }
            Err(ref msg) => {
                error!(self.logger, "Failed to start: {:?}", msg);

                self.uri
                    .lock()
                    .unwrap()
                    .1 = false;
                self.post_message(msg);
                false
            }
        }
    }

    fn stop(&self) -> bool {
        let source = &mut self.source.lock().unwrap();

        debug!(self.logger, "Stopping");

        match source.stop() {
            Ok(..) => {
                trace!(self.logger, "Stopped successfully");
                self.uri
                    .lock()
                    .unwrap()
                    .1 = false;
                true
            }
            Err(ref msg) => {
                error!(self.logger, "Failed to stop: {:?}", msg);

                self.post_message(msg);
                false
            }
        }
    }

    fn fill(&self, offset: u64, length: u32, buffer: &mut Buffer) -> GstFlowReturn {
        let source = &mut self.source.lock().unwrap();

        trace!(self.logger,
               "Filling buffer {:?} with offset {} and length {}",
               buffer,
               offset,
               length);

        match source.fill(offset, length, buffer) {
            Ok(()) => GstFlowReturn::Ok,
            Err(flow_error) => {
                error!(self.logger, "Failed to fill: {:?}", flow_error);
                match flow_error {
                    FlowError::NotNegotiated(ref msg) |
                    FlowError::Error(ref msg) => self.post_message(msg),
                    _ => (),
                }
                flow_error.to_native()
            }
        }
    }

    fn seek(&self, start: u64, stop: Option<u64>) -> bool {
        let source = &mut self.source.lock().unwrap();

        debug!(self.logger, "Seeking to {:?}-{:?}", start, stop);

        match source.seek(start, stop) {
            Ok(..) => true,
            Err(ref msg) => {
                error!(self.logger, "Failed to seek {:?}", msg);
                self.post_message(msg);
                false
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
pub unsafe extern "C" fn source_new(source: *mut gst::GstElement,
                                    create_instance: fn(Element) -> Box<Source>)
                                    -> *mut SourceWrapper {
    let instance = create_instance(Element::new(source));

    Box::into_raw(Box::new(SourceWrapper::new(source, instance)))
}

#[no_mangle]
pub unsafe extern "C" fn source_drop(ptr: *mut SourceWrapper) {
    let _ = Box::from_raw(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn source_set_uri(ptr: *const SourceWrapper,
                                        uri_ptr: *const c_char,
                                        cerr: *mut *mut glib::GError)
                                        -> glib::gboolean {
    let wrap: &SourceWrapper = &*ptr;

    panic_to_error!(wrap, glib::GFALSE, {
        let uri_str = if uri_ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(uri_ptr).to_str().unwrap())
        };

        match wrap.set_uri(uri_str) {
            Err(err) => {
                error!(wrap.logger, "Failed to set URI {:?}", err);
                err.into_gerror(cerr);
                glib::GFALSE
            }
            Ok(_) => glib::GTRUE,
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_get_uri(ptr: *const SourceWrapper) -> *mut c_char {
    let wrap: &SourceWrapper = &*ptr;
    panic_to_error!(wrap, ptr::null_mut(), {
        match wrap.get_uri() {
            Some(uri_str) => CString::new(uri_str).unwrap().into_raw(),
            None => ptr::null_mut(),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_is_seekable(ptr: *const SourceWrapper) -> glib::gboolean {
    let wrap: &SourceWrapper = &*ptr;

    panic_to_error!(wrap, glib::GFALSE, {
        if wrap.is_seekable() {
            glib::GTRUE
        } else {
            glib::GFALSE
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_get_size(ptr: *const SourceWrapper) -> u64 {
    let wrap: &SourceWrapper = &*ptr;
    panic_to_error!(wrap, u64::MAX, {
        wrap.get_size()
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_start(ptr: *const SourceWrapper) -> glib::gboolean {
    let wrap: &SourceWrapper = &*ptr;

    panic_to_error!(wrap, glib::GFALSE, {
        if wrap.start() {
            glib::GTRUE
        } else {
            glib::GFALSE
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_stop(ptr: *const SourceWrapper) -> glib::gboolean {
    let wrap: &SourceWrapper = &*ptr;

    panic_to_error!(wrap, glib::GTRUE, {
        if wrap.stop() {
            glib::GTRUE
        } else {
            glib::GFALSE
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_fill(ptr: *const SourceWrapper,
                                     offset: u64,
                                     length: u32,
                                     buffer: GstRefPtr<Buffer>)
                                     -> GstFlowReturn {
    let wrap: &SourceWrapper = &*ptr;

    panic_to_error!(wrap, GstFlowReturn::Error, {
        let mut buffer: GstRef<Buffer> = GstRef::new(&buffer);
        wrap.fill(offset, length, buffer.get_mut().unwrap())
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_seek(ptr: *const SourceWrapper,
                                     start: u64,
                                     stop: u64)
                                     -> glib::gboolean {
    let wrap: &SourceWrapper = &*ptr;

    panic_to_error!(wrap, glib::GFALSE, {
        if wrap.seek(start, if stop == u64::MAX { None } else { Some(stop) }) {
            glib::GTRUE
        } else {
            glib::GFALSE
        }
    })
}

pub struct SourceInfo<'a> {
    pub name: &'a str,
    pub long_name: &'a str,
    pub description: &'a str,
    pub classification: &'a str,
    pub author: &'a str,
    pub rank: i32,
    pub create_instance: fn(Element) -> Box<Source>,
    pub protocols: &'a str,
    pub push_only: bool,
}

pub fn source_register(plugin: &Plugin, source_info: &SourceInfo) {

    extern "C" {
        fn gst_rs_source_register(plugin: *const gst::GstPlugin,
                                  name: *const c_char,
                                  long_name: *const c_char,
                                  description: *const c_char,
                                  classification: *const c_char,
                                  author: *const c_char,
                                  rank: i32,
                                  create_instance: *const c_void,
                                  protocols: *const c_char,
                                  push_only: glib::gboolean)
                                  -> glib::gboolean;
    }

    let cname = CString::new(source_info.name).unwrap();
    let clong_name = CString::new(source_info.long_name).unwrap();
    let cdescription = CString::new(source_info.description).unwrap();
    let cclassification = CString::new(source_info.classification).unwrap();
    let cauthor = CString::new(source_info.author).unwrap();
    let cprotocols = CString::new(source_info.protocols).unwrap();

    unsafe {
        gst_rs_source_register(plugin.as_ptr(),
                               cname.as_ptr(),
                               clong_name.as_ptr(),
                               cdescription.as_ptr(),
                               cclassification.as_ptr(),
                               cauthor.as_ptr(),
                               source_info.rank,
                               source_info.create_instance as *const c_void,
                               cprotocols.as_ptr(),
                               if source_info.push_only {
                                   glib::GTRUE
                               } else {
                                   glib::GFALSE
                               });
    }
}
