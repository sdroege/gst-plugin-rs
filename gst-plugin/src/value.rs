// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ffi::{CString, CStr};
use std::mem;
use std::marker::PhantomData;
use std::ops::Deref;

pub use num_rational::Rational32;

use buffer::*;
use miniobject::*;

use glib;
use gobject;
use gst;

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

lazy_static! {
    static ref TYPE_BUFFER: glib::GType = unsafe { gst::gst_buffer_get_type() };
    static ref TYPE_FRACTION: glib::GType = unsafe { gst::gst_fraction_get_type() };
    static ref TYPE_GST_VALUE_ARRAY: glib::GType = unsafe { gst::gst_value_array_get_type() };
}

impl Value {
    pub unsafe fn to_gvalue(&self) -> gobject::GValue {
        let mut gvalue = mem::zeroed();

        match *self {
            Value::Bool(v) => {
                gobject::g_value_init(&mut gvalue, gobject::G_TYPE_BOOLEAN);
                gobject::g_value_set_boolean(&mut gvalue,
                                             if v { glib::GTRUE } else { glib::GFALSE });
            }
            Value::Int(v) => {
                gobject::g_value_init(&mut gvalue, gobject::G_TYPE_INT);
                gobject::g_value_set_int(&mut gvalue, v);
            }
            Value::UInt(v) => {
                gobject::g_value_init(&mut gvalue, gobject::G_TYPE_UINT);
                gobject::g_value_set_uint(&mut gvalue, v);
            }
            Value::Int64(v) => {
                gobject::g_value_init(&mut gvalue, gobject::G_TYPE_INT64);
                gobject::g_value_set_int64(&mut gvalue, v);
            }
            Value::UInt64(v) => {
                gobject::g_value_init(&mut gvalue, gobject::G_TYPE_UINT64);
                gobject::g_value_set_uint64(&mut gvalue, v);
            }
            Value::String(ref v) => {
                let v_cstr = CString::new(String::from(v.clone())).unwrap();

                gobject::g_value_init(&mut gvalue, gobject::G_TYPE_STRING);
                gobject::g_value_set_string(&mut gvalue, v_cstr.as_ptr());
            }
            Value::Fraction(ref v) => {
                gobject::g_value_init(&mut gvalue, *TYPE_FRACTION);
                gst::gst_value_set_fraction(&mut gvalue, *v.numer(), *v.denom());
            }
            Value::Buffer(ref buffer) => {
                gobject::g_value_init(&mut gvalue, *TYPE_BUFFER);
                gobject::g_value_set_boxed(&mut gvalue, buffer.as_ptr() as glib::gconstpointer);
            }
            Value::Array(ref array) => {
                gobject::g_value_init(&mut gvalue, *TYPE_GST_VALUE_ARRAY);

                for e in array {
                    let mut e_value = e.to_gvalue();
                    gst::gst_value_array_append_and_take_value(&mut gvalue, &mut e_value);
                }
            }
        }

        gvalue
    }

    pub unsafe fn from_gvalue(gvalue: &gobject::GValue) -> Option<Self> {
        match gvalue.g_type {
            gobject::G_TYPE_BOOLEAN => {
                Some(Value::Bool(!(gobject::g_value_get_boolean(gvalue) == 0)))
            }
            gobject::G_TYPE_INT => Some(Value::Int(gobject::g_value_get_int(gvalue))),
            gobject::G_TYPE_UINT => Some(Value::UInt(gobject::g_value_get_uint(gvalue))),
            gobject::G_TYPE_INT64 => Some(Value::Int64(gobject::g_value_get_int64(gvalue))),
            gobject::G_TYPE_UINT64 => Some(Value::UInt64(gobject::g_value_get_uint64(gvalue))),
            gobject::G_TYPE_STRING => {
                let s = gobject::g_value_get_string(gvalue);
                if s.is_null() {
                    return None;
                }

                let cstr = CStr::from_ptr(s);
                match cstr.to_str() {
                    Err(_) => None,
                    Ok(s) => Some(Value::String(s.into())),
                }
            }
            typ if typ == *TYPE_FRACTION => {
                let n = gst::gst_value_get_fraction_numerator(gvalue);
                let d = gst::gst_value_get_fraction_denominator(gvalue);

                Some(Value::Fraction(Rational32::new(n, d)))
            }
            typ if typ == *TYPE_BUFFER => {
                let b = gobject::g_value_get_boxed(gvalue);

                if b.is_null() {
                    return None;
                }

                Some(Value::Buffer(GstRc::new_from_unowned_ptr(b as *mut gst::GstBuffer)))
            }
            typ if typ == *TYPE_GST_VALUE_ARRAY => {
                let n = gst::gst_value_array_get_size(gvalue);

                let mut vec = Vec::with_capacity(n as usize);

                for i in 0..n {
                    let val = gst::gst_value_array_get_value(gvalue, i);

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
            }
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
