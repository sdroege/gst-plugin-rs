// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ptr;

use glib_ffi;
use gobject_ffi;

#[macro_export]
macro_rules! callback_guard {
    () => {
        let _guard = ::glib::CallbackGuard::new();
    };
}

#[macro_export]
macro_rules! floating_reference_guard {
    ($obj:ident) => {
        let _guard = $crate::FloatingReferenceGuard::new($obj as *mut _);
    };
}

pub struct FloatingReferenceGuard(ptr::NonNull<gobject_ffi::GObject>);

impl FloatingReferenceGuard {
    pub unsafe fn new(obj: *mut gobject_ffi::GObject) -> Option<FloatingReferenceGuard> {
        assert!(!obj.is_null());
        if gobject_ffi::g_object_is_floating(obj) != glib_ffi::GFALSE {
            gobject_ffi::g_object_ref_sink(obj);
            Some(FloatingReferenceGuard(ptr::NonNull::new_unchecked(obj)))
        } else {
            None
        }
    }
}

impl Drop for FloatingReferenceGuard {
    fn drop(&mut self) {
        unsafe {
            gobject_ffi::g_object_force_floating(self.0.as_ptr());
        }
    }
}
