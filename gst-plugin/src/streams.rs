// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ptr;
use std::mem;
use std::ffi::{CStr, CString};
use caps::Caps;
use miniobject::*;
use tags::TagList;

use gst;

pub struct Stream(*mut gst::GstStream);
pub struct StreamCollection(*mut gst::GstStreamCollection);

bitflags! {
    #[repr(C)]
    pub struct StreamType: u32 {
        const TYPE_UNKNOWN   = 0b00000001;
        const TYPE_AUDIO     = 0b00000010;
        const TYPE_VIDEO     = 0b00000100;
        const TYPE_CONTAINER = 0b00001000;
        const TYPE_TEXT      = 0b00010000;
    }
}

bitflags! {
    #[repr(C)]
    pub struct StreamFlags: u32 {
        const FLAG_SPARSE   = 0b00000001;
        const FLAG_SELECT   = 0b00000010;
        const FLAG_UNSELECT = 0b00000100;
    }
}

impl Stream {
    pub fn new(stream_id: &str,
               caps: Option<GstRc<Caps>>,
               t: StreamType,
               flags: StreamFlags)
               -> Self {
        let stream_id_cstr = CString::new(stream_id).unwrap();
        let caps = caps.map(|caps| unsafe { caps.as_mut_ptr() })
            .unwrap_or(ptr::null_mut());

        Stream(unsafe {
                   gst::gst_stream_new(stream_id_cstr.as_ptr(),
                                       caps,
                                       mem::transmute(t.bits()),
                                       mem::transmute(flags.bits()))
               })
    }

    pub unsafe fn as_ptr(&self) -> *const gst::GstStream {
        self.0
    }

    pub fn get_caps(&self) -> Option<&Caps> {
        let ptr = unsafe { gst::gst_stream_get_caps(self.0) };

        if ptr.is_null() {
            return None;
        }

        Some(unsafe { <Caps as MiniObject>::from_ptr(ptr) })
    }

    pub fn get_stream_flags(&self) -> StreamFlags {
        StreamFlags::from_bits_truncate(unsafe { gst::gst_stream_get_stream_flags(self.0).bits() })
    }

    pub fn get_stream_type(&self) -> StreamType {
        StreamType::from_bits_truncate(unsafe { gst::gst_stream_get_stream_type(self.0).bits() })
    }

    pub fn get_stream_id(&self) -> &str {
        let cstr = unsafe { CStr::from_ptr(gst::gst_stream_get_stream_id(self.0)) };
        cstr.to_str().unwrap()
    }

    pub fn get_tags(&self) -> Option<&TagList> {
        let ptr = unsafe { gst::gst_stream_get_tags(self.0) };

        if ptr.is_null() {
            return None;
        }

        Some(unsafe { <TagList as MiniObject>::from_ptr(ptr) })
    }

    pub fn set_caps(&self, caps: Option<GstRc<Caps>>) {
        let ptr = caps.map(|caps| unsafe { caps.as_mut_ptr() })
            .unwrap_or(ptr::null_mut());

        unsafe { gst::gst_stream_set_caps(self.0, ptr) }
    }

    pub fn set_stream_flags(&self, flags: StreamFlags) {
        unsafe { gst::gst_stream_set_stream_flags(self.0, mem::transmute(flags.bits())) }
    }

    pub fn set_stream_type(&self, t: StreamType) {
        unsafe { gst::gst_stream_set_stream_type(self.0, mem::transmute(t.bits())) }
    }

    pub fn set_tags(&self, tags: Option<TagList>) {
        let ptr = tags.map(|tags| unsafe { tags.as_mut_ptr() })
            .unwrap_or(ptr::null_mut());

        unsafe { gst::gst_stream_set_tags(self.0, ptr) }
    }
}

impl Clone for Stream {
    fn clone(&self) -> Self {
        unsafe { Stream(gst::gst_object_ref(self.0 as *mut gst::GstObject) as *mut gst::GstStream) }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe { gst::gst_object_unref(self.0 as *mut gst::GstObject) }
    }
}

impl StreamCollection {
    pub fn new(upstream_id: &str, streams: &[Stream]) -> Self {
        let upstream_id_cstr = CString::new(upstream_id).unwrap();
        let collection =
            StreamCollection(unsafe { gst::gst_stream_collection_new(upstream_id_cstr.as_ptr()) });

        for stream in streams {
            unsafe { gst::gst_stream_collection_add_stream(collection.0, stream.clone().0) };
        }

        collection
    }

    pub fn streams(&self) -> StreamCollectionIterator {
        StreamCollectionIterator::new(self)
    }

    pub fn len(&self) -> u32 {
        unsafe { gst::gst_stream_collection_get_size(self.0) }
    }

    pub fn empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get_upstream_id(&self) -> &str {
        let cstr = unsafe { CStr::from_ptr(gst::gst_stream_collection_get_upstream_id(self.0)) };
        cstr.to_str().unwrap()
    }

    pub unsafe fn as_ptr(&self) -> *const gst::GstStreamCollection {
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
        if self.position == self.length {
            return None;
        }

        let stream =
            unsafe { gst::gst_stream_collection_get_stream(self.collection.0, self.position) };
        if stream.is_null() {
            self.position = self.length;
            return None;
        }
        self.position += 1;

        Some(unsafe {
                 Stream(gst::gst_object_ref(stream as *mut gst::GstObject) as *mut gst::GstStream)
             })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.position == self.length {
            return (0, Some(0));
        }

        let remaining = (self.length - self.position) as usize;

        (remaining, Some(remaining))
    }
}

impl<'a> DoubleEndedIterator for StreamCollectionIterator<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.position == self.length {
            return None;
        }

        self.length -= 1;

        let stream =
            unsafe { gst::gst_stream_collection_get_stream(self.collection.0, self.length) };
        if stream.is_null() {
            self.position = self.length;
            return None;
        }

        Some(unsafe {
                 Stream(gst::gst_object_ref(stream as *mut gst::GstObject) as *mut gst::GstStream)
             })
    }
}

impl<'a> ExactSizeIterator for StreamCollectionIterator<'a> {}

impl Clone for StreamCollection {
    fn clone(&self) -> Self {
        unsafe {
            StreamCollection(gst::gst_object_ref(self.0 as *mut gst::GstObject) as
                             *mut gst::GstStreamCollection)
        }
    }
}

impl Drop for StreamCollection {
    fn drop(&mut self) {
        unsafe { gst::gst_object_unref(self.0 as *mut gst::GstObject) }
    }
}
