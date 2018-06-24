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

use element::*;
use object::*;

pub trait BinImpl<T: BinBase>:
    AnyImpl + ObjectImpl<T> + ElementImpl<T> + Send + Sync + 'static
where
    T::InstanceStructType: PanicPoison,
{
    fn add_element(&self, bin: &T, element: &gst::Element) -> bool {
        bin.parent_add_element(element)
    }

    fn remove_element(&self, bin: &T, element: &gst::Element) -> bool {
        bin.parent_remove_element(element)
    }

    fn handle_message(&self, bin: &T, message: gst::Message) {
        bin.parent_handle_message(message)
    }
}

any_impl!(BinBase, BinImpl, PanicPoison);

pub unsafe trait BinBase: IsA<gst::Element> + IsA<gst::Bin> + ObjectType {
    fn parent_add_element(&self, element: &gst::Element) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_ffi::GstBinClass;
            (*parent_klass)
                .add_element
                .map(|f| from_glib(f(self.to_glib_none().0, element.to_glib_none().0)))
                .unwrap_or(false)
        }
    }

    fn parent_remove_element(&self, element: &gst::Element) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_ffi::GstBinClass;
            (*parent_klass)
                .remove_element
                .map(|f| from_glib(f(self.to_glib_none().0, element.to_glib_none().0)))
                .unwrap_or(false)
        }
    }

    fn parent_handle_message(&self, message: gst::Message) {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_ffi::GstBinClass;
            (*parent_klass)
                .handle_message
                .map(move |f| f(self.to_glib_none().0, message.into_ptr()));
        }
    }
}

pub unsafe trait BinClassExt<T: BinBase>
where
    T::ImplType: BinImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    fn override_vfuncs(&mut self, _: &ClassInitToken) {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gst_ffi::GstBinClass);
            klass.add_element = Some(bin_add_element::<T>);
            klass.remove_element = Some(bin_remove_element::<T>);
            klass.handle_message = Some(bin_handle_message::<T>);
        }
    }
}

glib_wrapper! {
    pub struct Bin(Object<ElementInstanceStruct<Bin>>):
        [gst::Bin => gst_ffi::GstBin,
         gst::Element => gst_ffi::GstElement,
         gst::Object => gst_ffi::GstObject,
         gst::ChildProxy => gst_ffi::GstChildProxy];

    match fn {
        get_type => || get_type::<Bin>(),
    }
}

unsafe impl<T: IsA<gst::Element> + IsA<gst::Bin> + ObjectType> BinBase for T {}
pub type BinClass = ClassStruct<Bin>;

// FIXME: Boilerplate
unsafe impl BinClassExt<Bin> for BinClass {}
unsafe impl ElementClassExt<Bin> for BinClass {}
unsafe impl ObjectClassExt<Bin> for BinClass {}

unsafe impl Send for Bin {}
unsafe impl Sync for Bin {}

#[macro_export]
macro_rules! box_bin_impl(
    ($name:ident) => {
        box_element_impl!($name);

        impl<T: BinBase> BinImpl<T> for Box<$name<T>>
        where
            T::InstanceStructType: PanicPoison
        {
            fn add_element(&self, bin: &T, element: &gst::Element) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.add_element(bin, element)
            }

            fn remove_element(&self, bin: &T, element: &gst::Element) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.remove_element(bin, element)
            }

            fn handle_message(&self, bin: &T, message: gst::Message) {
                let imp: &$name<T> = self.as_ref();
                imp.handle_message(bin, message)
            }
        }
    };
);
box_bin_impl!(BinImpl);

impl ObjectType for Bin {
    const NAME: &'static str = "RsBin";
    type ParentType = gst::Bin;
    type ImplType = Box<BinImpl<Self>>;
    type InstanceStructType = ElementInstanceStruct<Self>;

    fn class_init(token: &ClassInitToken, klass: &mut BinClass) {
        ElementClassExt::override_vfuncs(klass, token);
        BinClassExt::override_vfuncs(klass, token);
    }

    object_type_fns!();
}

unsafe extern "C" fn bin_add_element<T: BinBase>(
    ptr: *mut gst_ffi::GstBin,
    element: *mut gst_ffi::GstElement,
) -> glib_ffi::gboolean
where
    T::ImplType: BinImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let bin = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = bin.get_impl();

    panic_to_error!(&wrap, &bin.panicked(), false, {
        imp.add_element(&wrap, &from_glib_none(element))
    }).to_glib()
}

unsafe extern "C" fn bin_remove_element<T: BinBase>(
    ptr: *mut gst_ffi::GstBin,
    element: *mut gst_ffi::GstElement,
) -> glib_ffi::gboolean
where
    T::ImplType: BinImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let bin = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = bin.get_impl();

    panic_to_error!(&wrap, &bin.panicked(), false, {
        imp.remove_element(&wrap, &from_glib_none(element))
    }).to_glib()
}

unsafe extern "C" fn bin_handle_message<T: BinBase>(
    ptr: *mut gst_ffi::GstBin,
    message: *mut gst_ffi::GstMessage,
) where
    T::ImplType: BinImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let bin = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = bin.get_impl();

    panic_to_error!(&wrap, &bin.panicked(), (), {
        imp.handle_message(&wrap, from_glib_full(message))
    });
}
