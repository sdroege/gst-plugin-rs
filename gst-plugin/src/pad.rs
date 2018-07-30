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

pub trait PadImpl<T: PadBase>: AnyImpl + ObjectImpl<T> + Send + Sync + 'static {
    fn linked(&self, pad: &T, peer: &gst::Pad) {
        pad.parent_linked(peer)
    }

    fn unlinked(&self, pad: &T, peer: &gst::Pad) {
        pad.parent_unlinked(peer)
    }
}

any_impl!(PadBase, PadImpl);

pub unsafe trait PadBase: IsA<gst::Pad> + ObjectType {
    fn parent_linked(&self, peer: &gst::Pad) {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_ffi::GstPadClass;
            (*parent_klass)
                .linked
                .map(|f| f(self.to_glib_none().0, peer.to_glib_none().0))
                .unwrap_or(())
        }
    }

    fn parent_unlinked(&self, peer: &gst::Pad) {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_ffi::GstPadClass;
            (*parent_klass)
                .unlinked
                .map(|f| f(self.to_glib_none().0, peer.to_glib_none().0))
                .unwrap_or(())
        }
    }
}

pub unsafe trait PadClassExt<T: PadBase>
where
    T::ImplType: PadImpl<T>,
{
    fn override_vfuncs(&mut self, _: &ClassInitToken) {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gst_ffi::GstPadClass);
            klass.linked = Some(pad_linked::<T>);
            klass.unlinked = Some(pad_unlinked::<T>);
        }
    }
}

glib_wrapper! {
    pub struct Pad(Object<InstanceStruct<Pad>>):
        [gst::Pad => gst_ffi::GstPad,
         gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || get_type::<Pad>(),
    }
}

unsafe impl<T: IsA<gst::Pad> + ObjectType> PadBase for T {}
pub type PadClass = ClassStruct<Pad>;

// FIXME: Boilerplate
unsafe impl PadClassExt<Pad> for PadClass {}
unsafe impl ObjectClassExt<Pad> for PadClass {}

unsafe impl Send for Pad {}
unsafe impl Sync for Pad {}

#[macro_export]
macro_rules! box_pad_impl(
    ($name:ident) => {
        box_object_impl!($name);

        impl<T: PadBase> PadImpl<T> for Box<$name<T>>
        {
            fn linked(&self, pad: &T, peer: &gst::Pad) {
                let imp: &$name<T> = self.as_ref();
                imp.linked(pad, peer)
            }

            fn unlinked(&self, pad: &T, peer: &gst::Pad) {
                let imp: &$name<T> = self.as_ref();
                imp.unlinked(pad, peer)
            }
        }
    };
);
box_pad_impl!(PadImpl);

impl ObjectType for Pad {
    const NAME: &'static str = "RsPad";
    type ParentType = gst::Pad;
    type ImplType = Box<PadImpl<Self>>;
    type InstanceStructType = InstanceStruct<Self>;

    fn class_init(token: &ClassInitToken, klass: &mut PadClass) {
        PadClassExt::override_vfuncs(klass, token);
    }

    object_type_fns!();
}

unsafe extern "C" fn pad_linked<T: PadBase>(ptr: *mut gst_ffi::GstPad, peer: *mut gst_ffi::GstPad)
where
    T::ImplType: PadImpl<T>,
{
    floating_reference_guard!(ptr);
    let pad = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = pad.get_impl();

    imp.linked(&wrap, &from_glib_borrow(peer))
}

unsafe extern "C" fn pad_unlinked<T: PadBase>(ptr: *mut gst_ffi::GstPad, peer: *mut gst_ffi::GstPad)
where
    T::ImplType: PadImpl<T>,
{
    floating_reference_guard!(ptr);
    let pad = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = pad.get_impl();

    imp.unlinked(&wrap, &from_glib_borrow(peer))
}
