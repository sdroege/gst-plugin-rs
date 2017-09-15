// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{borrow, fmt, ops};
use std::mem;
use std::marker::PhantomData;

use glib_ffi;
use gst_ffi;

#[derive(Hash, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GstRc<T: MiniObject>(*mut T, PhantomData<T>);

impl<T: MiniObject> GstRc<T> {
    unsafe fn new(obj: *const T, owned: bool) -> Self {
        assert!(!obj.is_null());

        if !owned {
            gst_ffi::gst_mini_object_ref((&*obj).as_ptr() as *mut gst_ffi::GstMiniObject);
        }

        GstRc(obj as *mut T, PhantomData)
    }

    pub unsafe fn from_owned_ptr(ptr: *const T::PtrType) -> Self {
        Self::new(T::from_ptr(ptr), true)
    }

    pub unsafe fn from_unowned_ptr(ptr: *const T::PtrType) -> Self {
        Self::new(T::from_ptr(ptr), false)
    }

    pub fn make_mut(&mut self) -> &mut T {
        unsafe {
            if self.is_writable() {
                return &mut *self.0;
            }

            self.0 = T::from_mut_ptr(gst_ffi::gst_mini_object_make_writable(
                self.as_mut_ptr() as *mut gst_ffi::GstMiniObject,
            ) as *mut T::PtrType);
            assert!(self.is_writable());

            &mut *self.0
        }
    }

    pub fn get_mut(&mut self) -> Option<&mut T> {
        if self.is_writable() {
            Some(unsafe { &mut *self.0 })
        } else {
            None
        }
    }

    pub fn copy(&self) -> Self {
        unsafe {
            GstRc::from_owned_ptr(
                gst_ffi::gst_mini_object_copy(self.as_ptr() as *const gst_ffi::GstMiniObject) as
                    *const T::PtrType,
            )
        }
    }

    fn is_writable(&self) -> bool {
        (unsafe {
            gst_ffi::gst_mini_object_is_writable(self.as_ptr() as *const gst_ffi::GstMiniObject)
        } == glib_ffi::GTRUE)
    }

    pub unsafe fn into_ptr(self) -> *mut T::PtrType {
        let ptr = self.as_mut_ptr();
        mem::forget(self);

        ptr
    }
}

impl<T: MiniObject> ops::Deref for GstRc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.as_ref()
    }
}

impl<T: MiniObject> AsRef<T> for GstRc<T> {
    fn as_ref(&self) -> &T {
        unsafe { &*self.0 }
    }
}

impl<T: MiniObject> borrow::Borrow<T> for GstRc<T> {
    fn borrow(&self) -> &T {
        self.as_ref()
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
        unsafe { GstRc::from_unowned_ptr(self.as_ptr()) }
    }
}

impl<T: MiniObject> Drop for GstRc<T> {
    fn drop(&mut self) {
        unsafe {
            gst_ffi::gst_mini_object_unref(self.as_ptr() as *mut gst_ffi::GstMiniObject);
        }
    }
}

unsafe impl<T: MiniObject + Sync> Sync for GstRc<T> {}
unsafe impl<T: MiniObject + Send> Send for GstRc<T> {}

impl<T: MiniObject + fmt::Display> fmt::Display for GstRc<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        (unsafe { &*self.0 }).fmt(f)
    }
}

pub unsafe trait MiniObject
where
    Self: Sized,
{
    type PtrType;

    unsafe fn as_ptr(&self) -> *const Self::PtrType {
        self as *const Self as *const Self::PtrType
    }

    unsafe fn as_mut_ptr(&self) -> *mut Self::PtrType {
        self as *const Self as *mut Self::PtrType
    }

    unsafe fn from_ptr<'a>(ptr: *const Self::PtrType) -> &'a Self {
        assert!(!ptr.is_null());
        &*(ptr as *const Self)
    }

    unsafe fn from_mut_ptr<'a>(ptr: *mut Self::PtrType) -> &'a mut Self {
        assert!(!ptr.is_null());
        &mut *(ptr as *mut Self)
    }
}
