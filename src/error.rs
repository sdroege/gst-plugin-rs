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
//
//
use libc::c_char;
use std::os::raw::c_void;
use std::ffi::CString;
use std::ptr;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fmt::Error as FmtError;
use std::borrow::Cow;

use url::Url;

use utils::*;

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

pub fn gst_library_error_domain() -> u32 {
    extern "C" {
        fn gst_library_error_quark() -> u32;
    }

    unsafe { gst_library_error_quark() }
}

pub fn gst_resource_error_domain() -> u32 {
    extern "C" {
        fn gst_resource_error_quark() -> u32;
    }

    unsafe { gst_resource_error_quark() }
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


    pub unsafe fn post(&self, element: *mut c_void) {
        extern "C" {
            fn gst_rs_element_error(sink: *mut c_void,
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
                           line } = *self;

        let message_cstr = message.as_ref().map(|m| CString::new(m.as_bytes()).unwrap());
        let message_ptr = message_cstr.as_ref().map_or(ptr::null(), |m| m.as_ptr());

        let debug_cstr = debug.as_ref().map(|m| CString::new(m.as_bytes()).unwrap());
        let debug_ptr = debug_cstr.as_ref().map_or(ptr::null(), |m| m.as_ptr());

        let file_cstr = CString::new(filename.as_bytes()).unwrap();
        let file_ptr = file_cstr.as_ptr();

        let function_cstr = CString::new(function.as_bytes()).unwrap();
        let function_ptr = function_cstr.as_ptr();

        gst_rs_element_error(element,
                             error_domain,
                             error_code,
                             message_ptr,
                             debug_ptr,
                             file_ptr,
                             function_ptr,
                             line);
    }
}

#[derive(Debug)]
pub enum FlowError {
    NotLinked,
    Flushing,
    Eos,
    NotNegotiated(ErrorMessage),
    Error(ErrorMessage),
}

impl FlowError {
    pub fn to_native(&self) -> GstFlowReturn {
        match *self {
            FlowError::NotLinked => GstFlowReturn::NotLinked,
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
            FlowError::NotLinked | FlowError::Flushing | FlowError::Eos => {
                f.write_str(self.description())
            }
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
            FlowError::NotLinked => "Not Linked",
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

    pub unsafe fn into_gerror(self, err: *mut c_void) {
        extern "C" {
            fn g_set_error_literal(err: *mut c_void,
                                   domain: u32,
                                   code: i32,
                                   message: *const c_char);
            fn gst_uri_error_quark() -> u32;
        }


        if let Some(msg) = self.message {
            let cmsg = CString::new(msg.as_str()).unwrap();
            g_set_error_literal(err,
                                gst_uri_error_quark(),
                                self.error_kind as i32,
                                cmsg.as_ptr());
        } else {
            g_set_error_literal(err,
                                gst_uri_error_quark(),
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
