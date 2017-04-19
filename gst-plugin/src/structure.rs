// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::fmt;
use std::ptr;
use std::ffi::{CStr, CString};

use value::*;

use glib;
use gobject;
use gst;

#[repr(C)]
pub struct Structure(*mut gst::GstStructure);

impl Structure {
    pub fn new(name: &str) -> Structure {
        let name_cstr = CString::new(name).unwrap();
        Structure(unsafe { gst::gst_structure_new_empty(name_cstr.as_ptr()) })
    }

    pub fn from_string(s: &str) -> Option<Structure> {
        unsafe {
            let cstr = CString::new(s).unwrap();
            let structure = gst::gst_structure_from_string(cstr.as_ptr(), ptr::null_mut());
            if structure.is_null() {
                None
            } else {
                Some(Structure(structure))
            }
        }
    }

    pub fn to_string(&self) -> String {
        unsafe {
            let ptr = gst::gst_structure_to_string(self.0);
            let s = CStr::from_ptr(ptr).to_str().unwrap().into();
            glib::g_free(ptr as glib::gpointer);

            s
        }
    }

    pub fn get<'a, T: ValueType<'a>>(&'a self, name: &str) -> Option<TypedValueRef<'a, T>> {
        match self.get_value(name) {
            Some(value) => TypedValueRef::from_value_ref(value),
            None => None,
        }
    }

    pub fn get_value<'a>(&'a self, name: &str) -> Option<ValueRef<'a>> {
        unsafe {
            let name_cstr = CString::new(name).unwrap();

            let value = gst::gst_structure_get_value(self.0, name_cstr.as_ptr());

            if value.is_null() {
                return None;
            }

            ValueRef::from_ptr(value)
        }
    }

    pub fn set<T: Into<Value>>(&mut self, name: &str, value: T) {
        unsafe {
            let name_cstr = CString::new(name).unwrap();
            let mut gvalue = value.into().into_raw();

            gst::gst_structure_take_value(self.0, name_cstr.as_ptr(), &mut gvalue);
            gvalue.g_type = gobject::G_TYPE_NONE;
        }
    }

    pub fn get_name<'a>(&'a self) -> &'a str {
        unsafe {
            let cstr = CStr::from_ptr(gst::gst_structure_get_name(self.0));
            cstr.to_str().unwrap()
        }
    }

    pub fn has_field(&self, field: &str) -> bool {
        unsafe {
            let cstr = CString::new(field).unwrap();
            if gst::gst_structure_has_field(self.0, cstr.as_ptr()) == glib::GTRUE {
                true
            } else {
                false
            }
        }
    }

    pub fn remove_field(&mut self, field: &str) {
        unsafe {
            let cstr = CString::new(field).unwrap();
            gst::gst_structure_remove_field(self.0, cstr.as_ptr());
        }
    }

    pub fn remove_all_fields(&mut self) {
        unsafe {
            gst::gst_structure_remove_all_fields(self.0);
        }
    }

    pub fn fields<'a>(&'a self) -> FieldIterator<'a> {
        FieldIterator::new(self)
    }

    pub fn iter<'a>(&'a self) -> Iter<'a> {
        Iter::new(self)
    }
}

impl Clone for Structure {
    fn clone(&self) -> Self {
        Structure(unsafe { gst::gst_structure_copy(self.0) })
    }
}

impl Drop for Structure {
    fn drop(&mut self) {
        unsafe { gst::gst_structure_free(self.0) }
    }
}

impl fmt::Debug for Structure {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.to_string())
    }
}

impl PartialEq for Structure {
    fn eq(&self, other: &Structure) -> bool {
        (unsafe { gst::gst_structure_is_equal(self.0, other.0) } == glib::GTRUE)
    }
}

impl Eq for Structure {}

pub struct FieldIterator<'a> {
    structure: &'a Structure,
    idx: u32,
    n_fields: u32,
}

impl<'a> FieldIterator<'a> {
    pub fn new(structure: &'a Structure) -> FieldIterator<'a> {
        let n_fields = unsafe { gst::gst_structure_n_fields(structure.0) } as u32;

        FieldIterator {
            structure: structure,
            idx: 0,
            n_fields: n_fields,
        }
    }
}

impl<'a> Iterator for FieldIterator<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.idx >= self.n_fields {
            return None;
        }

        unsafe {
            let field_name = gst::gst_structure_nth_field_name(self.structure.0, self.idx);
            if field_name.is_null() {
                return None;
            }
            self.idx += 1;

            let cstr = CStr::from_ptr(field_name);
            Some(cstr.to_str().unwrap())
        }
    }
}

pub struct Iter<'a> {
    structure: &'a Structure,
    idx: u32,
    n_fields: u32,
}

impl<'a> Iter<'a> {
    pub fn new(structure: &'a Structure) -> Iter<'a> {
        let n_fields = unsafe { gst::gst_structure_n_fields(structure.0) } as u32;

        Iter {
            structure: structure,
            idx: 0,
            n_fields: n_fields,
        }
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a str, ValueRef<'a>);

    fn next(&mut self) -> Option<(&'a str, ValueRef<'a>)> {
        if self.idx >= self.n_fields {
            return None;
        }

        unsafe {
            let field_name = gst::gst_structure_nth_field_name(self.structure.0, self.idx);
            if field_name.is_null() {
                return None;
            }
            self.idx += 1;

            let cstr = CStr::from_ptr(field_name);
            let f = cstr.to_str().unwrap();
            let v = self.structure.get_value(f);

            Some((f, v.unwrap()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    #[test]
    fn set_get() {
        unsafe { gst::gst_init(ptr::null_mut(), ptr::null_mut()) };

        let mut s = Structure::new("test");
        assert_eq!(s.get_name(), "test");

        s.set("f1", "abc");
        s.set("f2", String::from("bcd"));
        s.set("f3", 123i32);

        assert_eq!(s.get::<&str>("f1").unwrap().get(), "abc");
        assert_eq!(s.get::<&str>("f2").unwrap().get(), "bcd");
        assert_eq!(s.get::<i32>("f3").unwrap().get(), 123i32);
        assert_eq!(s.fields().collect::<Vec<_>>(), vec!["f1", "f2", "f3"]);
        assert_eq!(s.iter()
                       .map(|(f, v)| (f, Value::from_value_ref(&v)))
                       .collect::<Vec<_>>(),
                   vec![("f1", Value::new("abc")),
                        ("f2", Value::new("bcd")),
                        ("f3", Value::new(123i32))]);
    }
}
