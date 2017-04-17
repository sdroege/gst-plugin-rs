// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ffi::CStr;
use std::mem;
use std::marker::PhantomData;
use std::borrow::Cow;
use std::fmt;
use std::slice;
use libc::c_char;

pub use num_rational::Rational32;

use buffer::*;
use miniobject::*;

use glib;
use gobject;
use gst;

#[repr(C)]
pub struct Value(gobject::GValue);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ValueRef<'a> {
    Bool(bool),
    Int(i32),
    UInt(u32),
    Int64(i64),
    UInt64(u64),
    String(Cow<'a, str>),
    Fraction(Rational32),
    Buffer(GstRc<Buffer>),
    Array(Cow<'a, [Value]>),
}

impl<'a> ValueRef<'a> {
    pub fn try_get<T: ValueType<'a>>(&'a self) -> Option<T> {
        T::from_value_ref(&self)
    }
}

pub trait ValueType<'a>
    where Self: Sized
{
    fn g_type() -> glib::GType;

    fn from_value(v: &'a Value) -> Option<Self>;
    fn from_value_ref(v: &'a ValueRef<'a>) -> Option<Self>;
}

lazy_static! {
    static ref TYPE_BUFFER: glib::GType = unsafe { gst::gst_buffer_get_type() };
    static ref TYPE_FRACTION: glib::GType = unsafe { gst::gst_fraction_get_type() };
    static ref TYPE_GST_VALUE_ARRAY: glib::GType = unsafe { gst::gst_value_array_get_type() };
}

impl Value {
    pub unsafe fn as_ptr(&self) -> *const gobject::GValue {
        &self.0
    }

    pub unsafe fn from_ptr(ptr: *const gobject::GValue) -> Option<Value> {
        if ptr.is_null() || !Value::is_supported_type((*ptr).g_type) {
            return None;
        }

        let mut value = Value(mem::zeroed());
        gobject::g_value_init(&mut value.0, (*ptr).g_type);
        gobject::g_value_copy(ptr, &mut value.0);

        Some(value)
    }

    pub unsafe fn from_raw(value: gobject::GValue) -> Option<Value> {
        if !Value::is_supported_type(value.g_type) {
            return None;
        }
        Some(Value(value))
    }

    pub unsafe fn into_raw(mut self) -> gobject::GValue {
        mem::replace(&mut self.0, mem::zeroed())
    }

    fn is_supported_type(typ: glib::GType) -> bool {
        match typ {
            gobject::G_TYPE_BOOLEAN |
            gobject::G_TYPE_INT |
            gobject::G_TYPE_UINT |
            gobject::G_TYPE_INT64 |
            gobject::G_TYPE_UINT64 |
            gobject::G_TYPE_STRING => true,
            typ if typ == *TYPE_FRACTION => true,
            //typ if typ == *TYPE_BUFFER  => true
            typ if typ == *TYPE_GST_VALUE_ARRAY => true,
            _ => false,
        }
    }

    pub fn new<T: Into<Value>>(v: T) -> Value {
        v.into()
    }

    pub fn new_from_value_ref(v: ValueRef) -> Value {
        match v {
            ValueRef::Bool(v) => Value::from(v),
            ValueRef::Int(v) => Value::from(v),
            ValueRef::UInt(v) => Value::from(v),
            ValueRef::Int64(v) => Value::from(v),
            ValueRef::UInt64(v) => Value::from(v),
            ValueRef::Fraction(v) => Value::from(v),
            ValueRef::String(v) => Value::from(v),
            ValueRef::Array(v) => Value::from(v),
            ValueRef::Buffer(v) => Value::from(v),
        }
    }

    pub fn get(&self) -> ValueRef {
        match self.0.g_type {
            gobject::G_TYPE_BOOLEAN => ValueRef::Bool(bool::from_value(&self).unwrap()),
            gobject::G_TYPE_INT => ValueRef::Int(i32::from_value(&self).unwrap()),
            gobject::G_TYPE_UINT => ValueRef::UInt(u32::from_value(&self).unwrap()),
            gobject::G_TYPE_INT64 => ValueRef::Int64(i64::from_value(&self).unwrap()),
            gobject::G_TYPE_UINT64 => ValueRef::UInt64(u64::from_value(&self).unwrap()),
            typ if typ == *TYPE_FRACTION => {
                ValueRef::Fraction(Rational32::from_value(&self).unwrap())
            }
            gobject::G_TYPE_STRING => {
                ValueRef::String(Cow::Borrowed(<&str as ValueType>::from_value(&self).unwrap()))
            }
            typ if typ == *TYPE_GST_VALUE_ARRAY => {
                ValueRef::Array(Cow::Borrowed(<&[Value] as ValueType>::from_value(&self).unwrap()))
            }
            typ if typ == *TYPE_BUFFER => {
                ValueRef::Buffer(<GstRc<Buffer> as ValueType>::from_value(&self).unwrap())
            }
            _ => unreachable!(),
        }
    }

    pub fn try_get<'a, T: ValueType<'a>>(&'a self) -> Option<T> {
        T::from_value(self)
    }
}

impl Clone for Value {
    fn clone(&self) -> Self {
        unsafe {
            let mut new_value = Value(mem::zeroed());
            gobject::g_value_init(&mut new_value.0, self.0.g_type);
            gobject::g_value_copy(&self.0, &mut new_value.0);

            new_value
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        self.get().eq(&other.get())
    }
}
impl Eq for Value {}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.get().fmt(f)
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        unsafe {
            if self.0.g_type != gobject::G_TYPE_NONE {
                gobject::g_value_unset(&mut self.0);
            }
        }
    }
}

macro_rules! impl_value_type_simple(
    ($typ:ty, $variant:ident, $g_type:expr, $getter:expr, $setter:expr) => {
        impl<'a> ValueType<'a> for $typ {
            fn g_type() -> glib::GType {
                $g_type
            }

            fn from_value(value: &'a Value) -> Option<Self> {
                if value.0.g_type != Self::g_type() {
                    return None;
                }

                unsafe {
                    Some($getter(value))
                }
            }

            fn from_value_ref(value_ref: &'a ValueRef<'a>) -> Option<Self> {
                if let ValueRef::$variant(ref v) = *value_ref {
                    Some(*v)
                } else {
                    None
                }
            }
        }

        impl From<$typ> for Value {
            fn from(v: $typ) -> Value {
                unsafe {
                    let mut value = Value(mem::zeroed());

                    gobject::g_value_init(&mut value.0, <$typ as ValueType>::g_type());
                    $setter(&mut value, v);

                    value
                }
            }
        }
    };
);

impl_value_type_simple!(bool,
                        Bool,
                        gobject::G_TYPE_BOOLEAN,
                        |value: &Value| !(gobject::g_value_get_boolean(&value.0) == 0),
                        |value: &mut Value, v| {
                            gobject::g_value_set_boolean(&mut value.0,
                                                         if v { glib::GTRUE } else { glib::GFALSE })
                        });
impl_value_type_simple!(i32,
                        Int,
                        gobject::G_TYPE_INT,
                        |value: &Value| gobject::g_value_get_int(&value.0),
                        |value: &mut Value, v| gobject::g_value_set_int(&mut value.0, v));
impl_value_type_simple!(u32,
                        UInt,
                        gobject::G_TYPE_UINT,
                        |value: &Value| gobject::g_value_get_uint(&value.0),
                        |value: &mut Value, v| gobject::g_value_set_uint(&mut value.0, v));
impl_value_type_simple!(i64,
                        Int64,
                        gobject::G_TYPE_INT64,
                        |value: &Value| gobject::g_value_get_int64(&value.0),
                        |value: &mut Value, v| gobject::g_value_set_int64(&mut value.0, v));
impl_value_type_simple!(u64,
                        UInt64,
                        gobject::G_TYPE_UINT64,
                        |value: &Value| gobject::g_value_get_uint64(&value.0),
                        |value: &mut Value, v| gobject::g_value_set_uint64(&mut value.0, v));
impl_value_type_simple!(Rational32,
                        Fraction,
                        *TYPE_FRACTION,
                        |value: &Value| {
                            Rational32::new(gst::gst_value_get_fraction_numerator(&value.0),
                                            gst::gst_value_get_fraction_denominator(&value.0))
                        },
                        |value: &mut Value, v: Rational32| {
                            gst::gst_value_set_fraction(&mut value.0, *v.numer(), *v.denom())
                        });

impl<'a> ValueType<'a> for &'a str {
    fn g_type() -> glib::GType {
        gobject::G_TYPE_STRING
    }

    fn from_value(value: &'a Value) -> Option<Self> {
        if value.0.g_type != Self::g_type() {
            return None;
        }

        unsafe {
            let s = gobject::g_value_get_string(&value.0);
            if s.is_null() {
                return Some(&"");
            }

            let cstr = CStr::from_ptr(s).to_str().expect("Invalid string");
            Some(cstr)
        }
    }

    fn from_value_ref(value_ref: &'a ValueRef<'a>) -> Option<Self> {
        if let ValueRef::String(ref v) = *value_ref {
            Some(v.as_ref())
        } else {
            None
        }
    }
}

impl<'a> From<Cow<'a, str>> for Value {
    fn from(v: Cow<'a, str>) -> Value {
        unsafe {
            let mut value = Value(mem::zeroed());

            gobject::g_value_init(&mut value.0, <&str as ValueType>::g_type());
            let v_cstr = glib::g_strndup(v.as_ptr() as *const c_char, v.len());
            gobject::g_value_take_string(&mut value.0, v_cstr);

            value
        }
    }
}

impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::from(Cow::Owned::<str>(v))
    }
}

impl<'a> From<&'a str> for Value {
    fn from(v: &'a str) -> Value {
        Value::from(Cow::Borrowed::<str>(v))
    }
}

impl<'a> ValueType<'a> for GstRc<Buffer> {
    fn g_type() -> glib::GType {
        *TYPE_BUFFER
    }

    fn from_value(value: &'a Value) -> Option<Self> {
        if value.0.g_type != Self::g_type() {
            return None;
        }

        unsafe {
            let buffer = gobject::g_value_get_boxed(&value.0) as *mut gst::GstBuffer;
            Some(GstRc::new_from_unowned_ptr(buffer))
        }
    }

    fn from_value_ref(value_ref: &'a ValueRef<'a>) -> Option<Self> {
        if let ValueRef::Buffer(ref v) = *value_ref {
            Some(v.clone())
        } else {
            None
        }
    }
}

impl From<GstRc<Buffer>> for Value {
    fn from(v: GstRc<Buffer>) -> Value {
        Value::from(v.as_ref())
    }
}

impl<'a> From<&'a GstRc<Buffer>> for Value {
    fn from(v: &'a GstRc<Buffer>) -> Value {
        Value::from(v.as_ref())
    }
}

impl<'a> From<&'a Buffer> for Value {
    fn from(v: &'a Buffer) -> Value {
        unsafe {
            let mut value = Value(mem::zeroed());

            gobject::g_value_init(&mut value.0, <GstRc<Buffer> as ValueType>::g_type());
            gobject::g_value_set_boxed(&mut value.0, v.as_ptr() as glib::gpointer);

            value
        }
    }
}

impl<'a> ValueType<'a> for &'a [Value] {
    fn g_type() -> glib::GType {
        *TYPE_GST_VALUE_ARRAY
    }

    fn from_value(value: &'a Value) -> Option<Self> {
        if value.0.g_type != Self::g_type() {
            return None;
        }

        unsafe {
            let arr = value.0.data[0] as *const glib::GArray;

            if arr.is_null() {
                Some(&[])
            } else {
                let arr = &*arr;
                Some(slice::from_raw_parts(arr.data as *const Value, arr.len as usize))
            }
        }
    }

    fn from_value_ref(value_ref: &'a ValueRef<'a>) -> Option<Self> {
        if let ValueRef::Array(ref v) = *value_ref {
            Some(v.as_ref())
        } else {
            None
        }
    }
}

impl<'a> From<Cow<'a, [Value]>> for Value {
    fn from(v: Cow<'a, [Value]>) -> Value {
        unsafe {
            let mut value = Value(mem::zeroed());

            gobject::g_value_init(&mut value.0, <&[Value] as ValueType>::g_type());

            match v {
                Cow::Borrowed(ref array) => {
                    for e in *array {
                        gst::gst_value_array_append_value(&mut value.0,
                                                          e.as_ptr() as *mut gobject::GValue);
                    }
                }
                Cow::Owned(array) => {
                    for mut e in array {
                        gst::gst_value_array_append_and_take_value(&mut value.0,
                                                                   e.as_ptr() as
                                                                   *mut gobject::GValue);
                        e.0.g_type = gobject::G_TYPE_NONE;
                    }
                }
            }

            value
        }
    }
}

impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Value {
        Value::from(Cow::Owned::<[Value]>(v))
    }
}

impl<'a> From<&'a Vec<Value>> for Value {
    fn from(v: &'a Vec<Value>) -> Value {
        Value::from(Cow::Borrowed::<[Value]>(v.as_ref()))
    }
}

impl<'a> From<&'a [Value]> for Value {
    fn from(v: &'a [Value]) -> Value {
        Value::from(Cow::Borrowed::<[Value]>(v))
    }
}

impl<'a> From<ValueRef<'a>> for Value {
    fn from(value_ref: ValueRef<'a>) -> Value {
        Value::new_from_value_ref(value_ref)
    }
}

impl<'a> From<&'a ValueRef<'a>> for Value {
    fn from(value_ref: &'a ValueRef<'a>) -> Value {
        Value::new_from_value_ref(value_ref.clone())
    }
}

impl From<(i32, i32)> for Value {
    fn from((f_n, f_d): (i32, i32)) -> Value {
        Value::from(Rational32::new(f_n, f_d))
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TypedValue<T> {
    value: Value,
    phantom: PhantomData<T>,
}

impl<'a, T> TypedValue<T>
    where T: ValueType<'a>
{
    pub fn new<VT: Into<TypedValue<T>>>(v: VT) -> TypedValue<T> {
        v.into()
    }

    pub fn from_value(value: Value) -> Option<TypedValue<T>> {
        if value.0.g_type != T::g_type() {
            return None;
        }

        Some(TypedValue {
                 value: value,
                 phantom: PhantomData,
             })
    }

    pub fn get(&'a self) -> T {
        self.value.try_get::<T>().unwrap()
    }

    pub fn into_value(self) -> Value {
        self.value
    }

    pub unsafe fn as_ptr(&self) -> *const gobject::GValue {
        &self.value.0
    }

    pub unsafe fn from_ptr(ptr: *const gobject::GValue) -> Option<TypedValue<T>> {
        if let Some(value) = Value::from_ptr(ptr) {
            return TypedValue::from_value(value);
        }
        None
    }

    pub unsafe fn from_raw(value: gobject::GValue) -> Option<TypedValue<T>> {
        if let Some(value) = Value::from_raw(value) {
            return TypedValue::from_value(value);
        }
        None
    }

    pub unsafe fn into_raw(mut self) -> gobject::GValue {
        mem::replace(&mut self.value.0, mem::zeroed())
    }
}

impl<'a, T> From<T> for TypedValue<T>
    where T: ValueType<'a> + Into<Value>
{
    fn from(v: T) -> Self {
        TypedValue::from_value(Value::new(v)).unwrap()
    }
}

impl<'a> From<Cow<'a, str>> for TypedValue<&'a str> {
    fn from(v: Cow<'a, str>) -> Self {
        TypedValue::from_value(Value::new(v)).unwrap()
    }
}

impl<'a> From<String> for TypedValue<&'a str> {
    fn from(v: String) -> Self {
        TypedValue::from_value(Value::new(v)).unwrap()
    }
}

impl<'a> From<Vec<Value>> for TypedValue<&'a [Value]> {
    fn from(v: Vec<Value>) -> Self {
        TypedValue::from_value(Value::new(v)).unwrap()
    }
}

impl<'a> From<&'a Vec<Value>> for TypedValue<&'a [Value]> {
    fn from(v: &'a Vec<Value>) -> Self {
        TypedValue::from_value(Value::new(v)).unwrap()
    }
}

impl<'a> From<Cow<'a, [Value]>> for TypedValue<&'a [Value]> {
    fn from(v: Cow<'a, [Value]>) -> Self {
        TypedValue::from_value(Value::new(v)).unwrap()
    }
}

impl<'a> From<&'a GstRc<Buffer>> for TypedValue<GstRc<Buffer>> {
    fn from(v: &'a GstRc<Buffer>) -> Self {
        TypedValue::from_value(Value::new(v)).unwrap()
    }
}

impl<'a> From<&'a Buffer> for TypedValue<GstRc<Buffer>> {
    fn from(v: &'a Buffer) -> Self {
        TypedValue::from_value(Value::new(v)).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    macro_rules! gen_test_value(
        ($name: ident, $typ:ty, $value:expr, $variant:ident) => {
            #[test]
            fn $name() {
                unsafe { gst::gst_init(ptr::null_mut(), ptr::null_mut()) };

                let value = Value::new($value);
                if let ValueRef::$variant(v) = value.get() {
                    assert_eq!(v, $value);
                } else {
                    unreachable!();
                }

                if let Some(v) = value.get().try_get::<$typ>() {
                    assert_eq!(v, $value);
                } else {
                    unreachable!();
                }

                let value2 = value.clone();
                if let ValueRef::$variant(v) = value2.get() {
                    assert_eq!(v, $value);
                } else {
                    unreachable!();
                }

                let value2 = Value::new_from_value_ref(value.get());
                assert_eq!(value2, value);

                let value3 = TypedValue::new($value);
                assert_eq!(value3.get(), $value);

                if let Some(value3) = TypedValue::<$typ>::from_value(value) {
                    assert_eq!(value3.get(), $value);
                } else {
                    unreachable!();
                }
            }
        };
    );

    gen_test_value!(int, i32, 12i32, Int);
    gen_test_value!(uint, u32, 12u32, UInt);
    gen_test_value!(int64, i64, 12i64, Int64);
    gen_test_value!(uint64, u64, 12u64, UInt64);
    gen_test_value!(boolean, bool, true, Bool);
    gen_test_value!(fraction, Rational32, Rational32::new(1, 2), Fraction);

    #[test]
    fn string_owned() {
        unsafe { gst::gst_init(ptr::null_mut(), ptr::null_mut()) };

        let orig_v = String::from("foo");

        let value = Value::new(orig_v.clone());
        if let ValueRef::String(v) = value.get() {
            assert_eq!(v, orig_v);
        } else {
            unreachable!();
        }

        if let Some(v) = value.get().try_get::<&str>() {
            assert_eq!(v, orig_v);
        } else {
            unreachable!();
        }

        let value2 = value.clone();
        if let ValueRef::String(v) = value2.get() {
            assert_eq!(v, orig_v);
        } else {
            unreachable!();
        }

        let value2 = Value::new_from_value_ref(value.get());
        assert_eq!(value2, value);


        let value2 = Value::new_from_value_ref(value.get());
        assert_eq!(value2, value);

        let value3 = TypedValue::new(orig_v.clone());
        assert_eq!(value3.get(), orig_v.as_str());

        if let Some(value3) = TypedValue::<&str>::from_value(value) {
            assert_eq!(value3.get(), orig_v.as_str());
        } else {
            unreachable!();
        }
    }

    #[test]
    fn string_borrowed() {
        unsafe { gst::gst_init(ptr::null_mut(), ptr::null_mut()) };

        let orig_v = "foo";

        let value = Value::new(orig_v);
        if let ValueRef::String(v) = value.get() {
            assert_eq!(v, orig_v);
        } else {
            unreachable!();
        }

        if let Some(v) = value.get().try_get::<&str>() {
            assert_eq!(v, orig_v);
        } else {
            unreachable!();
        }

        let value2 = value.clone();
        if let ValueRef::String(v) = value2.get() {
            assert_eq!(v, orig_v);
        } else {
            unreachable!();
        }

        let value2 = Value::new_from_value_ref(value.get());
        assert_eq!(value2, value);

        let value3 = TypedValue::new(orig_v);
        assert_eq!(value3.get(), orig_v);

        if let Some(value3) = TypedValue::<&str>::from_value(value) {
            assert_eq!(value3.get(), orig_v);
        } else {
            unreachable!();
        }
    }

    #[test]
    fn array_owned() {
        unsafe { gst::gst_init(ptr::null_mut(), ptr::null_mut()) };

        let orig_v = vec![Value::new("a"), Value::new("b")];

        let value = Value::new(orig_v.clone());
        if let ValueRef::Array(arr) = value.get() {
            assert_eq!(arr, orig_v.as_slice());
        } else {
            unreachable!();
        }

        if let Some(v) = value.get().try_get::<&[Value]>() {
            assert_eq!(v, orig_v.as_slice());
        } else {
            unreachable!();
        }

        let value2 = Value::new_from_value_ref(value.get());
        assert_eq!(value2, value);

        let value2 = Value::new_from_value_ref(value.get());
        assert_eq!(value2, value);

        let value3 = TypedValue::new(orig_v.clone());
        assert_eq!(value3.get(), orig_v.as_slice());

        if let Some(value3) = TypedValue::<&[Value]>::from_value(value) {
            assert_eq!(value3.get(), orig_v.as_slice());
        } else {
            unreachable!();
        }
    }

    #[test]
    fn array_borrowed() {
        unsafe { gst::gst_init(ptr::null_mut(), ptr::null_mut()) };

        let orig_v = vec![Value::new("a"), Value::new("b")];

        let value = Value::new(&orig_v);
        if let ValueRef::Array(arr) = value.get() {
            assert_eq!(arr, orig_v.as_slice());
        } else {
            unreachable!();
        }

        if let Some(arr) = value.get().try_get::<&[Value]>() {
            assert_eq!(arr, orig_v.as_slice());
        } else {
            unreachable!();
        }

        let value2 = Value::new_from_value_ref(value.get());
        assert_eq!(value2, value);

        let value3 = TypedValue::new(orig_v.as_slice());
        assert_eq!(value3.get(), orig_v.as_slice());

        if let Some(value3) = TypedValue::<&[Value]>::from_value(value) {
            assert_eq!(value3.get(), orig_v.as_slice());
        } else {
            unreachable!();
        }
    }

    #[test]
    fn buffer() {
        unsafe { gst::gst_init(ptr::null_mut(), ptr::null_mut()) };

        let orig_v = Buffer::new_from_vec(vec![1, 2, 3, 4]).unwrap();

        let value = Value::new(orig_v.clone());
        if let ValueRef::Buffer(buf) = value.get() {
            assert_eq!(buf, orig_v);
        } else {
            unreachable!();
        }

        if let Some(buf) = value.get().try_get::<GstRc<Buffer>>() {
            assert_eq!(buf, orig_v);
        } else {
            unreachable!();
        }

        let value2 = Value::new_from_value_ref(value.get());
        assert_eq!(value2, value);

        let value3 = TypedValue::new(&orig_v);
        assert_eq!(value3.get(), orig_v);

        if let Some(value3) = TypedValue::<GstRc<Buffer>>::from_value(value) {
            assert_eq!(value3.get(), orig_v);
        } else {
            unreachable!();
        }
    }
}
