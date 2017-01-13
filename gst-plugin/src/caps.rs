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
use std::fmt;

use value::*;

pub struct Caps(*mut c_void);

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
    use value::*;
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
