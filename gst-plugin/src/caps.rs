// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ffi::CString;
use std::ffi::CStr;
use std::fmt;
use value::*;
use miniobject::*;

use glib;
use gobject;
use gst;

#[derive(Eq)]
pub struct Caps(*mut gst::GstCaps);

unsafe impl MiniObject for Caps {
    type PtrType = gst::GstCaps;

    unsafe fn as_ptr(&self) -> *mut gst::GstCaps {
        self.0
    }

    unsafe fn replace_ptr(&mut self, ptr: *mut gst::GstCaps) {
        self.0 = ptr
    }

    unsafe fn new_from_ptr(ptr: *mut gst::GstCaps) -> Self {
        Caps(ptr)
    }
}

impl Caps {
    pub fn new_empty() -> GstRc<Self> {
        unsafe { GstRc::new_from_owned_ptr(gst::gst_caps_new_empty()) }
    }

    pub fn new_any() -> GstRc<Self> {
        unsafe { GstRc::new_from_owned_ptr(gst::gst_caps_new_any()) }
    }

    pub fn new_simple(name: &str, values: &[(&str, &Value)]) -> GstRc<Self> {
        let mut caps = Caps::new_empty();

        let name_cstr = CString::new(name).unwrap();
        let structure = unsafe { gst::gst_structure_new_empty(name_cstr.as_ptr()) };

        unsafe {
            gst::gst_caps_append_structure((*caps).0, structure);
        }

        caps.get_mut().unwrap().set_simple(values);

        caps
    }

    pub fn from_string(value: &str) -> Option<GstRc<Self>> {
        let value_cstr = CString::new(value).unwrap();

        unsafe {
            let caps_ptr = gst::gst_caps_from_string(value_cstr.as_ptr());

            if caps_ptr.is_null() {
                None
            } else {
                Some(GstRc::new_from_owned_ptr(caps_ptr))
            }
        }
    }

    pub fn set_simple(&mut self, values: &[(&str, &Value)]) {
        for value in values {
            let name_cstr = CString::new(value.0).unwrap();
            unsafe {
                let mut gvalue = value.1.to_gvalue();
                gst::gst_caps_set_value(self.0, name_cstr.as_ptr(), &gvalue);
                gobject::g_value_unset(&mut gvalue);
            }
        }
    }

    pub fn to_string(&self) -> String {
        unsafe {
            let ptr = gst::gst_caps_to_string(self.0);
            let s = CStr::from_ptr(ptr).to_str().unwrap().into();
            glib::g_free(ptr as glib::gpointer);

            s
        }
    }
}

impl fmt::Debug for Caps {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.to_string())
    }
}

impl PartialEq for Caps {
    fn eq(&self, other: &Caps) -> bool {
        (unsafe { gst::gst_caps_is_equal(self.0, other.0) } == glib::GTRUE)
    }
}

unsafe impl Sync for Caps {}
unsafe impl Send for Caps {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    fn init() {
        unsafe {
            gst::gst_init(ptr::null_mut(), ptr::null_mut());
        }
    }

    #[test]
    fn test_simple() {
        init();

        let caps = Caps::new_simple("foo/bar",
                                    &[("int", &12.into()),
                                      ("bool", &true.into()),
                                      ("string", &"bla".into()),
                                      ("fraction", &(1, 2).into()),
                                      ("array", &vec![1.into(), 2.into()].into())]);
        assert_eq!(caps.to_string(),
                   "foo/bar, int=(int)12, bool=(boolean)true, string=(string)bla, \
                    fraction=(fraction)1/2, array=(int)< 1, 2 >");
    }
}
