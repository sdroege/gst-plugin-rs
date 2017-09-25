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
extern crate mopa;
extern crate url;
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

// mopafy! macro to work with generic traits over T: ObjectType
macro_rules! mopafy_object_impl {
    ($bound:ident, $trait:ident) => {
        impl<T: $bound> $trait<T> {
            #[inline]
            pub fn downcast_ref<U: $trait<T>>(&self) -> Option<&U> {
                if self.is::<U>() {
                    unsafe {
                        Some(self.downcast_ref_unchecked())
                    }
                } else {
                    None
                }
            }

            #[inline]
            pub unsafe fn downcast_ref_unchecked<U: $trait<T>>(&self) -> &U {
                &*(self as *const Self as *const U)
            }

            #[inline]
            pub fn is<U: $trait<T>>(&self) -> bool {
                use std::any::TypeId;
                use mopa;
                TypeId::of::<U>() == mopa::Any::get_type_id(self)
            }
        }
    };
}

#[macro_use]
pub mod utils;
#[macro_use]
pub mod error;
pub mod adapter;
#[macro_use]
pub mod plugin;
pub mod source;
pub mod sink;
pub mod demuxer;
pub mod bytes;

#[macro_use]
pub mod object;
#[macro_use]
pub mod element;
#[macro_use]
pub mod base_src;
#[macro_use]
pub mod base_sink;
pub mod uri_handler;
