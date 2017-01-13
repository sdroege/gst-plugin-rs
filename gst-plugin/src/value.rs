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
use std::mem;

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

#[repr(C)]
pub struct GValue {
    typ: usize,
    data: [u64; 2],
}

// See gtype.h
const TYPE_BOOLEAN: usize = (5 << 2);
const TYPE_INT: usize = (6 << 2);
const TYPE_STRING: usize = (16 << 2);

impl Value {
    pub fn to_gvalue(&self) -> GValue {
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

impl From<bool> for Value {
    fn from(f: bool) -> Value {
        Value::Bool(f)
    }
}

impl From<i32> for Value {
    fn from(f: i32) -> Value {
        Value::Int(f)
    }
}

impl From<String> for Value {
    fn from(f: String) -> Value {
        Value::String(f)
    }
}

impl<'a> From<&'a str> for Value {
    fn from(f: &'a str) -> Value {
        Value::String(f.into())
    }
}

impl From<(i32, i32)> for Value {
    fn from((f_n, f_d): (i32, i32)) -> Value {
        Value::Fraction(f_n, f_d)
    }
}

impl From<Buffer> for Value {
    fn from(f: Buffer) -> Value {
        Value::Buffer(f)
    }
}

impl<'a> From<&'a Buffer> for Value {
    fn from(f: &'a Buffer) -> Value {
        Value::Buffer(f.clone())
    }
}

impl From<Vec<Value>> for Value {
    fn from(f: Vec<Value>) -> Value {
        Value::Array(f)
    }
}

impl<'a> From<&'a [Value]> for Value {
    fn from(f: &'a [Value]) -> Value {
        Value::Array(f.to_vec())
    }
}
