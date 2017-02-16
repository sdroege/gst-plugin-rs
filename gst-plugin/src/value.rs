// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use libc::c_char;
use std::os::raw::c_void;
use std::ffi::{CString, CStr};
use std::mem;
use std::marker::PhantomData;
use std::ops::Deref;

pub use num_rational::Rational32;

use buffer::*;
use miniobject::*;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Value {
    Bool(bool),
    Int(i32),
    UInt(u32),
    Int64(i64),
    UInt64(u64),
    String(String),
    Fraction(Rational32),
    Buffer(GstRc<Buffer>),
    Array(Vec<Value>),
}

pub trait ValueType {
    fn extract(v: &Value) -> Option<&Self>;
}

macro_rules! impl_value_type(
    ($t:ty, $variant:ident) => {
        impl ValueType for $t {
            fn extract(v: &Value) -> Option<&$t> {
                if let Value::$variant(ref b) = *v {
                    return Some(b);
                }
                None
            }
        }
    };
);

impl_value_type!(bool, Bool);
impl_value_type!(i32, Int);
impl_value_type!(u32, UInt);
impl_value_type!(i64, Int64);
impl_value_type!(u64, UInt64);
impl_value_type!(String, String);
impl_value_type!(Rational32, Fraction);
impl_value_type!(GstRc<Buffer>, Buffer);
impl_value_type!(Vec<Value>, Array);

#[repr(C)]
pub struct GValue {
    typ: usize,
    data: [u64; 2],
}

impl GValue {
    pub fn new() -> GValue {
        unsafe { mem::zeroed() }
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

// See gtype.h
const TYPE_BOOLEAN: usize = (5 << 2);
const TYPE_INT: usize = (6 << 2);
const TYPE_UINT: usize = (7 << 2);
const TYPE_INT64: usize = (10 << 2);
const TYPE_UINT64: usize = (11 << 2);
const TYPE_STRING: usize = (16 << 2);

extern "C" {
    fn gst_buffer_get_type() -> usize;
    fn gst_fraction_get_type() -> usize;
    fn gst_value_array_get_type() -> usize;
}

lazy_static! {
    static ref TYPE_BUFFER: usize = unsafe { gst_buffer_get_type() };
    static ref TYPE_FRACTION: usize = unsafe { gst_fraction_get_type() };
    static ref TYPE_GST_VALUE_ARRAY: usize = unsafe { gst_value_array_get_type() };
}

impl Value {
    pub fn to_gvalue(&self) -> GValue {
        extern "C" {
            fn g_value_init(value: *mut GValue, gtype: usize);
            fn g_value_set_boolean(value: *mut GValue, value: i32);
            fn g_value_set_int(value: *mut GValue, value: i32);
            fn g_value_set_uint(value: *mut GValue, value: u32);
            fn g_value_set_int64(value: *mut GValue, value: i64);
            fn g_value_set_uint64(value: *mut GValue, value: u64);
            fn g_value_set_string(value: *mut GValue, value: *const c_char);
            fn gst_value_set_fraction(value: *mut GValue, value_n: i32, value_d: i32);
            fn g_value_set_boxed(value: *mut GValue, boxed: *const c_void);
            fn gst_value_array_append_and_take_value(value: *mut GValue, element: *mut GValue);
        }

        let mut gvalue = GValue::new();

        match *self {
            Value::Bool(v) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, TYPE_BOOLEAN);
                g_value_set_boolean(&mut gvalue as *mut GValue, if v { 1 } else { 0 });
            },
            Value::Int(v) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, TYPE_INT);
                g_value_set_int(&mut gvalue as *mut GValue, v);
            },
            Value::UInt(v) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, TYPE_UINT);
                g_value_set_uint(&mut gvalue as *mut GValue, v);
            },
            Value::Int64(v) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, TYPE_INT64);
                g_value_set_int64(&mut gvalue as *mut GValue, v);
            },
            Value::UInt64(v) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, TYPE_UINT64);
                g_value_set_uint64(&mut gvalue as *mut GValue, v);
            },
            Value::String(ref v) => unsafe {
                let v_cstr = CString::new(String::from(v.clone())).unwrap();

                g_value_init(&mut gvalue as *mut GValue, TYPE_STRING);
                g_value_set_string(&mut gvalue as *mut GValue, v_cstr.as_ptr());
            },
            Value::Fraction(ref v) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, *TYPE_FRACTION);
                gst_value_set_fraction(&mut gvalue as *mut GValue, *v.numer(), *v.denom());
            },
            Value::Buffer(ref buffer) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, *TYPE_BUFFER);
                g_value_set_boxed(&mut gvalue as *mut GValue, buffer.as_ptr());
            },
            Value::Array(ref array) => unsafe {
                g_value_init(&mut gvalue as *mut GValue, *TYPE_GST_VALUE_ARRAY);

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

    pub fn from_gvalue(gvalue: &GValue) -> Option<Self> {
        extern "C" {
            fn g_value_get_boolean(value: *const GValue) -> i32;
            fn g_value_get_int(value: *const GValue) -> i32;
            fn g_value_get_uint(value: *const GValue) -> u32;
            fn g_value_get_int64(value: *const GValue) -> i64;
            fn g_value_get_uint64(value: *const GValue) -> u64;
            fn g_value_get_string(value: *const GValue) -> *const c_char;
            fn gst_value_get_fraction_numerator(value: *const GValue) -> i32;
            fn gst_value_get_fraction_denominator(value: *const GValue) -> i32;
            fn g_value_get_boxed(value: *const GValue) -> *mut c_void;
            fn gst_value_array_get_size(value: *const GValue) -> u32;
            fn gst_value_array_get_value(value: *const GValue, index: u32) -> *const GValue;
        }

        match gvalue.typ {
            TYPE_BOOLEAN => unsafe {
                Some(Value::Bool(!(g_value_get_boolean(gvalue as *const GValue) == 0)))
            },
            TYPE_INT => unsafe { Some(Value::Int(g_value_get_int(gvalue as *const GValue))) },
            TYPE_UINT => unsafe { Some(Value::UInt(g_value_get_uint(gvalue as *const GValue))) },
            TYPE_INT64 => unsafe { Some(Value::Int64(g_value_get_int64(gvalue as *const GValue))) },
            TYPE_UINT64 => unsafe {
                Some(Value::UInt64(g_value_get_uint64(gvalue as *const GValue)))
            },
            TYPE_STRING => unsafe {
                let s = g_value_get_string(gvalue as *const GValue);
                if s.is_null() {
                    return None;
                }

                let cstr = CStr::from_ptr(s);
                match cstr.to_str() {
                    Err(_) => None,
                    Ok(s) => Some(Value::String(s.into())),
                }
            },
            typ if typ == *TYPE_FRACTION => unsafe {
                let n = gst_value_get_fraction_numerator(gvalue as *const GValue);
                let d = gst_value_get_fraction_denominator(gvalue as *const GValue);

                Some(Value::Fraction(Rational32::new(n, d)))
            },
            typ if typ == *TYPE_BUFFER => unsafe {
                let b = g_value_get_boxed(gvalue as *const GValue);

                if b.is_null() {
                    return None;
                }

                Some(Value::Buffer(GstRc::new_from_unowned_ptr(b)))
            },
            typ if typ == *TYPE_GST_VALUE_ARRAY => unsafe {
                let n = gst_value_array_get_size(gvalue as *const GValue);

                let mut vec = Vec::with_capacity(n as usize);

                for i in 0..n {
                    let val = gst_value_array_get_value(gvalue as *const GValue, i);

                    if val.is_null() {
                        return None;
                    }

                    if let Some(val) = Value::from_gvalue(&*val) {
                        vec.push(val);
                    } else {
                        return None;
                    }
                }

                Some(Value::Array(vec))
            },
            _ => None,
        }
    }

    pub fn try_get<T: ValueType>(&self) -> Option<&T> {
        T::extract(self)
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

impl From<u32> for Value {
    fn from(f: u32) -> Value {
        Value::UInt(f)
    }
}

impl From<i64> for Value {
    fn from(f: i64) -> Value {
        Value::Int64(f)
    }
}

impl From<u64> for Value {
    fn from(f: u64) -> Value {
        Value::UInt64(f)
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

impl From<Rational32> for Value {
    fn from(f: Rational32) -> Value {
        Value::Fraction(f)
    }
}

impl From<(i32, i32)> for Value {
    fn from((f_n, f_d): (i32, i32)) -> Value {
        Value::Fraction(Rational32::new(f_n, f_d))
    }
}

impl From<GstRc<Buffer>> for Value {
    fn from(f: GstRc<Buffer>) -> Value {
        Value::Buffer(f)
    }
}

impl<'a> From<&'a Buffer> for Value {
    fn from(f: &'a Buffer) -> Value {
        Value::Buffer(f.into())
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

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct TypedValue<T>
    where T: ValueType
{
    value: Value,
    phantom: PhantomData<T>,
}

impl<T> TypedValue<T>
    where T: ValueType
{
    pub fn new(value: Value) -> Self {
        TypedValue {
            value: value,
            phantom: PhantomData,
        }
    }

    pub fn into_value(self) -> Value {
        self.value
    }
}

impl<T> Deref for TypedValue<T>
    where T: ValueType
{
    type Target = T;
    fn deref(&self) -> &T {
        self.value.try_get().unwrap()
    }
}

impl<T> AsRef<T> for TypedValue<T>
    where T: ValueType
{
    fn as_ref(&self) -> &T {
        self.value.try_get().unwrap()
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct TypedValueRef<'a, T>
    where T: ValueType
{
    value: &'a Value,
    phantom: PhantomData<T>,
}

impl<'a, T> TypedValueRef<'a, T>
    where T: ValueType
{
    pub fn new(value: &'a Value) -> TypedValueRef<'a, T> {
        TypedValueRef {
            value: value,
            phantom: PhantomData,
        }
    }

    pub fn value(self) -> &'a Value {
        self.value
    }
}

impl<'a, T> Deref for TypedValueRef<'a, T>
    where T: ValueType
{
    type Target = T;
    fn deref(&self) -> &T {
        self.value.try_get().unwrap()
    }
}

impl<'a, T> AsRef<T> for TypedValueRef<'a, T>
    where T: ValueType
{
    fn as_ref(&self) -> &T {
        self.value.try_get().unwrap()
    }
}
