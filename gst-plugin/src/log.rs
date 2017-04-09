// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use libc::c_char;
use std::ffi::CString;
use slog::{Drain, Record, OwnedKeyValueList, Never, Level};
use std::fmt;
use std::ptr;
use std::mem;

use utils::Element;

use gobject;
use gst;

pub struct GstDebugDrain {
    category: *mut gst::GstDebugCategory,
    element: gobject::GWeakRef,
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
                                       -> *mut gst::GstDebugCategory;
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

        let mut drain = GstDebugDrain {
            category: category,
            element: unsafe { mem::zeroed() },
        };

        if !element.is_null() {
            unsafe {
                gobject::g_weak_ref_set(&mut drain.element, element as *mut gobject::GObject);
            }
        }

        drain
    }
}

impl Drop for GstDebugDrain {
    fn drop(&mut self) {
        unsafe {
            gobject::g_weak_ref_clear(&mut self.element);
        }
    }
}

impl Drain for GstDebugDrain {
    type Error = Never;

    fn log(&self, record: &Record, _: &OwnedKeyValueList) -> Result<(), Never> {
        let level = match record.level() {
            Level::Critical | Level::Error => gst::GST_LEVEL_ERROR,
            Level::Warning => gst::GST_LEVEL_WARNING,
            Level::Info => gst::GST_LEVEL_INFO,
            Level::Debug => gst::GST_LEVEL_DEBUG,
            Level::Trace => gst::GST_LEVEL_TRACE,
        };

        let threshold = unsafe { gst::gst_debug_category_get_threshold(self.category) };

        if level as u32 > threshold as u32 {
            return Ok(());
        }

        let file_cstr = CString::new(record.file().as_bytes()).unwrap();

        // TODO: Probably want to include module?
        let function_cstr = CString::new(record.function().as_bytes()).unwrap();

        let message_cstr = CString::new(fmt::format(record.msg()).as_bytes()).unwrap();

        unsafe {
            let element = gobject::g_weak_ref_get(&self.element as *const gobject::GWeakRef as
                                                  *mut gobject::GWeakRef);

            gst::gst_debug_log(self.category,
                               level,
                               file_cstr.as_ptr(),
                               function_cstr.as_ptr(),
                               record.line() as i32,
                               element as *mut gobject::GObject,
                               message_cstr.as_ptr());

            if !element.is_null() {
                gst::gst_object_unref(element as *mut gst::GstObject);
            }
        }

        Ok(())
    }
}

unsafe impl Sync for GstDebugDrain {}
unsafe impl Send for GstDebugDrain {}
