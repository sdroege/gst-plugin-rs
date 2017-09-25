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

pub trait BaseSinkImpl<T: BaseSink>
    : mopa::Any + ObjectImpl<T> + ElementImpl<T> + Send + Sync + 'static {
    fn start(&self, _element: &T) -> bool {
        true
    }
    fn stop(&self, _element: &T) -> bool {
        true
    }
    fn render(&self, element: &T, buffer: &gst::BufferRef) -> gst::FlowReturn;
    fn query(&self, element: &T, query: &mut gst::QueryRef) -> bool {
        element.parent_query(query)
    }
    fn event(&self, element: &T, event: &gst::Event) -> bool {
        element.parent_event(event)
    }
}

mopafy_object_impl!(BaseSink, BaseSinkImpl);

pub unsafe trait BaseSink
    : IsA<gst::Element> + IsA<gst_base::BaseSink> + ObjectType {
    fn parent_query(&self, query: &mut gst::QueryRef) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSinkClass;
            (*parent_klass)
                .query
                .map(|f| from_glib(f(self.to_glib_none().0, query.as_mut_ptr())))
                .unwrap_or(false)
        }
    }

    fn parent_event(&self, event: &gst::Event) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSinkClass;
            (*parent_klass)
                .event
                .map(|f| {
                    from_glib(f(self.to_glib_none().0, event.to_glib_none().0))
                })
                .unwrap_or(false)
        }
    }
}

pub unsafe trait BaseSinkClass<T: BaseSink>
where
    T::ImplType: BaseSinkImpl<T>,
{
    fn override_vfuncs(&mut self, _: &ClassInitToken) {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gst_base_ffi::GstBaseSinkClass);
            klass.start = Some(base_sink_start::<T>);
            klass.stop = Some(base_sink_stop::<T>);
            klass.render = Some(base_sink_render::<T>);
            klass.query = Some(base_sink_query::<T>);
            klass.event = Some(base_sink_event::<T>);
        }
    }
}

glib_wrapper! {
    pub struct RsBaseSink(Object<InstanceStruct<RsBaseSink>>): [gst_base::BaseSink => gst_base_ffi::GstBaseSink,
                                                                gst::Element => gst_ffi::GstElement,
                                                                gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || get_type::<RsBaseSink>(),
    }
}

unsafe impl<T: IsA<gst::Element> + IsA<gst_base::BaseSink> + ObjectType> BaseSink for T {}
pub type RsBaseSinkClass = ClassStruct<RsBaseSink>;

// FIXME: Boilerplate
unsafe impl BaseSinkClass<RsBaseSink> for RsBaseSinkClass {}
unsafe impl ElementClass<RsBaseSink> for RsBaseSinkClass {}

#[macro_export]
macro_rules! box_base_sink_impl(
    ($name:ident) => {
        box_element_impl!($name);

        impl<T: BaseSink> BaseSinkImpl<T> for Box<$name<T>> {
            fn start(&self, element: &T) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.start(element)
            }

            fn stop(&self, element: &T) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.stop(element)
            }

            fn render(&self, element: &T, buffer: &gst::BufferRef) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.render(element, buffer)
            }

            fn query(&self, element: &T, query: &mut gst::QueryRef) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.query(element, query)
            }
            fn event(&self, element: &T, event: &gst::Event) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.event(element, event)
            }
        }
    };
);

box_base_sink_impl!(BaseSinkImpl);

impl ObjectType for RsBaseSink {
    const NAME: &'static str = "RsBaseSink";
    type GlibType = gst_base_ffi::GstBaseSink;
    type GlibClassType = gst_base_ffi::GstBaseSinkClass;
    type ImplType = Box<BaseSinkImpl<Self>>;

    fn glib_type() -> glib::Type {
        unsafe { from_glib(gst_base_ffi::gst_base_sink_get_type()) }
    }

    fn class_init(token: &ClassInitToken, klass: &mut RsBaseSinkClass) {
        ElementClass::override_vfuncs(klass, token);
        BaseSinkClass::override_vfuncs(klass, token);
    }

    object_type_fns!();
}

unsafe extern "C" fn base_sink_start<T: BaseSink>(
    ptr: *mut gst_base_ffi::GstBaseSink,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSinkImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, { imp.start(&wrap) }).to_glib()
}

unsafe extern "C" fn base_sink_stop<T: BaseSink>(
    ptr: *mut gst_base_ffi::GstBaseSink,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSinkImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, { imp.stop(&wrap) }).to_glib()
}

unsafe extern "C" fn base_sink_render<T: BaseSink>(
    ptr: *mut gst_base_ffi::GstBaseSink,
    buffer: *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: BaseSinkImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;
    let buffer = gst::BufferRef::from_ptr(buffer);

    panic_to_error!(&wrap, &element.panicked, gst::FlowReturn::Error, {
        imp.render(&wrap, buffer)
    }).to_glib()
}

unsafe extern "C" fn base_sink_query<T: BaseSink>(
    ptr: *mut gst_base_ffi::GstBaseSink,
    query_ptr: *mut gst_ffi::GstQuery,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSinkImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;
    let query = gst::QueryRef::from_mut_ptr(query_ptr);

    panic_to_error!(&wrap, &element.panicked, false, { imp.query(&wrap, query) }).to_glib()
}

unsafe extern "C" fn base_sink_event<T: BaseSink>(
    ptr: *mut gst_base_ffi::GstBaseSink,
    event_ptr: *mut gst_ffi::GstEvent,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSinkImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, {
        imp.event(&wrap, &from_glib_none(event_ptr))
    }).to_glib()
}
