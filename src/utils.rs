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

#[macro_export]
macro_rules! println_err(
    ($($arg:tt)*) => { {
        if let Err(_) = writeln!(&mut ::std::io::stderr(), $($arg)*) {
// Ignore when writing fails
        };
    } }
);

#[repr(C)]
pub enum GstFlowReturn {
    Ok = 0,
    NotLinked = -1,
    Flushing = -2,
    Eos = -3,
    NotNegotiated = -4,
    Error = -5,
}

#[repr(C)]
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

#[repr(C)]
pub enum UriError {
    UnsupportedProtocol = 0,
    BadUri,
    BadState,
    BadReference,
}

extern "C" {
    fn g_set_error_literal(err: *mut c_void, domain: u32, code: i32, message: *const c_char);
    fn gst_uri_error_quark() -> u32;
}

impl UriError {
    pub unsafe fn into_gerror(self, err: *mut c_void, message: Option<&String>) {
        if let Some(msg) = message {
            let cmsg = CString::new(msg.as_str()).unwrap();
            g_set_error_literal(err, gst_uri_error_quark(), self as i32, cmsg.as_ptr());
        } else {
            g_set_error_literal(err, gst_uri_error_quark(), self as i32, ptr::null());
        }
    }
}
