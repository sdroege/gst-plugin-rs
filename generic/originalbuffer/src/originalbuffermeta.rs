// Copyright (C) 2024 Collabora Ltd
//   @author: Olivier Crête <olivier.crete@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use std::fmt;
use std::mem;

/// A single geometric transform that was applied to a buffer (and, along with
/// it, to `OriginalBufferMeta`) between save and restore. Kept as an ordered
/// list, rather than a single collapsed matrix, so that `originalbufferrestore`
/// can replay each transform in reverse using whichever mechanism (scale or
/// matrix) the original transform used, and so that crop/letterbox rectangles
/// carried by matrix transforms aren't lost.
#[derive(Clone, Debug)]
pub enum RecordedTransform {
    Scale {
        in_info: gst_video::VideoInfo,
        out_info: gst_video::VideoInfo,
    },
    Matrix {
        in_info: gst_video::VideoInfo,
        in_rectangle: gst_video::VideoRectangle,
        out_info: gst_video::VideoInfo,
        out_rectangle: gst_video::VideoRectangle,
        matrix: [[f32; 3]; 3],
    },
}

use std::sync::LazyLock;
#[allow(dead_code)]
static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "originalbuffermeta",
        gst::DebugColorFlags::empty(),
        Some("Original Buffer Meta"),
    )
});

#[repr(transparent)]
pub struct OriginalBufferMeta(imp::OriginalBufferMeta);

unsafe impl Send for OriginalBufferMeta {}
unsafe impl Sync for OriginalBufferMeta {}

impl OriginalBufferMeta {
    pub(crate) fn add(
        buffer: &mut gst::BufferRef,
        original: gst::Buffer,
        caps: Option<gst::Caps>,
        transforms: Vec<RecordedTransform>,
    ) -> gst::MetaRefMut<'_, Self, gst::meta::Standalone> {
        unsafe {
            // Manually dropping because gst_buffer_add_meta() takes ownership of the
            // content of the struct
            let mut params = mem::ManuallyDrop::new(imp::OriginalBufferMetaParams {
                original,
                caps,
                transforms,
            });

            let meta = gst::ffi::gst_buffer_add_meta(
                buffer.as_mut_ptr(),
                imp::original_buffer_meta_get_info(),
                &mut *params as *mut imp::OriginalBufferMetaParams as gst::glib::ffi::gpointer,
            ) as *mut imp::OriginalBufferMeta;

            Self::from_mut_ptr(buffer, meta)
        }
    }

    pub fn replace(&mut self, original: gst::Buffer, caps: Option<gst::Caps>) {
        self.0.original = Some(original);
        self.0.caps = caps;
        // A fresh original was stored: any transforms recorded so far were
        // relative to the previous original and no longer apply.
        self.0.transforms.clear();
    }

    pub fn original(&self) -> &gst::Buffer {
        self.0.original.as_ref().unwrap()
    }

    pub fn caps(&self) -> &gst::Caps {
        self.0.caps.as_ref().unwrap()
    }

    /// The ordered list of transforms that were applied to the buffer (and
    /// hence to this meta) since it was added or last replaced, oldest first.
    pub fn transforms(&self) -> &[RecordedTransform] {
        &self.0.transforms
    }
}

unsafe impl MetaAPI for OriginalBufferMeta {
    type GstType = imp::OriginalBufferMeta;

    fn meta_api() -> gst::glib::Type {
        imp::original_buffer_meta_api_get_type()
    }
}

impl fmt::Debug for OriginalBufferMeta {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("OriginalBufferMeta")
            .field("buffer", &self.original())
            .finish()
    }
}

mod imp {
    use super::CAT;
    use gst::glib::translate::*;
    use std::mem;
    use std::ptr;
    use std::sync::LazyLock;

    pub(super) struct OriginalBufferMetaParams {
        pub original: gst::Buffer,
        pub caps: Option<gst::Caps>,
        pub transforms: Vec<super::RecordedTransform>,
    }

    #[repr(C)]
    pub struct OriginalBufferMeta {
        parent: gst::ffi::GstMeta,
        pub(super) original: Option<gst::Buffer>,
        pub(super) caps: Option<gst::Caps>,
        pub(super) transforms: Vec<super::RecordedTransform>,
    }

    pub(super) fn original_buffer_meta_api_get_type() -> glib::Type {
        static TYPE: LazyLock<glib::Type> = LazyLock::new(|| unsafe {
            let t = from_glib(gst::ffi::gst_meta_api_type_register(
                c"GstOriginalBufferMetaAPI".as_ptr() as *const _,
                [
                    gst_video::ffi::GST_META_TAG_VIDEO_STR.as_ptr() as *const std::ffi::c_char,
                    gst_video::ffi::GST_META_TAG_VIDEO_SIZE_STR.as_ptr() as *const std::ffi::c_char,
                    gst_video::ffi::GST_META_TAG_VIDEO_ORIENTATION_STR.as_ptr()
                        as *const std::ffi::c_char,
                    ptr::null::<std::ffi::c_char>(),
                ]
                .as_ptr() as *mut *const _,
            ));

            assert_ne!(t, glib::Type::INVALID);

            t
        });

        *TYPE
    }

    unsafe extern "C" fn original_buffer_meta_init(
        meta: *mut gst::ffi::GstMeta,
        params: glib::ffi::gpointer,
        _buffer: *mut gst::ffi::GstBuffer,
    ) -> glib::ffi::gboolean {
        unsafe {
            assert!(!params.is_null());
            let meta = &mut *(meta as *mut OriginalBufferMeta);
            let params = ptr::read(params as *const OriginalBufferMetaParams);

            let OriginalBufferMetaParams {
                original,
                caps,
                transforms,
            } = params;

            ptr::write(&mut meta.original, Some(original));
            ptr::write(&mut meta.caps, caps);
            ptr::write(&mut meta.transforms, transforms);

            true.into_glib()
        }
    }

    unsafe extern "C" fn original_buffer_meta_free(
        meta: *mut gst::ffi::GstMeta,
        _buffer: *mut gst::ffi::GstBuffer,
    ) {
        unsafe {
            let meta = &mut *(meta as *mut OriginalBufferMeta);
            meta.original = None;
            meta.caps = None;
            meta.transforms = Vec::new();
        }
    }

    unsafe extern "C" fn original_buffer_meta_transform(
        dest: *mut gst::ffi::GstBuffer,
        meta: *mut gst::ffi::GstMeta,
        _buffer: *mut gst::ffi::GstBuffer,
        type_: glib::ffi::GQuark,
        data: glib::ffi::gpointer,
    ) -> glib::ffi::gboolean {
        unsafe {
            let dest = gst::BufferRef::from_mut_ptr(dest);
            let meta = &*(meta as *const OriginalBufferMeta);

            if dest.meta::<super::OriginalBufferMeta>().is_some() {
                return true.into_glib();
            }

            let mut transforms = meta.transforms.clone();

            if type_ == gst_video::ffi::gst_video_meta_transform_scale_get_quark() {
                let data = &*(data as *const gst_video::ffi::GstVideoMetaTransform);
                let transform = super::RecordedTransform::Scale {
                    in_info: from_glib_none(data.in_info),
                    out_info: from_glib_none(data.out_info),
                };
                gst::trace!(
                    CAT,
                    "Storing transform {:?} from {:?} to {:?}",
                    transform,
                    _buffer,
                    dest
                );
                transforms.push(transform);
            } else if type_ == gst_video::ffi::gst_video_meta_transform_matrix_get_quark() {
                let data = &*(data as *const gst_video::ffi::GstVideoMetaTransformMatrix);
                let m = &data.matrix;
                let transform = super::RecordedTransform::Matrix {
                    in_info: from_glib_none(data.in_info),
                    in_rectangle: gst_video::VideoRectangle::new(
                        data.in_rectangle.x,
                        data.in_rectangle.y,
                        data.in_rectangle.w,
                        data.in_rectangle.h,
                    ),
                    out_info: from_glib_none(data.out_info),
                    out_rectangle: gst_video::VideoRectangle::new(
                        data.out_rectangle.x,
                        data.out_rectangle.y,
                        data.out_rectangle.w,
                        data.out_rectangle.h,
                    ),
                    matrix: [[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]],
                };
                gst::trace!(
                    CAT,
                    "Storing transform {:?} from {:?} to {:?}",
                    transform,
                    _buffer,
                    dest
                );
                transforms.push(transform);
            }
            // Any other transform type (e.g. plain gst-copy) carries the
            // recorded list through unchanged.

            // We don't store a ref in the meta if it's self-refencing, but we add it
            // when copying the meta to another buffer.
            super::OriginalBufferMeta::add(
                dest,
                meta.original.as_ref().unwrap().clone(),
                meta.caps.clone(),
                transforms,
            );

            true.into_glib()
        }
    }

    pub(super) fn original_buffer_meta_get_info() -> *const gst::ffi::GstMetaInfo {
        struct MetaInfo(ptr::NonNull<gst::ffi::GstMetaInfo>);
        unsafe impl Send for MetaInfo {}
        unsafe impl Sync for MetaInfo {}

        static META_INFO: LazyLock<MetaInfo> = LazyLock::new(|| unsafe {
            MetaInfo(
                ptr::NonNull::new(gst::ffi::gst_meta_register(
                    original_buffer_meta_api_get_type().into_glib(),
                    c"OriginalBufferMeta".as_ptr() as *const _,
                    mem::size_of::<OriginalBufferMeta>(),
                    Some(original_buffer_meta_init),
                    Some(original_buffer_meta_free),
                    Some(original_buffer_meta_transform),
                ) as *mut gst::ffi::GstMetaInfo)
                .expect("Failed to register meta API"),
            )
        });

        META_INFO.0.as_ptr()
    }
}

#[test]
fn test() {
    gst::init().unwrap();
    let mut b = gst::Buffer::with_size(10).unwrap();
    let caps = gst::Caps::new_empty_simple("video/x-raw");
    let copy = b.copy();
    let m = OriginalBufferMeta::add(b.make_mut(), copy, Some(caps.clone()), Vec::new());
    assert_eq!(m.caps(), caps.as_ref());
    assert_eq!(m.original().clone(), b);
    let b2: gst::Buffer = b.copy_deep().unwrap();
    let m = b.meta::<OriginalBufferMeta>().unwrap();
    assert_eq!(m.caps(), caps.as_ref());
    assert_eq!(m.original(), &b);
    let m = b2.meta::<OriginalBufferMeta>().unwrap();
    assert_eq!(m.caps(), caps.as_ref());
    assert_eq!(m.original(), &b);
    let b3: gst::Buffer = b2.copy_deep().unwrap();
    drop(b2);
    let m = b3.meta::<OriginalBufferMeta>().unwrap();
    assert_eq!(m.caps(), caps.as_ref());
    assert_eq!(m.original(), &b);
}
