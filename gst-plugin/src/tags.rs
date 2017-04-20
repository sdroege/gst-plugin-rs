// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::fmt;
use std::mem;
use std::ffi::{CStr, CString};
use std::marker::PhantomData;
use value::*;
use miniobject::*;

use glib;
use gobject;
use gst;

pub trait Tag<'a> {
    type TagType: ValueType<'a>;
    fn tag_name() -> &'static str;
}

macro_rules! impl_tag(
    ($name:ident, $t:ty, $tag:expr) => {
        pub struct $name;
        impl<'a> Tag<'a> for $name {
            type TagType = $t;
            fn tag_name() -> &'static str {
                $tag
            }
        }
    };
);

impl_tag!(Title, &'a str, "title");
impl_tag!(Album, &'a str, "album");
impl_tag!(Artist, &'a str, "artist");
impl_tag!(Encoder, &'a str, "encoder");
impl_tag!(AudioCodec, &'a str, "audio-codec");
impl_tag!(VideoCodec, &'a str, "video-codec");
impl_tag!(SubtitleCodec, &'a str, "subtitle-codec");
impl_tag!(ContainerFormat, &'a str, "container-format");
// TODO: Should ideally enforce this to be ISO-639
impl_tag!(LanguageCode, &'a str, "language-code");
impl_tag!(Duration, u64, "duration");
impl_tag!(NominalBitrate, u32, "nominal-bitrate");

pub enum MergeMode {
    ReplaceAll,
    Replace,
    Append,
    Prepend,
    Keep,
    KeepAll,
}

impl MergeMode {
    fn to_ffi(&self) -> gst::GstTagMergeMode {
        match *self {
            MergeMode::ReplaceAll => gst::GST_TAG_MERGE_REPLACE_ALL,
            MergeMode::Replace => gst::GST_TAG_MERGE_REPLACE,
            MergeMode::Append => gst::GST_TAG_MERGE_APPEND,
            MergeMode::Prepend => gst::GST_TAG_MERGE_PREPEND,
            MergeMode::Keep => gst::GST_TAG_MERGE_KEEP,
            MergeMode::KeepAll => gst::GST_TAG_MERGE_KEEP_ALL,
        }
    }
}

#[derive(Eq)]
pub struct TagList(*mut gst::GstTagList);

unsafe impl MiniObject for TagList {
    type PtrType = gst::GstTagList;

    unsafe fn as_ptr(&self) -> *mut gst::GstTagList {
        self.0
    }

    unsafe fn replace_ptr(&mut self, ptr: *mut gst::GstTagList) {
        self.0 = ptr
    }

    unsafe fn from_ptr(ptr: *mut gst::GstTagList) -> Self {
        TagList(ptr)
    }
}

impl TagList {
    pub fn new() -> GstRc<Self> {
        unsafe { GstRc::from_owned_ptr(gst::gst_tag_list_new_empty()) }
    }

    pub fn add<'a, T: Tag<'a>>(&mut self, value: T::TagType, mode: MergeMode)
        where T::TagType: Into<Value>
    {
        unsafe {
            let v = value.into();
            let mut gvalue = v.into_raw();
            let tag_name = CString::new(T::tag_name()).unwrap();

            gst::gst_tag_list_add_value(self.0, mode.to_ffi(), tag_name.as_ptr(), &gvalue);

            gobject::g_value_unset(&mut gvalue);
        }
    }

    pub fn get<'a, T: Tag<'a>>(&self) -> Option<TypedValue<T::TagType>> {
        unsafe {
            let mut gvalue = mem::zeroed();
            let tag_name = CString::new(T::tag_name()).unwrap();

            let found = gst::gst_tag_list_copy_value(&mut gvalue, self.0, tag_name.as_ptr());

            if found == glib::GFALSE {
                return None;
            }

            let res = match Value::from_raw(gvalue) {
                Some(value) => TypedValue::from_value(value),
                None => None,
            };

            res
        }
    }

    pub fn get_index<'a, T: Tag<'a>>(&'a self, idx: u32) -> Option<TypedValueRef<'a, T::TagType>> {
        unsafe {
            let tag_name = CString::new(T::tag_name()).unwrap();

            let value = gst::gst_tag_list_get_value_index(self.0, tag_name.as_ptr(), idx);

            if value.is_null() {
                return None;
            }

            let res = match ValueRef::from_ptr(value) {
                Some(value) => TypedValueRef::from_value_ref(value),
                None => None,
            };

            res
        }
    }

    pub fn get_size<'a, T: Tag<'a>>(&'a self) -> u32 {
        unsafe {
            let tag_name = CString::new(T::tag_name()).unwrap();

            gst::gst_tag_list_get_tag_size(self.0, tag_name.as_ptr())
        }
    }

    pub fn iter_tag<'a, T: Tag<'a>>(&'a self) -> TagIterator<'a, T> {
        TagIterator::new(self)
    }

    pub fn to_string(&self) -> String {
        unsafe {
            let ptr = gst::gst_tag_list_to_string(self.0);
            let s = CStr::from_ptr(ptr).to_str().unwrap().into();
            glib::g_free(ptr as glib::gpointer);

            s
        }
    }
}

impl fmt::Debug for TagList {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.to_string())
    }
}

impl PartialEq for TagList {
    fn eq(&self, other: &TagList) -> bool {
        (unsafe { gst::gst_tag_list_is_equal(self.0, other.0) } == glib::GTRUE)
    }
}

unsafe impl Sync for TagList {}
unsafe impl Send for TagList {}

pub struct TagIterator<'a, T: Tag<'a>> {
    taglist: &'a TagList,
    idx: u32,
    size: u32,
    phantom: PhantomData<T>,
}

impl<'a, T: Tag<'a>> TagIterator<'a, T> {
    fn new(taglist: &'a TagList) -> TagIterator<'a, T> {
        TagIterator {
            taglist: taglist,
            idx: 0,
            size: taglist.get_size::<T>(),
            phantom: PhantomData,
        }
    }
}

impl<'a, T: Tag<'a>> Iterator for TagIterator<'a, T> {
    type Item = TypedValueRef<'a, T::TagType>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.idx >= self.size {
            return None;
        }

        let item = self.taglist.get_index::<T>(self.idx);
        self.idx += 1;

        item
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.idx == self.size {
            return (0, Some(0));
        }

        let remaining = (self.size - self.idx) as usize;

        (remaining, Some(remaining))
    }
}

impl<'a, T: Tag<'a>> DoubleEndedIterator for TagIterator<'a, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.idx == self.size {
            return None;
        }

        self.size -= 1;
        self.taglist.get_index::<T>(self.size)
    }
}

impl<'a, T: Tag<'a>> ExactSizeIterator for TagIterator<'a, T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    fn init() {
        unsafe {
            gst::gst_init(ptr::null_mut(), ptr::null_mut());
        }
    }

    #[test]
    fn test_add() {
        init();

        let mut tags = TagList::new();
        assert_eq!(tags.to_string(), "taglist;");
        {
            let tags = tags.get_mut().unwrap();
            tags.add::<Title>("some title".into(), MergeMode::Append);
            tags.add::<Duration>((1000u64 * 1000 * 1000 * 120).into(), MergeMode::Append);
        }
        assert_eq!(tags.to_string(),
                   "taglist, title=(string)\"some\\ title\", duration=(guint64)120000000000;");
    }

    #[test]
    fn test_get() {
        init();

        let mut tags = TagList::new();
        assert_eq!(tags.to_string(), "taglist;");
        {
            let tags = tags.get_mut().unwrap();
            tags.add::<Title>("some title".into(), MergeMode::Append);
            tags.add::<Duration>((1000u64 * 1000 * 1000 * 120).into(), MergeMode::Append);
        }

        assert_eq!(tags.get::<Title>().unwrap().get(), "some title");
        assert_eq!(tags.get::<Duration>().unwrap().get(),
                   (1000u64 * 1000 * 1000 * 120));
        assert_eq!(tags.get_index::<Title>(0).unwrap().get(), "some title");
        assert_eq!(tags.get_index::<Duration>(0).unwrap().get(),
                   (1000u64 * 1000 * 1000 * 120));
    }
}
