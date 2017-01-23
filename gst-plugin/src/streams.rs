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
use std::ptr;
use libc::c_char;
use std::ffi::{CStr, CString};
use caps::Caps;
use tags::TagList;

pub struct Stream(*mut c_void);
pub struct StreamCollection(*mut c_void);

bitflags! {
    #[repr(C)]
    pub flags StreamType: u32 {
        const TYPE_UNKNOWN   = 0b00000001,
        const TYPE_AUDIO     = 0b00000010,
        const TYPE_VIDEO     = 0b00000100,
        const TYPE_CONTAINER = 0b00001000,
        const TYPE_TEXT      = 0b00010000,
    }
}

bitflags! {
    #[repr(C)]
    pub flags StreamFlags: u32 {
        const FLAG_SPARSE   = 0b00000001,
        const FLAG_SELECT   = 0b00000010,
        const FLAG_UNSELECT = 0b00000100,
    }
}

impl Stream {
    pub fn new(stream_id: &str, caps: Option<Caps>, t: StreamType, flags: StreamFlags) -> Self {
        extern "C" {
            fn gst_stream_new(stream_id: *const c_char,
                              caps: *const c_void,
                              t: StreamType,
                              flags: StreamFlags)
                              -> *mut c_void;
        }

        let stream_id_cstr = CString::new(stream_id).unwrap();
        let caps = caps.map(|caps| unsafe { caps.as_ptr() }).unwrap_or(ptr::null_mut());

        Stream(unsafe { gst_stream_new(stream_id_cstr.as_ptr(), caps, t, flags) })
    }

    pub unsafe fn as_ptr(&self) -> *const c_void {
        self.0
    }

    pub fn get_caps(&self) -> Option<Caps> {
        extern "C" {
            fn gst_stream_get_caps(stream: *mut c_void) -> *mut c_void;
        }

        let ptr = unsafe { gst_stream_get_caps(self.0) };

        if ptr.is_null() {
            return None;
        }

        Some(unsafe { Caps::new_from_ptr(ptr) })
    }

    pub fn get_stream_flags(&self) -> StreamFlags {
        extern "C" {
            fn gst_stream_get_stream_flags(stream: *mut c_void) -> u32;
        }

        StreamFlags::from_bits_truncate(unsafe { gst_stream_get_stream_flags(self.0) })
    }

    pub fn get_stream_type(&self) -> StreamType {
        extern "C" {
            fn gst_stream_get_stream_type(stream: *mut c_void) -> u32;
        }

        StreamType::from_bits_truncate(unsafe { gst_stream_get_stream_type(self.0) })
    }

    pub fn get_stream_id(&self) -> &str {
        extern "C" {
            fn gst_stream_get_stream_id(collection: *mut c_void) -> *mut c_char;
        }

        unsafe {
            CStr::from_ptr(gst_stream_get_stream_id(self.0))
                .to_str()
                .unwrap()
        }
    }

    pub fn get_tags(&self) -> Option<TagList> {
        extern "C" {
            fn gst_stream_get_tags(stream: *mut c_void) -> *mut c_void;
        }

        let ptr = unsafe { gst_stream_get_tags(self.0) };

        if ptr.is_null() {
            return None;
        }

        Some(unsafe { TagList::new_from_ptr(ptr) })
    }

    pub fn set_caps(&self, caps: Option<Caps>) {
        extern "C" {
            fn gst_stream_set_caps(stream: *mut c_void, caps: *mut c_void);
        }

        let ptr = caps.map(|caps| unsafe { caps.as_ptr() }).unwrap_or(ptr::null_mut());

        unsafe { gst_stream_set_caps(self.0, ptr as *mut c_void) }
    }

    pub fn set_stream_flags(&self, flags: StreamFlags) {
        extern "C" {
            fn gst_stream_set_stream_flags(stream: *mut c_void, flags: u32);
        }

        unsafe { gst_stream_set_stream_flags(self.0, flags.bits()) }
    }

    pub fn set_stream_type(&self, t: StreamType) {
        extern "C" {
            fn gst_stream_set_stream_type(stream: *mut c_void, t: u32);
        }

        unsafe { gst_stream_set_stream_type(self.0, t.bits()) }
    }

    pub fn set_tags(&self, tags: Option<TagList>) {
        extern "C" {
            fn gst_stream_set_tags(stream: *mut c_void, tags: *mut c_void);
        }

        let ptr = tags.map(|tags| unsafe { tags.as_ptr() }).unwrap_or(ptr::null_mut());

        unsafe { gst_stream_set_tags(self.0, ptr as *mut c_void) }
    }
}

impl Clone for Stream {
    fn clone(&self) -> Self {
        extern "C" {
            fn gst_object_ref(object: *mut c_void) -> *mut c_void;
        }

        unsafe { Stream(gst_object_ref(self.0)) }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        extern "C" {
            fn gst_object_unref(object: *mut c_void);
        }

        unsafe { gst_object_unref(self.0) }
    }
}

impl StreamCollection {
    pub fn new(upstream_id: &str, streams: &[Stream]) -> Self {
        extern "C" {
            fn gst_stream_collection_new(upstream_id: *const c_char) -> *mut c_void;
            fn gst_stream_collection_add_stream(collection: *mut c_void, stream: *mut c_void);
        }

        let upstream_id_cstr = CString::new(upstream_id).unwrap();
        let collection =
            StreamCollection(unsafe { gst_stream_collection_new(upstream_id_cstr.as_ptr()) });

        for stream in streams {
            unsafe { gst_stream_collection_add_stream(collection.0, stream.clone().0) }
        }

        collection
    }

    pub fn streams(&self) -> StreamCollectionIterator {
        StreamCollectionIterator::new(self)
    }

    pub fn len(&self) -> u32 {
        extern "C" {
            fn gst_stream_collection_get_size(collection: *mut c_void) -> u32;
        }

        unsafe { gst_stream_collection_get_size(self.0) }
    }

    pub fn get_upstream_id(&self) -> &str {
        extern "C" {
            fn gst_stream_collection_get_upstream_id(collection: *mut c_void) -> *mut c_char;
        }

        unsafe {
            CStr::from_ptr(gst_stream_collection_get_upstream_id(self.0))
                .to_str()
                .unwrap()
        }
    }

    pub unsafe fn as_ptr(&self) -> *const c_void {
        self.0
    }
}

pub struct StreamCollectionIterator<'a> {
    position: u32,
    length: u32,
    collection: &'a StreamCollection,
}

impl<'a> StreamCollectionIterator<'a> {
    fn new(collection: &'a StreamCollection) -> Self {
        StreamCollectionIterator {
            position: 0,
            length: collection.len(),
            collection: collection,
        }
    }
}

impl<'a> Iterator for StreamCollectionIterator<'a> {
    type Item = Stream;

    fn next(&mut self) -> Option<Stream> {
        extern "C" {
            fn gst_stream_collection_get_stream(collection: *mut c_void,
                                                index: u32)
                                                -> *mut c_void;
            fn gst_object_ref(object: *mut c_void) -> *mut c_void;
        }

        if self.position == self.length {
            return None;
        }

        let stream = unsafe { gst_stream_collection_get_stream(self.collection.0, self.position) };
        if stream.is_null() {
            self.position = self.length;
            return None;
        }
        self.position += 1;

        Some(unsafe { Stream(gst_object_ref(stream)) })
    }
}

impl Clone for StreamCollection {
    fn clone(&self) -> Self {
        extern "C" {
            fn gst_object_ref(object: *mut c_void) -> *mut c_void;
        }

        unsafe { StreamCollection(gst_object_ref(self.0)) }
    }
}

impl Drop for StreamCollection {
    fn drop(&mut self) {
        extern "C" {
            fn gst_object_unref(object: *mut c_void);
        }

        unsafe { gst_object_unref(self.0) }
    }
}
