// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
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

use pad::*;

pub trait GhostPadImpl<T: GhostPadBase>:
    AnyImpl + ObjectImpl<T> + PadImpl<T> + Send + Sync + 'static
{
}

any_impl!(GhostPadBase, GhostPadImpl);

pub unsafe trait GhostPadBase: IsA<gst::GhostPad> + IsA<gst::Pad> + ObjectType {}

pub unsafe trait GhostPadClassExt<T: GhostPadBase>
where
    T::ImplType: GhostPadImpl<T>,
{
    fn override_vfuncs(&mut self, _: &ClassInitToken) {}
}

glib_wrapper! {
    pub struct GhostPad(Object<InstanceStruct<GhostPad>>):
        [gst::GhostPad => gst_ffi::GstGhostPad,
         gst::ProxyPad => gst_ffi::GstProxyPad,
         gst::Pad => gst_ffi::GstPad,
         gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || get_type::<GhostPad>(),
    }
}

unsafe impl<T: IsA<gst::GhostPad> + IsA<gst::Pad> + ObjectType> GhostPadBase for T {}
pub type GhostPadClass = ClassStruct<GhostPad>;

// FIXME: Boilerplate
unsafe impl GhostPadClassExt<GhostPad> for GhostPadClass {}
unsafe impl PadClassExt<GhostPad> for GhostPadClass {}
unsafe impl ObjectClassExt<GhostPad> for GhostPadClass {}

unsafe impl Send for GhostPad {}
unsafe impl Sync for GhostPad {}

#[macro_export]
macro_rules! box_ghost_pad_impl(
    ($name:ident) => {
        box_pad_impl!($name);

        impl<T: GhostPadBase> GhostPadImpl<T> for Box<$name<T>>
        {
        }
    };
);
box_ghost_pad_impl!(GhostPadImpl);

impl ObjectType for GhostPad {
    const NAME: &'static str = "RsGhostPad";
    type ParentType = gst::GhostPad;
    type ImplType = Box<GhostPadImpl<Self>>;
    type InstanceStructType = InstanceStruct<Self>;

    fn class_init(token: &ClassInitToken, klass: &mut GhostPadClass) {
        PadClassExt::override_vfuncs(klass, token);
        GhostPadClassExt::override_vfuncs(klass, token);
    }

    object_type_fns!();
}
