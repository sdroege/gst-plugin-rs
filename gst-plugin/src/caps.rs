// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use libc::c_char;
use std::os::raw::c_void;
use std::ffi::CString;
use std::ffi::CStr;
use std::fmt;
use value::*;
use utils::*;
use miniobject::*;

#[derive(Eq)]
pub struct Caps(*mut c_void);

unsafe impl MiniObject for Caps {
    unsafe fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    unsafe fn replace_ptr(&mut self, ptr: *mut c_void) {
        self.0 = ptr
    }

    unsafe fn new_from_ptr(ptr: *mut c_void) -> Self {
        Caps(ptr)
    }
}

impl Caps {
    pub fn new_empty() -> GstRc<Self> {
        extern "C" {
            fn gst_caps_new_empty() -> *mut c_void;
        }

        unsafe { GstRc::new_from_owned_ptr(gst_caps_new_empty()) }
    }

    pub fn new_any() -> GstRc<Self> {
        extern "C" {
            fn gst_caps_new_any() -> *mut c_void;
        }

        unsafe { GstRc::new_from_owned_ptr(gst_caps_new_any()) }
    }

    pub fn new_simple(name: &str, values: &[(&str, &Value)]) -> GstRc<Self> {
        extern "C" {
            fn gst_caps_append_structure(caps: *mut c_void, structure: *mut c_void);
            fn gst_structure_new_empty(name: *const c_char) -> *mut c_void;
        }

        let mut caps = Caps::new_empty();

        let name_cstr = CString::new(name).unwrap();
        let structure = unsafe { gst_structure_new_empty(name_cstr.as_ptr()) };

        unsafe {
            gst_caps_append_structure(caps.as_ptr(), structure);
        }

        caps.get_mut().unwrap().set_simple(values);

        caps
    }

    pub fn from_string(value: &str) -> Option<GstRc<Self>> {
        extern "C" {
            fn gst_caps_from_string(value: *const c_char) -> *mut c_void;
        }

        let value_cstr = CString::new(value).unwrap();

        unsafe {
            let caps_ptr = gst_caps_from_string(value_cstr.as_ptr());

            if caps_ptr.is_null() {
                None
            } else {
                Some(GstRc::new_from_owned_ptr(caps_ptr))
            }
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
            fn g_free(ptr: *mut c_char);
        }

        unsafe {
            let ptr = gst_caps_to_string(self.0);
            let s = CStr::from_ptr(ptr).to_str().unwrap().into();
            g_free(ptr);

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
        extern "C" {
            fn gst_caps_is_equal(a: *const c_void, b: *const c_void) -> GBoolean;
        }

        unsafe { gst_caps_is_equal(self.0, other.0).to_bool() }
    }
}

unsafe impl Sync for Caps {}
unsafe impl Send for Caps {}

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
