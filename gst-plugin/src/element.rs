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

pub trait ElementImpl: ObjectImpl + mopa::Any + Send + Sync + 'static {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> gst::StateChangeReturn {
        element.parent_change_state(transition)
    }
}

mopafy!(ElementImpl);

pub unsafe trait Element: IsA<gst::Element> {
    fn parent_change_state(&self, transition: gst::StateChange) -> gst::StateChangeReturn {
        unsafe {
            // Our class
            let klass = *(self.to_glib_none().0 as *const glib_ffi::gpointer);
            // The parent class, RsElement or any other first-level Rust implementation
            let parent_klass = gobject_ffi::g_type_class_peek_parent(klass);
            // The actual parent class as defined in C
            let parent_klass = &*(gobject_ffi::g_type_class_peek_parent(parent_klass) as
                *const gst_ffi::GstElementClass);
            parent_klass
                .change_state
                .map(|f| {
                    from_glib(f(self.to_glib_none().0, transition.to_glib()))
                })
                .unwrap_or(gst::StateChangeReturn::Success)
        }
    }
}

pub unsafe trait ElementClass<T: ObjectType>
where
    T::RsType: IsA<gst::Element>,
    T::ImplType: ElementImpl,
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

impl RsElement {
    pub fn get_impl(&self) -> &ElementImpl {
        unsafe {
            let stash = self.to_glib_none();
            let ptr: *mut InstanceStruct<RsElement> = stash.0;
            (*ptr).get_impl().as_ref()
        }
    }
}

unsafe impl<T: IsA<gst::Element>> Element for T {}
pub type RsElementClass = ClassStruct<RsElement>;

// FIXME: Boilerplate
unsafe impl ElementClass<RsElement> for RsElementClass {}
unsafe impl ElementClass<RsElement> for gst_ffi::GstElementClass {}
unsafe impl ObjectClass for gst_ffi::GstElementClass {}

// FIXME: Boilerplate
impl ObjectImpl for Box<ElementImpl> {
    fn set_property(&self, obj: &glib::Object, id: u32, value: &glib::Value) {
        let imp: &ElementImpl = self.as_ref();
        imp.set_property(obj, id, value);
    }

    fn get_property(&self, obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let imp: &ElementImpl = self.as_ref();
        imp.get_property(obj, id)
    }
}

impl ElementImpl for Box<ElementImpl> {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> gst::StateChangeReturn {
        let imp: &ElementImpl = self.as_ref();
        imp.change_state(element, transition)
    }
}

impl ObjectType for RsElement {
    const NAME: &'static str = "RsElement";
    type GlibType = gst_ffi::GstElement;
    type GlibClassType = gst_ffi::GstElementClass;
    type RsType = RsElement;
    type ImplType = Box<ElementImpl>;

    fn glib_type() -> glib::Type {
        unsafe { from_glib(gst_ffi::gst_element_get_type()) }
    }

    fn class_init(klass: &mut Self::GlibClassType) {
        klass.override_vfuncs();
    }
}

unsafe extern "C" fn element_change_state<T: ObjectType>(
    ptr: *mut gst_ffi::GstElement,
    transition: gst_ffi::GstStateChange,
) -> gst_ffi::GstStateChangeReturn
where
    T::RsType: IsA<gst::Element>,
    T::ImplType: ElementImpl,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: gst::Element = from_glib_borrow(ptr);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, gst::StateChangeReturn::Failure, {
        imp.change_state(&wrap, from_glib(transition))
    }).to_glib()
}
