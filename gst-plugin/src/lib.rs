// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//               2016 Luis de Bethencourt <luisbg@osg.samsung.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

extern crate byteorder;
extern crate gstreamer_base_sys as gst_base_ffi;
#[macro_use]
extern crate lazy_static;
extern crate libc;
pub extern crate glib_sys as glib_ffi;
pub extern crate gobject_sys as gobject_ffi;
pub extern crate gstreamer_sys as gst_ffi;

extern crate gstreamer_base as gst_base;
#[macro_use]
pub extern crate glib;
#[macro_use]
pub extern crate gstreamer as gst;

macro_rules! callback_guard {
    () => (
        let _guard = ::glib::CallbackGuard::new();
    )
}

macro_rules! floating_reference_guard {
    ($obj:ident) => (
        let _guard = $crate::FloatingReferenceGuard::new($obj as *mut _);
    )
}

pub struct FloatingReferenceGuard(*mut gobject_ffi::GObject);

impl FloatingReferenceGuard {
    pub unsafe fn new(obj: *mut gobject_ffi::GObject) -> Option<FloatingReferenceGuard> {
        if gobject_ffi::g_object_is_floating(obj) != glib_ffi::GFALSE {
            gobject_ffi::g_object_ref_sink(obj);
            Some(FloatingReferenceGuard(obj))
        } else {
            None
        }
    }
}

impl Drop for FloatingReferenceGuard {
    fn drop(&mut self) {
        unsafe {
            gobject_ffi::g_object_force_floating(self.0);
        }
    }
}

#[macro_use]
pub mod anyimpl;

#[macro_use]
pub mod utils;
#[macro_use]
pub mod error;
pub mod adapter;
#[macro_use]
pub mod plugin;
pub mod bytes;

#[macro_use]
pub mod object;
#[macro_use]
pub mod element;
#[macro_use]
pub mod base_src;
#[macro_use]
pub mod base_sink;
#[macro_use]
pub mod base_transform;
pub mod uri_handler;
