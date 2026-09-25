// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(unused_doc_comments)]

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use llama_cpp_2::mtmd::MtmdBitmap;

pub mod imp;

glib::wrapper! {
    pub struct LlamaBaseTransform(ObjectSubclass<imp::LlamaBaseTransform>) @extends gst::Element, gst::Object;
}

/// Trait to implement in `LlamaBaseTransform` subclasses.
pub trait LlamaBaseTransformImpl:
    ObjectSubclass<Type: IsA<LlamaBaseTransform>> + ElementImpl
{
    /// Set to `true` if a multimodal projection model is required.
    ///
    /// If set and `mmproj_path()` returns `None` then the `NULL` -> `READY` state change will
    /// fail.
    const REQUIRE_MMPROJ: bool = false;

    /// Called for every input buffer to extract the user content.
    ///
    /// By default the buffer is interpreted as UTF-8 encoded text.
    fn transform_buffer(
        &self,
        buf: &gst::BufferRef,
    ) -> Result<(String, Option<MtmdBitmap>), gst::FlowError> {
        self.parent_transform_buffer(buf)
    }

    /// Called to process the rendered prompt and generate the assistant output.
    fn transform_prompt(
        &self,
        model: &mut imp::ModelState,
        messages: &[imp::Message],
        prompt: &str,
    ) -> Result<String, gst::FlowError>;

    /// Called on events received on the sink pad.
    ///
    /// By default this clears the conversation history on a flush and forwards all other events.
    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        self.parent_sink_event(pad, event)
    }

    /// Called on events received on the src pad.
    ///
    /// By default this clears the conversation history on a flush and forwards all other events.
    fn src_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        self.parent_src_event(pad, event)
    }

    /// Returns the path to the multimodal projection model file to load, if any.
    fn mmproj_path(&self) -> Option<String> {
        self.parent_mmproj_path()
    }
}

/// Trait containing extension methods for `LlamaBaseTransformImpl`, specifically methods for
/// chaining up to the parent implementation of virtual methods.
pub trait LlamaBaseTransformImplExt: LlamaBaseTransformImpl {
    fn parent_transform_buffer(
        &self,
        buf: &gst::BufferRef,
    ) -> Result<(String, Option<MtmdBitmap>), gst::FlowError> {
        unsafe {
            let data = Self::type_data();
            let parent_class = &*(data.as_ref().parent_class() as *mut Class);
            (parent_class.transform_buffer)(self.obj().unsafe_cast_ref(), buf)
        }
    }

    fn parent_sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        unsafe {
            let data = Self::type_data();
            let parent_class = &*(data.as_ref().parent_class() as *mut Class);
            (parent_class.sink_event)(self.obj().unsafe_cast_ref(), pad, event)
        }
    }

    fn parent_src_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        unsafe {
            let data = Self::type_data();
            let parent_class = &*(data.as_ref().parent_class() as *mut Class);
            (parent_class.src_event)(self.obj().unsafe_cast_ref(), pad, event)
        }
    }

    fn parent_mmproj_path(&self) -> Option<String> {
        unsafe {
            let data = Self::type_data();
            let parent_class = &*(data.as_ref().parent_class() as *mut Class);
            (parent_class.mmproj_path)(self.obj().unsafe_cast_ref())
        }
    }
}

impl<T: LlamaBaseTransformImpl> LlamaBaseTransformImplExt for T {}

/// Function signature of the `transform_buffer` virtual method.
type TransformBufferFn = fn(
    &LlamaBaseTransform,
    buf: &gst::BufferRef,
) -> Result<(String, Option<MtmdBitmap>), gst::FlowError>;

/// Function signature of the `transform_prompt` virtual method.
type TransformPromptFn = fn(
    &LlamaBaseTransform,
    model: &mut imp::ModelState,
    messages: &[imp::Message],
    prompt: &str,
) -> Result<String, gst::FlowError>;

/// Function signature of the sink/src pad event virtual methods.
type PadEventFn = fn(&LlamaBaseTransform, pad: &gst::Pad, event: gst::Event) -> bool;

/// Class struct for `LlamaBaseTransform`.
#[repr(C)]
pub struct Class {
    parent: gst::ffi::GstElementClass,

    transform_buffer: TransformBufferFn,
    transform_prompt: TransformPromptFn,
    sink_event: PadEventFn,
    src_event: PadEventFn,
    mmproj_path: fn(&LlamaBaseTransform) -> Option<String>,
    require_mmproj: bool,
}

unsafe impl ClassStruct for Class {
    type Type = imp::LlamaBaseTransform;
}

impl std::ops::Deref for Class {
    type Target = glib::Class<<<Self as ClassStruct>::Type as ObjectSubclass>::ParentType>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*(&self.parent as *const _ as *const _) }
    }
}

unsafe impl<T: LlamaBaseTransformImpl> IsSubclassable<T> for LlamaBaseTransform {
    fn class_init(class: &mut glib::Class<Self>) {
        Self::parent_class_init::<T>(class);

        let class = class.as_mut();

        class.transform_buffer = |obj, buf| unsafe {
            let imp = obj.unsafe_cast_ref::<T::Type>().imp();
            imp.transform_buffer(buf)
        };

        class.transform_prompt = |obj, model, messages, prompt| unsafe {
            let imp = obj.unsafe_cast_ref::<T::Type>().imp();
            imp.transform_prompt(model, messages, prompt)
        };

        class.sink_event = |obj, pad, event| unsafe {
            let imp = obj.unsafe_cast_ref::<T::Type>().imp();
            imp.sink_event(pad, event)
        };

        class.src_event = |obj, pad, event| unsafe {
            let imp = obj.unsafe_cast_ref::<T::Type>().imp();
            imp.src_event(pad, event)
        };

        class.mmproj_path = |obj| unsafe {
            let imp = obj.unsafe_cast_ref::<T::Type>().imp();
            imp.mmproj_path()
        };

        class.require_mmproj = T::REQUIRE_MMPROJ;
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        // Make the internal base class available in docs
        LlamaBaseTransform::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }

    gst::Element::register(
        Some(plugin),
        "llamacpp-texttransform",
        gst::Rank::NONE,
        crate::texttransform::TextTransform::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "llamacpp-imagetransform",
        gst::Rank::NONE,
        crate::imagetransform::ImageTransform::static_type(),
    )
}
