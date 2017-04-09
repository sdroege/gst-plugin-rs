// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ffi::CString;
use std::ptr;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fmt::Error as FmtError;
use std::borrow::Cow;

use url::Url;

use utils::*;

use glib;
use gst;

#[macro_export]
macro_rules! error_msg(
// Plain strings
    ($err:expr, ($msg:expr), [$dbg:expr]) =>  {
        ErrorMessage::new(&$err, Some(From::from($msg)),
                          Some(From::from($dbg)),
                          file!(), module_path!(), line!())
    };
    ($err:expr, ($msg:expr)) => {
        ErrorMessage::new(&$err, Some(From::from($msg)),
                          None,
                          file!(), module_path!(), line!())
    };
    ($err:expr, [$dbg:expr]) => {
        ErrorMessage::new(&$err, None,
                          Some(From::from($dbg)),
                          file!(), module_path!(), line!())
    };

// Format strings
    ($err:expr, ($($msg:tt)*), [$($dbg:tt)*]) =>  { {
        ErrorMessage::new(&$err, Some(From::from(format!($($msg)*))),
                          From::from(Some(format!($($dbg)*))),
                          file!(), module_path!(), line!())
    }};
    ($err:expr, ($($msg:tt)*)) =>  { {
        ErrorMessage::new(&$err, Some(From::from(format!($($msg)*))),
                          None,
                          file!(), module_path!(), line!())
    }};

    ($err:expr, [$($dbg:tt)*]) =>  { {
        ErrorMessage::new(&$err, None,
                          Some(From::from(format!($($dbg)*))),
                          file!(), module_path!(), line!())
    }};
);

pub trait ToGError {
    fn to_gerror(&self) -> (u32, i32);
}

pub fn gst_library_error_domain() -> glib::GQuark {
    unsafe { gst::gst_library_error_quark() }
}

pub fn gst_resource_error_domain() -> glib::GQuark {
    unsafe { gst::gst_resource_error_quark() }
}

#[derive(Debug)]
pub struct ErrorMessage {
    pub error_domain: u32,
    pub error_code: i32,
    pub message: Option<String>,
    pub debug: Option<String>,
    pub filename: &'static str,
    pub function: &'static str,
    pub line: u32,
}

impl ErrorMessage {
    pub fn new<T: ToGError>(error: &T,
                            message: Option<Cow<str>>,
                            debug: Option<Cow<str>>,
                            filename: &'static str,
                            function: &'static str,
                            line: u32)
                            -> ErrorMessage {
        let (gdomain, gcode) = error.to_gerror();

        ErrorMessage {
            error_domain: gdomain,
            error_code: gcode,
            message: message.map(|m| m.into_owned()),
            debug: debug.map(|d| d.into_owned()),
            filename: filename,
            function: function,
            line: line,
        }
    }


    pub unsafe fn post(&self, element: *mut gst::GstElement) {
        let ErrorMessage { error_domain,
                           error_code,
                           ref message,
                           ref debug,
                           filename,
                           function,
                           line } = *self;

        let message_cstr = message.as_ref().map(|m| CString::new(m.as_bytes()).unwrap());
        let message_ptr = message_cstr.as_ref().map_or(ptr::null(), |m| m.as_ptr());

        let debug_cstr = debug.as_ref().map(|m| CString::new(m.as_bytes()).unwrap());
        let debug_ptr = debug_cstr.as_ref().map_or(ptr::null(), |m| m.as_ptr());

        let file_cstr = CString::new(filename.as_bytes()).unwrap();
        let file_ptr = file_cstr.as_ptr();

        let function_cstr = CString::new(function.as_bytes()).unwrap();
        let function_ptr = function_cstr.as_ptr();

        gst::gst_element_message_full(element,
                                      gst::GST_MESSAGE_ERROR,
                                      error_domain,
                                      error_code,
                                      glib::g_strdup(message_ptr),
                                      glib::g_strdup(debug_ptr),
                                      file_ptr,
                                      function_ptr,
                                      line as i32);
    }
}

#[derive(Debug)]
pub enum FlowError {
    Flushing,
    Eos,
    NotNegotiated(ErrorMessage),
    Error(ErrorMessage),
}

impl FlowError {
    pub fn to_native(&self) -> GstFlowReturn {
        match *self {
            FlowError::Flushing => GstFlowReturn::Flushing,
            FlowError::Eos => GstFlowReturn::Eos,
            FlowError::NotNegotiated(..) => GstFlowReturn::NotNegotiated,
            FlowError::Error(..) => GstFlowReturn::Error,
        }
    }
}

impl Display for FlowError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), FmtError> {
        match *self {
            FlowError::Flushing | FlowError::Eos => f.write_str(self.description()),
            FlowError::NotNegotiated(ref m) => {
                f.write_fmt(format_args!("{}: {} ({})",
                                         self.description(),
                                         m.message.as_ref().map_or("None", |s| s.as_str()),
                                         m.debug.as_ref().map_or("None", |s| s.as_str())))
            }
            FlowError::Error(ref m) => {
                f.write_fmt(format_args!("{}: {} ({})",
                                         self.description(),
                                         m.message.as_ref().map_or("None", |s| s.as_str()),
                                         m.debug.as_ref().map_or("None", |s| s.as_str())))
            }
        }
    }
}

impl Error for FlowError {
    fn description(&self) -> &str {
        match *self {
            FlowError::Flushing => "Flushing",
            FlowError::Eos => "Eos",
            FlowError::NotNegotiated(..) => "Not Negotiated",
            FlowError::Error(..) => "Error",
        }
    }
}

#[derive(Debug)]
pub enum UriErrorKind {
    UnsupportedProtocol = 0,
    BadUri,
    BadState,
    BadReference,
}

#[derive(Debug)]
pub struct UriError {
    error_kind: UriErrorKind,
    message: Option<String>,
}

impl UriError {
    pub fn new(error_kind: UriErrorKind, message: Option<String>) -> UriError {
        UriError {
            error_kind: error_kind,
            message: message,
        }
    }

    pub fn message(&self) -> &Option<String> {
        &self.message
    }

    pub fn kind(&self) -> &UriErrorKind {
        &self.error_kind
    }

    pub unsafe fn into_gerror(self, err: *mut *mut glib::GError) {
        if let Some(msg) = self.message {
            let cmsg = CString::new(msg.as_str()).unwrap();
            glib::g_set_error_literal(err,
                                      gst::gst_uri_error_quark(),
                                      self.error_kind as i32,
                                      cmsg.as_ptr());
        } else {
            glib::g_set_error_literal(err,
                                      gst::gst_uri_error_quark(),
                                      self.error_kind as i32,
                                      ptr::null());
        }
    }
}

impl Display for UriError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), FmtError> {
        match self.message {
            None => f.write_str(self.description()),
            Some(ref message) => f.write_fmt(format_args!("{}: {}", self.description(), message)),
        }
    }
}

impl Error for UriError {
    fn description(&self) -> &str {
        match self.error_kind {
            UriErrorKind::UnsupportedProtocol => "Unsupported protocol",
            UriErrorKind::BadUri => "Bad URI",
            UriErrorKind::BadState => "Bad State",
            UriErrorKind::BadReference => "Bad Reference",
        }
    }
}

pub type UriValidator = Fn(&Url) -> Result<(), UriError>;

#[derive(Debug)]
pub struct PanicError;

impl ToGError for PanicError {
    fn to_gerror(&self) -> (u32, i32) {
        (gst_library_error_domain(), 1)
    }
}

#[macro_export]
macro_rules! panic_to_error(
    ($wrap:expr, $ret:expr, $code:block) => {{
        if $wrap.panicked.load(Ordering::Relaxed) {
            error_msg!(PanicError, ["Panicked"]).post($wrap.raw);
            return $ret;
        }

        let result = panic::catch_unwind(AssertUnwindSafe(|| $code));

        match result {
            Ok(result) => result,
            Err(err) => {
                $wrap.panicked.store(true, Ordering::Relaxed);
                if let Some(cause) = err.downcast_ref::<&str>() {
                    error_msg!(PanicError, ["Panicked: {}", cause]).post($wrap.raw);
                } else if let Some(cause) = err.downcast_ref::<String>() {
                    error_msg!(PanicError, ["Panicked: {}", cause]).post($wrap.raw);
                } else {
                    error_msg!(PanicError, ["Panicked"]).post($wrap.raw);
                }
                $ret
            }
        }
    }}
);
