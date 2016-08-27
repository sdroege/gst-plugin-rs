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
use std::ffi::CString;

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
