// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ffi::CString;
use std::ptr;
use std::mem;
use std::sync::atomic::AtomicBool;
use mopa;

use glib_ffi;
use gobject_ffi;
use gst_ffi;

use glib;
use glib::translate::*;
use gst;
use gst::prelude::*;

pub trait ElementImpl: mopa::Any + Send + Sync + 'static {
    fn change_state(
        &self,
        element: &RsElement,
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

pub unsafe trait ElementClass {
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
}

glib_wrapper! {
    pub struct RsElement(Object<ffi::RsElement>): [gst::Element => gst_ffi::GstElement,
                                                   gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || ffi::rs_element_get_type(),
    }
}

impl RsElement {
    pub fn get_impl(&self) -> &ElementImpl {
        unsafe {
            let stash = self.to_glib_none();
            let ptr: *const ffi::RsElement = stash.0;
            (*ptr).get_impl()
        }
    }
}

unsafe impl<T: IsA<gst::Element>> Element for T {}
unsafe impl ElementClass for RsElementClass {}

struct ElementData {
    class_init: Box<Fn(&mut RsElementClass) + Send + 'static>,
    init: Box<Fn(&RsElement) -> Box<ElementImpl> + Send + Sync + 'static>,
}

pub mod ffi {
    use super::*;
    use super::RsElement as RsElementWrapper;

    #[repr(C)]
    pub struct RsElement {
        parent: gst_ffi::GstElement,
        imp: *const Box<ElementImpl>,
        panicked: AtomicBool,
    }

    impl RsElement {
        pub fn get_impl(&self) -> &ElementImpl {
            unsafe {
                assert!(!self.imp.is_null());
                &*(*self.imp)
            }
        }
    }

    #[repr(C)]
    pub struct RsElementClass {
        parent_class: gst_ffi::GstElementClass,
        element_data: *const ElementData,
    }

    pub unsafe fn rs_element_get_type() -> glib_ffi::GType {
        use std::sync::{Once, ONCE_INIT};

        static mut TYPE: glib_ffi::GType = gobject_ffi::G_TYPE_INVALID;
        static ONCE: Once = ONCE_INIT;

        ONCE.call_once(|| {
            let type_info = gobject_ffi::GTypeInfo {
                class_size: mem::size_of::<RsElementClass>() as u16,
                base_init: None,
                base_finalize: None,
                class_init: Some(element_class_init),
                class_finalize: None,
                class_data: ptr::null_mut(),
                instance_size: mem::size_of::<RsElement>() as u16,
                n_preallocs: 0,
                instance_init: None,
                value_table: ptr::null(),
            };

            let type_name = {
                let mut idx = 0;

                loop {
                    let type_name = CString::new(format!("RsElement-{}", idx)).unwrap();
                    if gobject_ffi::g_type_from_name(type_name.as_ptr()) ==
                        gobject_ffi::G_TYPE_INVALID
                    {
                        break type_name;
                    }
                    idx += 1;
                }
            };

            TYPE = gobject_ffi::g_type_register_static(
                gst_ffi::gst_element_get_type(),
                type_name.as_ptr(),
                &type_info,
                gobject_ffi::GTypeFlags::empty(),
            );
        });

        TYPE
    }

    unsafe extern "C" fn element_finalize(obj: *mut gobject_ffi::GObject) {
        callback_guard!();
        let element = &mut *(obj as *mut RsElement);

        drop(Box::from_raw(element.imp as *mut Box<ElementImpl>));
        element.imp = ptr::null_mut();

        let klass = *(obj as *const glib_ffi::gpointer);
        let parent_klass = gobject_ffi::g_type_class_peek_parent(klass);
        let parent_klass = &*(gobject_ffi::g_type_class_peek_parent(parent_klass) as
            *const gobject_ffi::GObjectClass);
        parent_klass.finalize.map(|f| f(obj));
    }

    unsafe extern "C" fn element_sub_class_init(
        klass: glib_ffi::gpointer,
        klass_data: glib_ffi::gpointer,
    ) {
        callback_guard!();
        let element_data = &*(klass_data as *const ElementData);

        {
            let klass = &mut *(klass as *mut RsElementClass);

            klass.element_data = element_data;

            (element_data.class_init)(klass);
        }
    }

    unsafe extern "C" fn element_class_init(
        klass: glib_ffi::gpointer,
        _klass_data: glib_ffi::gpointer,
    ) {
        callback_guard!();
        {
            let gobject_klass = &mut *(klass as *mut gobject_ffi::GObjectClass);

            gobject_klass.finalize = Some(element_finalize);
        }

        {
            let element_klass = &mut *(klass as *mut gst_ffi::GstElementClass);

            element_klass.change_state = Some(element_change_state);
        }
    }

    unsafe extern "C" fn element_change_state(
        ptr: *mut gst_ffi::GstElement,
        transition: gst_ffi::GstStateChange,
    ) -> gst_ffi::GstStateChangeReturn {
        callback_guard!();
        let element = &*(ptr as *mut RsElement);
        let wrap: RsElementWrapper = from_glib_borrow(ptr as *mut RsElement);
        let imp = &*element.imp;

        panic_to_error2!(&wrap, &element.panicked, gst::StateChangeReturn::Failure, {
            imp.change_state(&wrap, from_glib(transition))
        }).to_glib()
    }

    unsafe extern "C" fn element_sub_init(
        instance: *mut gobject_ffi::GTypeInstance,
        klass: glib_ffi::gpointer,
    ) {
        callback_guard!();
        let element = &mut *(instance as *mut RsElement);
        let wrap: RsElementWrapper = from_glib_borrow(instance as *mut RsElement);
        let klass = &*(klass as *const RsElementClass);
        let element_data = &*klass.element_data;

        let imp = (element_data.init)(&wrap);
        element.imp = Box::into_raw(Box::new(imp));
    }

    pub fn element_register<F, G>(
        plugin: &gst::Plugin,
        name: &str,
        rank: u32,
        class_init: F,
        init: G,
    ) where
        F: Fn(&mut RsElementClass) + Send + 'static,
        G: Fn(&RsElementWrapper) -> Box<ElementImpl> + Send + Sync + 'static,
    {
        unsafe {
            let parent_type = rs_element_get_type();
            let type_name = format!("RsElement-{}", name);

            let element_data = ElementData {
                class_init: Box::new(class_init),
                init: Box::new(init),
            };
            let element_data = Box::into_raw(Box::new(element_data)) as glib_ffi::gpointer;

            let type_info = gobject_ffi::GTypeInfo {
                class_size: mem::size_of::<RsElementClass>() as u16,
                base_init: None,
                base_finalize: None,
                class_init: Some(element_sub_class_init),
                class_finalize: None,
                class_data: element_data,
                instance_size: mem::size_of::<RsElement>() as u16,
                n_preallocs: 0,
                instance_init: Some(element_sub_init),
                value_table: ptr::null(),
            };

            let type_ = gobject_ffi::g_type_register_static(
                parent_type,
                type_name.to_glib_none().0,
                &type_info,
                gobject_ffi::GTypeFlags::empty(),
            );

            gst::Element::register(plugin, &name, rank, from_glib(type_));
        }
    }
}

pub use self::ffi::RsElementClass;
pub use self::ffi::element_register;
