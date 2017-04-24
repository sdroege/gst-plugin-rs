// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{fmt, ops, borrow};
use std::mem;

use glib;
use gst;

#[derive(Hash, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GstRc<T: 'static + MiniObject> {
    obj: &'static T,
}

impl<T: MiniObject> GstRc<T> {
    unsafe fn new(obj: &'static T, owned: bool) -> Self {
        if !owned {
            gst::gst_mini_object_ref(obj.as_ptr() as *mut gst::GstMiniObject);
        }

        GstRc { obj: obj }
    }

    pub unsafe fn from_owned_ptr(ptr: *const T::PtrType) -> Self {
        Self::new(T::from_ptr(ptr), true)
    }

    pub unsafe fn from_unowned_ptr(ptr: *const T::PtrType) -> Self {
        Self::new(T::from_ptr(ptr), false)
    }

    pub fn make_mut(&mut self) -> &mut T {
        unsafe {
            let ptr = self.obj.as_ptr();

            if self.is_writable() {
                return &mut *(self.obj as *const T as *mut T);
            }

            self.obj = T::from_ptr(gst::gst_mini_object_make_writable(ptr as
                                                                      *mut gst::GstMiniObject) as
                                   *const T::PtrType);
            assert!(self.is_writable());

            &mut *(self.obj as *const T as *mut T)
        }
    }

    pub fn get_mut(&mut self) -> Option<&mut T> {
        if self.is_writable() {
            Some(unsafe { &mut *(self.obj as *const T as *mut T) })
        } else {
            None
        }
    }

    pub fn copy(&self) -> Self {
        unsafe {
            GstRc::from_owned_ptr(gst::gst_mini_object_copy(self.obj.as_ptr() as
                                                            *const gst::GstMiniObject) as
                                  *const T::PtrType)
        }
    }

    fn is_writable(&self) -> bool {
        (unsafe {
             gst::gst_mini_object_is_writable(self.as_ptr() as *const gst::GstMiniObject)
         } == glib::GTRUE)
    }

    pub unsafe fn into_ptr(self) -> *const T::PtrType {
        let ptr = self.obj.as_ptr();
        mem::forget(self);

        ptr
    }
}

impl<T: MiniObject> ops::Deref for GstRc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.obj
    }
}

impl<T: MiniObject> AsRef<T> for GstRc<T> {
    fn as_ref(&self) -> &T {
        self.obj
    }
}

impl<T: MiniObject> borrow::Borrow<T> for GstRc<T> {
    fn borrow(&self) -> &T {
        self.obj
    }
}

// FIXME: Not generally possible because neither T nor ToOwned are defined here...
//impl<T: MiniObject> ToOwned for T {
//    type Owned = GstRc<T>;
//
//    fn to_owned(&self) -> GstRc<T> {
//        unsafe { GstRc::from_unowned_ptr(self.as_ptr()) }
//    }
//}

impl<T: MiniObject> Clone for GstRc<T> {
    fn clone(&self) -> GstRc<T> {
        unsafe { GstRc::from_unowned_ptr(self.obj.as_ptr()) }
    }
}

impl<T: MiniObject> Drop for GstRc<T> {
    fn drop(&mut self) {
        unsafe {
            gst::gst_mini_object_unref(self.obj.as_ptr() as *mut gst::GstMiniObject);
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

pub unsafe trait MiniObject
    where Self: Sized
{
    type PtrType;

    unsafe fn as_ptr(&self) -> *const Self::PtrType {
        self as *const Self as *const Self::PtrType
    }

    unsafe fn as_mut_ptr(&self) -> *mut Self::PtrType {
        self as *const Self as *mut Self::PtrType
    }

    unsafe fn from_ptr<'a>(ptr: *const Self::PtrType) -> &'a Self {
        &*(ptr as *const Self)
    }

    unsafe fn from_mut_ptr<'a>(ptr: *mut Self::PtrType) -> &'a mut Self {
        &mut *(ptr as *mut Self)
    }
}

impl<'a, T: MiniObject> From<&'a T> for GstRc<T> {
    fn from(f: &'a T) -> GstRc<T> {
        unsafe { GstRc::from_unowned_ptr(f.as_ptr()) }
    }
}

impl<'a, T: MiniObject> From<&'a mut T> for GstRc<T> {
    fn from(f: &'a mut T) -> GstRc<T> {
        unsafe { GstRc::from_unowned_ptr(f.as_ptr()) }
    }
}
