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

use std::os::raw::c_void;
use libc::c_char;
use std::ffi::CString;
use slog::{Drain, Record, OwnedKeyValueList, Never, Level};
use std::fmt;
use std::ptr;

use utils::Element;

#[derive(Debug)]
pub struct GstDebugDrain {
    category: *const c_void,
    element: *const c_void,
}

impl GstDebugDrain {
    pub fn new(element: Option<&Element>,
               name: &str,
               color: u32,
               description: &str)
               -> GstDebugDrain {
        extern "C" {
            fn _gst_debug_category_new(name: *const c_char,
                                       color: u32,
                                       description: *const c_char)
                                       -> *const c_void;
        }

        let name_cstr = CString::new(name.as_bytes()).unwrap();
        let description_cstr = CString::new(description.as_bytes()).unwrap();

        // Gets the category if it exists already
        let category = unsafe {
            _gst_debug_category_new(name_cstr.as_ptr(), color, description_cstr.as_ptr())
        };

        let element = match element {
            Some(element) => unsafe { element.as_ptr() },
            None => ptr::null(),
        };

        let drain = GstDebugDrain {
            category: category,
            element: ptr::null(),
        };

        extern "C" {
            fn g_weak_ref_set(weak_ref: &*const c_void, obj: *const c_void);
        }

        if !element.is_null() {
            unsafe {
                g_weak_ref_set(&drain.element, element);
            }
        }

        drain
    }
}

impl Drop for GstDebugDrain {
    fn drop(&mut self) {
        extern "C" {
            fn g_weak_ref_clear(weak_ref: &*const c_void);
        }

        if !self.element.is_null() {
            unsafe {
                g_weak_ref_clear(&self.element);
            }
        }
    }
}

impl Drain for GstDebugDrain {
    type Error = Never;

    fn log(&self, record: &Record, _: &OwnedKeyValueList) -> Result<(), Never> {
        extern "C" {
            fn gst_rs_debug_log(category: *const c_void,
                                level: u32,
                                file: *const c_char,
                                function: *const c_char,
                                line: u32,
                                object: *const c_void,
                                message: *const c_char);
            fn gst_debug_category_get_threshold(category: *const c_void) -> u32;
            fn g_weak_ref_get(weak_ref: &*const c_void) -> *const c_void;
            fn gst_object_unref(obj: *const c_void);
        }

        let level = match record.level() {
            Level::Critical | Level::Error => 1,
            Level::Warning => 2,
            Level::Info => 4,
            Level::Debug => 5,
            Level::Trace => 7,
        };

        let threshold = unsafe { gst_debug_category_get_threshold(self.category) };

        if level > threshold {
            return Ok(());
        }

        let file_cstr = CString::new(record.file().as_bytes()).unwrap();

        // TODO: Probably want to include module?
        let function_cstr = CString::new(record.function().as_bytes()).unwrap();

        let message_cstr = CString::new(fmt::format(record.msg()).as_bytes()).unwrap();

        unsafe {
            let element = if self.element.is_null() {
                ptr::null()
            } else {
                g_weak_ref_get(&self.element)
            };

            gst_rs_debug_log(self.category,
                             level,
                             file_cstr.as_ptr(),
                             function_cstr.as_ptr(),
                             record.line(),
                             element,
                             message_cstr.as_ptr());

            if !element.is_null() {
                gst_object_unref(element);
            }
        }

        Ok(())
    }
}

unsafe impl Sync for GstDebugDrain {}
unsafe impl Send for GstDebugDrain {}
