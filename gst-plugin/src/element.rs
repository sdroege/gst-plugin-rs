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

use glib;
use glib::translate::*;
use gst;
use gst::prelude::*;

use object::*;

pub trait ElementImpl<T: Element>
    : ObjectImpl<T> + mopa::Any + Send + Sync + 'static {
    fn change_state(&self, element: &T, transition: gst::StateChange) -> gst::StateChangeReturn {
        element.parent_change_state(transition)
    }
}

mopafy_object_impl!(Element, ElementImpl);

pub unsafe trait Element: IsA<gst::Element> + ObjectType {
    fn parent_change_state(&self, transition: gst::StateChange) -> gst::StateChangeReturn {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_ffi::GstElementClass;
            (*parent_klass)
                .change_state
                .map(|f| {
                    from_glib(f(self.to_glib_none().0, transition.to_glib()))
                })
                .unwrap_or(gst::StateChangeReturn::Success)
        }
    }
}

pub unsafe trait ElementClass<T: Element>
where
    T::ImplType: ElementImpl<T>,
{
    fn add_pad_template(&mut self, pad_template: gst::PadTemplate) {
        unsafe {
            gst_ffi::gst_element_class_add_pad_template(
                self as *const Self as *mut gst_ffi::GstElementClass,
                pad_template.to_glib_full(),
            );
        }
    }

    fn set_metadata(
        &mut self,
        long_name: &str,
        classification: &str,
        description: &str,
        author: &str,
    ) {
        unsafe {
            gst_ffi::gst_element_class_set_metadata(
                self as *const Self as *mut gst_ffi::GstElementClass,
                long_name.to_glib_none().0,
                classification.to_glib_none().0,
                description.to_glib_none().0,
                author.to_glib_none().0,
            );
        }
    }

    fn override_vfuncs(&mut self) {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gst_ffi::GstElementClass);
            klass.change_state = Some(element_change_state::<T>);
        }
    }
}

glib_wrapper! {
    pub struct RsElement(Object<InstanceStruct<RsElement>>): [gst::Element => gst_ffi::GstElement,
                                                              gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || get_type::<RsElement>(),
    }
}

unsafe impl<T: IsA<gst::Element> + ObjectType> Element for T {}
pub type RsElementClass = ClassStruct<RsElement>;

// FIXME: Boilerplate
unsafe impl ElementClass<RsElement> for RsElementClass {}

#[macro_export]
macro_rules! box_element_impl(
    ($name:ident) => {
        box_object_impl!($name);

        impl<T: Element> ElementImpl<T> for Box<$name<T>> {
            fn change_state(
                &self,
                element: &T,
                transition: gst::StateChange,
            ) -> gst::StateChangeReturn {
                let imp: &$name<T> = self.as_ref();
                imp.change_state(element, transition)
            }
        }
    };
);

box_element_impl!(ElementImpl);

impl ObjectType for RsElement {
    const NAME: &'static str = "RsElement";
    type GlibType = gst_ffi::GstElement;
    type GlibClassType = gst_ffi::GstElementClass;
    type ImplType = Box<ElementImpl<Self>>;

    fn glib_type() -> glib::Type {
        unsafe { from_glib(gst_ffi::gst_element_get_type()) }
    }

    fn class_init(klass: &mut RsElementClass) {
        klass.override_vfuncs();
    }

    object_type_fns!();
}

unsafe extern "C" fn element_change_state<T: Element>(
    ptr: *mut gst_ffi::GstElement,
    transition: gst_ffi::GstStateChange,
) -> gst_ffi::GstStateChangeReturn
where
    T::ImplType: ElementImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, gst::StateChangeReturn::Failure, {
        imp.change_state(&wrap, from_glib(transition))
    }).to_glib()
}
