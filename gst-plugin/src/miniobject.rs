// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{fmt, ops, borrow, ptr};
use std::marker::PhantomData;

use glib;
use gst;

#[derive(Hash, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GstRc<T: MiniObject> {
    obj: T,
}

impl<T: MiniObject> GstRc<T> {
    unsafe fn new(obj: T, owned: bool) -> Self {
        assert!(!obj.as_ptr().is_null());

        if !owned {
            gst::gst_mini_object_ref(obj.as_ptr() as *mut gst::GstMiniObject);
        }

        GstRc { obj: obj }
    }

    pub unsafe fn new_from_owned_ptr(ptr: *mut T::PtrType) -> Self {
        Self::new(T::new_from_ptr(ptr), true)
    }

    pub unsafe fn new_from_unowned_ptr(ptr: *mut T::PtrType) -> Self {
        Self::new(T::new_from_ptr(ptr), false)
    }

    pub fn make_mut(&mut self) -> &mut T {
        unsafe {
            let ptr = self.obj.as_ptr();

            if self.is_writable() {
                return &mut self.obj;
            }

            self.obj.replace_ptr(gst::gst_mini_object_make_writable(ptr as
                                                                    *mut gst::GstMiniObject) as
                                 *mut T::PtrType);
            assert!(self.is_writable());

            &mut self.obj
        }
    }

    pub fn get_mut(&mut self) -> Option<&mut T> {
        if self.is_writable() {
            Some(&mut self.obj)
        } else {
            None
        }
    }

    pub fn copy(&self) -> Self {
        unsafe {
            GstRc::new_from_owned_ptr(gst::gst_mini_object_copy(self.obj.as_ptr() as
                                                                *const gst::GstMiniObject) as
                                      *mut T::PtrType)
        }
    }

    fn is_writable(&self) -> bool {
        (unsafe {
             gst::gst_mini_object_is_writable(self.as_ptr() as *const gst::GstMiniObject)
         } == glib::GTRUE)
    }

    pub unsafe fn into_ptr(mut self) -> *mut T::PtrType {
        self.obj.swap_ptr(ptr::null_mut())
    }
}

impl<T: MiniObject> ops::Deref for GstRc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.obj
    }
}

impl<T: MiniObject> AsRef<T> for GstRc<T> {
    fn as_ref(&self) -> &T {
        &self.obj
    }
}

impl<T: MiniObject> borrow::Borrow<T> for GstRc<T> {
    fn borrow(&self) -> &T {
        &self.obj
    }
}

impl<T: MiniObject> Clone for GstRc<T> {
    fn clone(&self) -> GstRc<T> {
        unsafe { GstRc::new_from_unowned_ptr(self.obj.as_ptr()) }
    }
}

impl<T: MiniObject> Drop for GstRc<T> {
    fn drop(&mut self) {
        unsafe {
            if !self.obj.as_ptr().is_null() {
                gst::gst_mini_object_unref(self.obj.as_ptr() as *mut gst::GstMiniObject);
            }
        }
    }
}

unsafe impl<T: MiniObject + Sync> Sync for GstRc<T> {}
unsafe impl<T: MiniObject + Send> Send for GstRc<T> {}

impl<T: MiniObject + fmt::Display> fmt::Display for GstRc<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.obj.fmt(f)
    }
}

// NOTE: Reference counting must not happen in here
pub unsafe trait MiniObject {
    type PtrType;

    unsafe fn as_ptr(&self) -> *mut Self::PtrType;
    unsafe fn replace_ptr(&mut self, ptr: *mut Self::PtrType);
    unsafe fn swap_ptr(&mut self, new_ptr: *mut Self::PtrType) -> *mut Self::PtrType {
        let ptr = self.as_ptr();
        self.replace_ptr(new_ptr);

        ptr
    }

    unsafe fn new_from_ptr(ptr: *mut Self::PtrType) -> Self;
}

impl<'a, T: MiniObject> From<&'a T> for GstRc<T> {
    fn from(f: &'a T) -> GstRc<T> {
        unsafe { GstRc::new_from_unowned_ptr(f.as_ptr()) }
    }
}

impl<'a, T: MiniObject> From<&'a mut T> for GstRc<T> {
    fn from(f: &'a mut T) -> GstRc<T> {
        unsafe { GstRc::new_from_unowned_ptr(f.as_ptr()) }
    }
}

#[repr(C)]
pub struct GstRefPtr<T: MiniObject>(*mut T::PtrType);

#[derive(Hash, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GstRef<'a, T: 'a + MiniObject> {
    obj: T,
    #[allow(dead_code)]
    phantom: PhantomData<&'a GstRefPtr<T>>,
}

impl<'a, T: MiniObject> GstRef<'a, T> {
    pub unsafe fn new(ptr: &'a GstRefPtr<T>) -> GstRef<'a, T> {
        GstRef {
            obj: T::new_from_ptr(ptr.0 as *mut T::PtrType),
            phantom: PhantomData,
        }
    }

    pub fn get_mut(&mut self) -> Option<&mut T> {
        if self.is_writable() {
            Some(&mut self.obj)
        } else {
            None
        }
    }

    pub fn copy(&self) -> GstRc<T> {
        unsafe {
            GstRc::new_from_owned_ptr(gst::gst_mini_object_copy(self.obj.as_ptr() as
                                                                *const gst::GstMiniObject) as
                                      *mut T::PtrType)
        }
    }

    fn is_writable(&self) -> bool {
        (unsafe {
             gst::gst_mini_object_is_writable(self.as_ptr() as *const gst::GstMiniObject)
         } == glib::GTRUE)
    }

    pub unsafe fn into_ptr(mut self) -> *mut T::PtrType {
        self.obj.swap_ptr(ptr::null_mut())
    }
}

impl<'a, T: MiniObject> ops::Deref for GstRef<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.obj
    }
}

impl<'a, T: MiniObject> AsRef<T> for GstRef<'a, T> {
    fn as_ref(&self) -> &T {
        &self.obj
    }
}

impl<'a, T: MiniObject> borrow::Borrow<T> for GstRef<'a, T> {
    fn borrow(&self) -> &T {
        &self.obj
    }
}

impl<'a, T: MiniObject + fmt::Display> fmt::Display for GstRef<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.obj.fmt(f)
    }
}
