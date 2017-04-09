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
use value::*;
use miniobject::*;

use glib;
use gobject;
use gst;

pub trait Tag {
    type TagType: ValueType;
    fn tag_name() -> &'static str;
}

macro_rules! impl_tag(
    ($name:ident, $t:ty, $tag:expr) => {
        pub struct $name;
        impl Tag for $name {
            type TagType = $t;
            fn tag_name() -> &'static str {
                $tag
            }
        }
    };
);

impl_tag!(Title, String, "title");
impl_tag!(Album, String, "album");
impl_tag!(Artist, String, "artist");
impl_tag!(Encoder, String, "encoder");
impl_tag!(AudioCodec, String, "audio-codec");
impl_tag!(VideoCodec, String, "video-codec");
impl_tag!(SubtitleCodec, String, "subtitle-codec");
impl_tag!(ContainerFormat, String, "container-format");
// TODO: Should ideally enforce this to be ISO-639
impl_tag!(LanguageCode, String, "language-code");
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

    unsafe fn new_from_ptr(ptr: *mut gst::GstTagList) -> Self {
        TagList(ptr)
    }
}

impl TagList {
    pub fn new() -> GstRc<Self> {
        unsafe { GstRc::new_from_owned_ptr(gst::gst_tag_list_new_empty()) }
    }

    pub fn add<T: Tag>(&mut self, value: T::TagType, mode: MergeMode)
        where Value: From<<T as Tag>::TagType>
    {
        unsafe {
            let v = Value::from(value);
            let mut gvalue = v.to_gvalue();
            let tag_name = CString::new(T::tag_name()).unwrap();

            gst::gst_tag_list_add_value(self.0, mode.to_ffi(), tag_name.as_ptr(), &gvalue);

            gobject::g_value_unset(&mut gvalue);
        }
    }

    pub fn get<T: Tag>(&self) -> Option<TypedValue<T::TagType>>
        where Value: From<<T as Tag>::TagType>
    {
        unsafe {
            let mut gvalue = mem::zeroed();
            let tag_name = CString::new(T::tag_name()).unwrap();

            let found = gst::gst_tag_list_copy_value(&mut gvalue, self.0, tag_name.as_ptr());

            if found == glib::GFALSE {
                return None;
            }

            let res = match Value::from_gvalue(&gvalue) {
                Some(value) => Some(TypedValue::new(value)),
                None => None,
            };

            gobject::g_value_unset(&mut gvalue);

            res
        }
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

        assert_eq!(*tags.get::<Title>().unwrap(), "some title");
        assert_eq!(*tags.get::<Duration>().unwrap(),
                   (1000u64 * 1000 * 1000 * 120));
    }
}
