// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::mem;
use std::ptr;

use glib_ffi;
use gobject_ffi;
use gst_ffi;

use glib;
use glib::translate::*;
use gst;
use gst::prelude::*;

use gobject_subclass::anyimpl::*;
use gobject_subclass::object::*;

use bin::*;
use element::*;
use object::*;

pub trait PipelineImpl<T: PipelineBase>:
    AnyImpl + ObjectImpl<T> + ElementImpl<T> + BinImpl<T> + Send + Sync + 'static
where
    T::InstanceStructType: PanicPoison,
{
}

any_impl!(PipelineBase, PipelineImpl, PanicPoison);

pub unsafe trait PipelineBase:
    IsA<gst::Element> + IsA<gst::Bin> + IsA<gst::Pipeline> + ObjectType
{
}

pub unsafe trait PipelineClassExt<T: PipelineBase>
where
    T::ImplType: PipelineImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    fn override_vfuncs(&mut self, _: &ClassInitToken) {}
}

glib_wrapper! {
    pub struct Pipeline(Object<ElementInstanceStruct<Pipeline>>):
        [gst::Pipeline => gst_ffi::GstPipeline,
         gst::Bin => gst_ffi::GstBin,
         gst::Element => gst_ffi::GstElement,
         gst::Object => gst_ffi::GstObject,
         gst::ChildProxy => gst_ffi::GstChildProxy];

    match fn {
        get_type => || get_type::<Pipeline>(),
    }
}

unsafe impl<T: IsA<gst::Element> + IsA<gst::Bin> + IsA<gst::Pipeline> + ObjectType> PipelineBase
    for T
{
}
pub type PipelineClass = ClassStruct<Pipeline>;

// FIXME: Boilerplate
unsafe impl PipelineClassExt<Pipeline> for PipelineClass {}
unsafe impl BinClassExt<Pipeline> for PipelineClass {}
unsafe impl ElementClassExt<Pipeline> for PipelineClass {}

unsafe impl Send for Pipeline {}
unsafe impl Sync for Pipeline {}

#[macro_export]
macro_rules! box_pipeline_impl(
    ($name:ident) => {
        box_bin_impl!($name);

        impl<T: PipelineBase> PipelineImpl<T> for Box<$name<T>>
        where
            T::InstanceStructType: PanicPoison
        {
        }
    };
);
box_pipeline_impl!(PipelineImpl);

impl ObjectType for Pipeline {
    const NAME: &'static str = "RsPipeline";
    type GlibType = gst_ffi::GstPipeline;
    type GlibClassType = gst_ffi::GstPipelineClass;
    type ImplType = Box<PipelineImpl<Self>>;
    type InstanceStructType = ElementInstanceStruct<Self>;

    fn glib_type() -> glib::Type {
        unsafe { from_glib(gst_ffi::gst_pipeline_get_type()) }
    }

    fn class_init(token: &ClassInitToken, klass: &mut PipelineClass) {
        ElementClassExt::override_vfuncs(klass, token);
        BinClassExt::override_vfuncs(klass, token);
        PipelineClassExt::override_vfuncs(klass, token);
    }

    object_type_fns!();
}
