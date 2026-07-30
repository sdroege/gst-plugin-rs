// GStreamer multi mixer split meta
//
// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2022-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// ---------------------------------------------------------------------------
// Internal GstMeta used by the N:M multi mixer element to tell the splitter
// element how to split up the audio buffers and which channels go with which
// output.
// ---------------------------------------------------------------------------

//#![allow(clippy::non_send_fields_in_send_ty)]

use gst::prelude::*;

use std::fmt;
use std::mem;

// Rust type for the custom split meta.
#[repr(transparent)]
pub(crate) struct SplitMeta(imp::SplitMeta);

// Metas must be Send+Sync.
unsafe impl Send for SplitMeta {}
unsafe impl Sync for SplitMeta {}

#[derive(Debug)]
pub(crate) struct OutputChannelSplit {
    pub(crate) output_num: u32,
    pub(crate) channel_offset: usize,
    pub(crate) n_channels: usize,
}

impl OutputChannelSplit {
    pub(crate) fn new(output_num: u32, channel_offset: usize, n_channels: usize) -> Self {
        OutputChannelSplit {
            output_num,
            channel_offset,
            n_channels,
        }
    }
}

impl SplitMeta {
    // Add a new split meta to the buffer
    pub fn add(
        buffer: &mut gst::BufferRef,
        output_split: Vec<OutputChannelSplit>,
    ) -> gst::MetaRefMut<'_, Self, gst::meta::Standalone> {
        unsafe {
            // Manually dropping because gst_buffer_add_meta() takes ownership of the
            // content of the struct.
            let mut params = mem::ManuallyDrop::new(imp::SplitMetaParams { output_split });

            // The label is passed through via the params to split_meta_init().
            let meta = gst::ffi::gst_buffer_add_meta(
                buffer.as_mut_ptr(),
                imp::split_meta_get_info(),
                &mut *params as *mut imp::SplitMetaParams as glib::ffi::gpointer,
            ) as *mut imp::SplitMeta;

            Self::from_mut_ptr(buffer, meta)
        }
    }

    // Retrieve the stored split info
    pub fn output_split(&self) -> &[OutputChannelSplit] {
        self.0.output_split.as_slice()
    }
}

// Trait to allow using the gst::Buffer API with this meta
unsafe impl MetaAPI for SplitMeta {
    type GstType = imp::SplitMeta;

    fn meta_api() -> glib::Type {
        imp::split_meta_api_get_type()
    }
}

impl fmt::Debug for SplitMeta {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SplitMeta")
            .field("output_split", &self.output_split())
            .finish()
    }
}

// Actual (unsafe) implementation of the meta
mod imp {
    use super::OutputChannelSplit;
    use glib::translate::*;
    use std::mem;
    use std::ptr;
    use std::sync::LazyLock;

    pub(super) struct SplitMetaParams {
        pub output_split: Vec<OutputChannelSplit>,
    }

    // This is the C type that is actually stored as meta inside the buffers.
    #[repr(C)]
    pub struct SplitMeta {
        parent: gst::ffi::GstMeta,
        pub(super) output_split: Vec<OutputChannelSplit>,
    }

    // Function to register the meta API and get a type back.
    pub(super) fn split_meta_api_get_type() -> glib::Type {
        static TYPE: LazyLock<glib::Type> = LazyLock::new(|| unsafe {
            let t = from_glib(gst::ffi::gst_meta_api_type_register(
                c"AudioMultiMixerSplitMetaAPI".as_ptr() as *const _,
                // We provide no tags here as our meta is just internal
                [ptr::null::<std::os::raw::c_char>()].as_ptr() as *mut *const _,
            ));

            assert_ne!(t, glib::Type::INVALID);

            t
        });

        *TYPE
    }

    // Initialization function for our meta. This needs to ensure all fields
    // are correctly initialized. They will contain random memory before.
    unsafe extern "C" fn split_meta_init(
        meta: *mut gst::ffi::GstMeta,
        params: glib::ffi::gpointer,
        _buffer: *mut gst::ffi::GstBuffer,
    ) -> glib::ffi::gboolean {
        unsafe {
            assert!(!params.is_null());

            let meta = &mut *(meta as *mut SplitMeta);
            let params = ptr::read(params as *const SplitMetaParams);

            // Need to initialize all our fields correctly here.
            ptr::write(&mut meta.output_split, params.output_split);

            true.into_glib()
        }
    }

    // Free function for our meta. This needs to free/drop all memory we allocated.
    unsafe extern "C" fn split_meta_free(
        meta: *mut gst::ffi::GstMeta,
        _buffer: *mut gst::ffi::GstBuffer,
    ) {
        unsafe {
            let meta = &mut *(meta as *mut SplitMeta);

            // Need to free/drop all our fields here.
            ptr::drop_in_place(&mut meta.output_split);
        }
    }

    unsafe extern "C" fn split_meta_transform(
        _dest: *mut gst::ffi::GstBuffer,
        _meta: *mut gst::ffi::GstMeta,
        _buffer: *mut gst::ffi::GstBuffer,
        _type_: glib::ffi::GQuark,
        _data: glib::ffi::gpointer,
    ) -> glib::ffi::gboolean {
        // We just lie to avoid debug log warnings warnings.
        // There's no need to copy/transform this internal meta.
        true.into_glib()
    }

    // Register the meta itself with its functions.
    pub(super) fn split_meta_get_info() -> *const gst::ffi::GstMetaInfo {
        struct MetaInfo(ptr::NonNull<gst::ffi::GstMetaInfo>);
        unsafe impl Send for MetaInfo {}
        unsafe impl Sync for MetaInfo {}

        static META_INFO: LazyLock<MetaInfo> = LazyLock::new(|| unsafe {
            MetaInfo(
                ptr::NonNull::new(gst::ffi::gst_meta_register(
                    split_meta_api_get_type().into_glib(),
                    c"AudioMultiMixerSplitMeta".as_ptr() as *const _,
                    mem::size_of::<SplitMeta>(),
                    Some(split_meta_init),
                    Some(split_meta_free),
                    Some(split_meta_transform),
                ) as *mut gst::ffi::GstMetaInfo)
                .expect("Failed to register meta API"),
            )
        });

        META_INFO.0.as_ptr()
    }
}
