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

use libc::c_char;
use std::os::raw::c_void;
use std::ffi::CString;
use std::ffi::CStr;
use std::mem;
use std::fmt;

use buffer::*;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Value {
    Bool(bool),
    Int(i32),
    String(String),
    Fraction(i32, i32),
    Buffer(Buffer),
    Array(Vec<Value>),
}

pub struct Caps(*mut c_void);

#[repr(C)]
struct GValue {
    typ: usize,
    data: [u64; 2],
}

// See gtype.h
const TYPE_BOOLEAN: usize = (5 << 2);
const TYPE_INT: usize = (6 << 2);
const TYPE_STRING: usize = (16 << 2);

impl Value {
    fn to_gvalue(&self) -> GValue {
        extern "C" {
            fn g_value_init(value: *mut GValue, gtype: usize);
            fn g_value_set_boolean(value: *mut GValue, value: i32);
            fn g_value_set_int(value: *mut GValue, value: i32);
            fn g_value_set_string(value: *mut GValue, value: *const c_char);
            fn gst_value_set_fraction(value: *mut GValue, value_n: i32, value_d: i32);
            fn gst_fraction_get_type() -> usize;
            fn g_value_set_boxed(value: *mut GValue, boxed: *const c_void);
            fn gst_buffer_get_type() -> usize;
            fn gst_value_array_get_type() -> usize;
            fn gst_value_array_append_and_take_value(value: *mut GValue, element: *mut GValue);
        }

        let mut gvalue: GValue = unsafe { mem::zeroed() };

        match *self {
            Value::Bool(v) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, TYPE_BOOLEAN);
                g_value_set_boolean(&mut gvalue as *mut GValue, if v { 1 } else { 0 });
            },
            Value::Int(v) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, TYPE_INT);
                g_value_set_int(&mut gvalue as *mut GValue, v);
            },
            Value::String(ref v) => unsafe {
                let v_cstr = CString::new(String::from(v.clone())).unwrap();

                g_value_init(&mut gvalue as *mut GValue, TYPE_STRING);
                g_value_set_string(&mut gvalue as *mut GValue, v_cstr.as_ptr());
            },
            Value::Fraction(v_n, v_d) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, gst_fraction_get_type());
                gst_value_set_fraction(&mut gvalue as *mut GValue, v_n, v_d);
            },
            Value::Buffer(ref buffer) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, gst_buffer_get_type());
                g_value_set_boxed(&mut gvalue as *mut GValue, buffer.as_ptr());
            },
            Value::Array(ref array) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, gst_value_array_get_type());

                for e in array {
                    let mut e_value = e.to_gvalue();
                    gst_value_array_append_and_take_value(&mut gvalue as *mut GValue,
                                                          &mut e_value as *mut GValue);
                    // Takes ownership, invalidate GValue
                    e_value.typ = 0;
                }
            },
        }

        gvalue
    }
}

impl Drop for GValue {
    fn drop(&mut self) {
        extern "C" {
            fn g_value_unset(value: *mut GValue);
        }

        if self.typ != 0 {
            unsafe { g_value_unset(self as *mut GValue) }
        }
    }
}

impl Caps {
    pub fn new_empty() -> Self {
        extern "C" {
            fn gst_caps_new_empty() -> *mut c_void;
        }

        Caps(unsafe { gst_caps_new_empty() })
    }

    pub fn new_any() -> Self {
        extern "C" {
            fn gst_caps_new_any() -> *mut c_void;
        }

        Caps(unsafe { gst_caps_new_any() })
    }

    pub fn new_simple(name: &str, values: &[(&str, &Value)]) -> Self {
        extern "C" {
            fn gst_caps_append_structure(caps: *mut c_void, structure: *mut c_void);
            fn gst_structure_new_empty(name: *const c_char) -> *mut c_void;
        }

        let mut caps = Caps::new_empty();

        let name_cstr = CString::new(name).unwrap();
        let structure = unsafe { gst_structure_new_empty(name_cstr.as_ptr()) };

        unsafe {
            gst_caps_append_structure(caps.0, structure);
        }

        caps.set_simple(values);

        caps
    }

    pub fn from_string(value: &str) -> Option<Self> {
        extern "C" {
            fn gst_caps_from_string(value: *const c_char) -> *mut c_void;
        }

        let value_cstr = CString::new(value).unwrap();

        let caps_ptr = unsafe { gst_caps_from_string(value_cstr.as_ptr()) };

        if caps_ptr.is_null() {
            None
        } else {
            Some(Caps(caps_ptr))
        }
    }

    pub fn set_simple(&mut self, values: &[(&str, &Value)]) {
        extern "C" {
            fn gst_caps_set_value(caps: *mut c_void, name: *const c_char, value: *const GValue);
        }

        for value in values {
            let name_cstr = CString::new(value.0).unwrap();
            let mut gvalue = value.1.to_gvalue();

            unsafe {
                gst_caps_set_value(self.0, name_cstr.as_ptr(), &mut gvalue as *mut GValue);
            }
        }
    }

    pub fn to_string(&self) -> String {
        extern "C" {
            fn gst_caps_to_string(caps: *mut c_void) -> *mut c_char;
        }

        unsafe { CStr::from_ptr(gst_caps_to_string(self.0)).to_string_lossy().into_owned() }
    }

    pub unsafe fn as_ptr(&self) -> *const c_void {
        self.0
    }

    pub fn make_writable(self: Caps) -> Caps {
        extern "C" {
            fn gst_mini_object_make_writable(obj: *mut c_void) -> *mut c_void;
        }

        let raw = unsafe { gst_mini_object_make_writable(self.0) };

        Caps(raw)
    }

    pub fn copy(&self) -> Caps {
        extern "C" {
            fn gst_mini_object_copy(obj: *const c_void) -> *mut c_void;
        }
        unsafe { Caps(gst_mini_object_copy(self.0)) }
    }
}

impl Clone for Caps {
    fn clone(&self) -> Self {
        extern "C" {
            fn gst_mini_object_ref(mini_object: *mut c_void) -> *mut c_void;
        }

        unsafe { Caps(gst_mini_object_ref(self.0)) }
    }
}

impl Drop for Caps {
    fn drop(&mut self) {
        extern "C" {
            fn gst_mini_object_unref(mini_object: *mut c_void);
        }

        unsafe { gst_mini_object_unref(self.0) }
    }
}

impl fmt::Debug for Caps {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;
    use std::os::raw::c_void;

    fn init() {
        extern "C" {
            fn gst_init(argc: *mut c_void, argv: *mut c_void);
        }

        unsafe {
            gst_init(ptr::null_mut(), ptr::null_mut());
        }
    }

    #[test]
    fn test_simple() {
        init();

        let caps = Caps::new_simple("foo/bar",
                                    &[("int", &Value::Int(12)),
                                      ("bool", &Value::Bool(true)),
                                      ("string", &Value::String("bla".into())),
                                      ("fraction", &Value::Fraction(1, 2)),
                                      ("array",
                                       &Value::Array(vec![Value::Int(1), Value::Int(2)]))]);
        assert_eq!(caps.to_string(),
                   "foo/bar, int=(int)12, bool=(boolean)true, string=(string)bla, \
                    fraction=(fraction)1/2, array=(int)< 1, 2 >");
    }
}
