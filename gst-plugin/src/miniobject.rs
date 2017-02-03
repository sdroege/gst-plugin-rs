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
use std::{fmt, ops, borrow, ptr};
use std::marker::PhantomData;
use utils::*;

#[derive(Hash, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GstRc<T: MiniObject> {
    obj: T,
}

impl<T: MiniObject> GstRc<T> {
    unsafe fn new(obj: T, owned: bool) -> Self {
        extern "C" {
            fn gst_mini_object_ref(obj: *mut c_void) -> *mut c_void;
        }

        assert!(!obj.as_ptr().is_null());

        if !owned {
            gst_mini_object_ref(obj.as_ptr());
        }

        GstRc { obj: obj }
    }

    pub unsafe fn new_from_owned_ptr(ptr: *mut c_void) -> Self {
        Self::new(T::new_from_ptr(ptr), true)
    }

    pub unsafe fn new_from_unowned_ptr(ptr: *mut c_void) -> Self {
        Self::new(T::new_from_ptr(ptr), false)
    }

    pub fn make_mut(&mut self) -> &mut T {
        extern "C" {
            fn gst_mini_object_make_writable(obj: *mut c_void) -> *mut c_void;
        }
        unsafe {
            let ptr = self.obj.as_ptr();

            if self.is_writable() {
                return &mut self.obj;
            }

            self.obj.replace_ptr(gst_mini_object_make_writable(ptr));
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
        extern "C" {
            fn gst_mini_object_copy(obj: *const c_void) -> *mut c_void;
        }
        unsafe { GstRc::new_from_owned_ptr(gst_mini_object_copy(self.obj.as_ptr())) }
    }

    pub unsafe fn into_ptr(mut self) -> *mut c_void {
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
        extern "C" {
            fn gst_mini_object_unref(obj: *mut c_void) -> *mut c_void;
        }

        unsafe {
            if !self.obj.as_ptr().is_null() {
                gst_mini_object_unref(self.obj.as_ptr());
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
    unsafe fn as_ptr(&self) -> *mut c_void;
    unsafe fn replace_ptr(&mut self, ptr: *mut c_void);
    unsafe fn swap_ptr(&mut self, new_ptr: *mut c_void) -> *mut c_void {
        let ptr = self.as_ptr();
        self.replace_ptr(new_ptr);

        ptr
    }

    unsafe fn new_from_ptr(ptr: *mut c_void) -> Self;

    fn is_writable(&self) -> bool {
        extern "C" {
            fn gst_mini_object_is_writable(obj: *mut c_void) -> GBoolean;
        }
        unsafe { gst_mini_object_is_writable(self.as_ptr()).to_bool() }
    }
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
pub struct GstRefPtr(*mut c_void);

#[derive(Hash, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GstRef<'a, T: MiniObject> {
    obj: T,
    #[allow(dead_code)]
    phantom: PhantomData<&'a GstRefPtr>,
}

impl<'a, T: MiniObject> GstRef<'a, T> {
    pub unsafe fn new(ptr: &'a GstRefPtr) -> GstRef<'a, T> {
        GstRef {
            obj: T::new_from_ptr(ptr.0),
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
        extern "C" {
            fn gst_mini_object_copy(obj: *const c_void) -> *mut c_void;
        }
        unsafe { GstRc::new_from_owned_ptr(gst_mini_object_copy(self.obj.as_ptr())) }
    }

    pub unsafe fn into_ptr(mut self) -> *mut c_void {
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
