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

#[macro_export]
macro_rules! println_err(
    ($($arg:tt)*) => { {
        if let Err(_) = writeln!(&mut ::std::io::stderr(), $($arg)*) {
// Ignore when writing fails
        };
    } }
);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GstFlowReturn {
    Ok = 0,
    NotLinked = -1,
    Flushing = -2,
    Eos = -3,
    NotNegotiated = -4,
    Error = -5,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GBoolean {
    False = 0,
    True = 1,
}

impl GBoolean {
    pub fn from_bool(v: bool) -> GBoolean {
        if v { GBoolean::True } else { GBoolean::False }
    }
}

#[no_mangle]
pub unsafe extern "C" fn cstring_drop(ptr: *mut c_char) {
    CString::from_raw(ptr);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

extern "C" {
    fn g_set_error_literal(err: *mut c_void, domain: u32, code: i32, message: *const c_char);
    fn gst_uri_error_quark() -> u32;
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

    pub fn kind(&self) -> UriErrorKind {
        self.error_kind
    }

    pub unsafe fn into_gerror(self, err: *mut c_void) {
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
