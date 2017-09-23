// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ptr;
use std::mem;
use mopa;

use glib_ffi;
use gobject_ffi;
use gst_ffi;
use gst_base_ffi;

use glib;
use glib::translate::*;
use gst;
use gst::prelude::*;
use gst_base;
use gst_base::prelude::*;

use object::*;
use element::*;

pub trait BaseSrcImpl
    : mopa::Any + ObjectImpl + ElementImpl + Send + Sync + 'static {
}

mopafy!(BaseSrcImpl);

pub unsafe trait BaseSrc: IsA<gst_base::BaseSrc> {}

pub unsafe trait BaseSrcClass<T: ObjectType>
where
    T::RsType: IsA<gst_base::BaseSrc>,
    T::ImplType: BaseSrcImpl,
{
}

glib_wrapper! {
    pub struct RsBaseSrc(Object<InstanceStruct<RsBaseSrc>>): [gst_base::BaseSrc => gst_base_ffi::GstBaseSrc,
                                                              gst::Element => gst_ffi::GstElement,
                                                              gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || get_type::<RsBaseSrc>(),
    }
}

unsafe impl<T: IsA<gst_base::BaseSrc>> BaseSrc for T {}
pub type RsBaseSrcClass = ClassStruct<RsBaseSrc>;

// FIXME: Boilerplate
unsafe impl BaseSrcClass<RsBaseSrc> for gst_base_ffi::GstBaseSrcClass {}
unsafe impl BaseSrcClass<RsBaseSrc> for RsBaseSrcClass {}
unsafe impl ElementClass<RsBaseSrc> for gst_base_ffi::GstBaseSrcClass {}
unsafe impl ElementClass<RsBaseSrc> for RsBaseSrcClass {}
unsafe impl ObjectClassStruct for gst_base_ffi::GstBaseSrcClass {}

// FIXME: Boilerplate
impl BaseSrcImpl for Box<BaseSrcImpl> {}

// FIXME: Boilerplate
impl ElementImpl for Box<BaseSrcImpl> {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> gst::StateChangeReturn {
        let imp: &BaseSrcImpl = self.as_ref();
        imp.change_state(element, transition)
    }
}

impl ObjectImpl for Box<BaseSrcImpl> {}

impl ObjectType for RsBaseSrc {
    const NAME: &'static str = "RsBaseSrc";
    type GlibType = gst_base_ffi::GstBaseSrc;
    type GlibClassType = gst_base_ffi::GstBaseSrcClass;
    type RsType = RsBaseSrc;
    type ImplType = Box<BaseSrcImpl>;

    fn glib_type() -> glib::Type {
        unsafe { from_glib(gst_base_ffi::gst_base_src_get_type()) }
    }

    fn class_init(klass: &mut Self::GlibClassType) {
        ElementClass::override_vfuncs(klass);
    }
}
