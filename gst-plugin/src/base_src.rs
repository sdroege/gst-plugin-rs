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

pub trait BaseSrcImpl<T: BaseSrc>
    : mopa::Any + ObjectImpl<T> + ElementImpl<T> + Send + Sync + 'static {
    fn start(&self, _element: &T) -> bool {
        true
    }
    fn stop(&self, _element: &T) -> bool {
        true
    }
    fn is_seekable(&self, _element: &T) -> bool {
        false
    }
    fn get_size(&self, _element: &T) -> Option<u64> {
        None
    }
    fn fill(
        &self,
        element: &T,
        offset: u64,
        length: u32,
        buffer: &mut gst::BufferRef,
    ) -> gst::FlowReturn;
    fn do_seek(&self, element: &T, segment: &mut gst::Segment) -> bool {
        element.parent_do_seek(segment)
    }
    fn query(&self, element: &T, query: &mut gst::QueryRef) -> bool {
        element.parent_query(query)
    }
    fn event(&self, element: &T, event: &gst::Event) -> bool {
        element.parent_event(event)
    }
}

mopafy_object_impl!(BaseSrc, BaseSrcImpl);

pub unsafe trait BaseSrc
    : IsA<gst::Element> + IsA<gst_base::BaseSrc> + ObjectType {
    fn parent_do_seek(&self, segment: &mut gst::Segment) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSrcClass;
            (*parent_klass)
                .do_seek
                .map(|f| {
                    from_glib(f(self.to_glib_none().0, segment.to_glib_none_mut().0))
                })
                .unwrap_or(false)
        }
    }

    fn parent_query(&self, query: &mut gst::QueryRef) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSrcClass;
            (*parent_klass)
                .query
                .map(|f| from_glib(f(self.to_glib_none().0, query.as_mut_ptr())))
                .unwrap_or(false)
        }
    }

    fn parent_event(&self, event: &gst::Event) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSrcClass;
            (*parent_klass)
                .event
                .map(|f| {
                    from_glib(f(self.to_glib_none().0, event.to_glib_none().0))
                })
                .unwrap_or(false)
        }
    }
}

pub unsafe trait BaseSrcClass<T: BaseSrc>
where
    T::ImplType: BaseSrcImpl<T>,
{
    fn override_vfuncs(&mut self, _: &ClassInitToken) {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gst_base_ffi::GstBaseSrcClass);
            klass.start = Some(base_src_start::<T>);
            klass.stop = Some(base_src_stop::<T>);
            klass.is_seekable = Some(base_src_is_seekable::<T>);
            klass.get_size = Some(base_src_get_size::<T>);
            klass.fill = Some(base_src_fill::<T>);
            klass.do_seek = Some(base_src_do_seek::<T>);
            klass.query = Some(base_src_query::<T>);
            klass.event = Some(base_src_event::<T>);
        }
    }
}

glib_wrapper! {
    pub struct RsBaseSrc(Object<InstanceStruct<RsBaseSrc>>): [gst_base::BaseSrc => gst_base_ffi::GstBaseSrc,
                                                              gst::Element => gst_ffi::GstElement,
                                                              gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || get_type::<RsBaseSrc>(),
    }
}

unsafe impl<T: IsA<gst::Element> + IsA<gst_base::BaseSrc> + ObjectType> BaseSrc for T {}
pub type RsBaseSrcClass = ClassStruct<RsBaseSrc>;

// FIXME: Boilerplate
unsafe impl BaseSrcClass<RsBaseSrc> for RsBaseSrcClass {}
unsafe impl ElementClass<RsBaseSrc> for RsBaseSrcClass {}

#[macro_export]
macro_rules! box_base_src_impl(
    ($name:ident) => {
        box_element_impl!($name);

        impl<T: BaseSrc> BaseSrcImpl<T> for Box<$name<T>> {
            fn start(&self, element: &T) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.start(element)
            }

            fn stop(&self, element: &T) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.stop(element)
            }

            fn is_seekable(&self, element: &T) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.is_seekable(element)
            }

            fn get_size(&self, element: &T) -> Option<u64> {
                let imp: &$name<T> = self.as_ref();
                imp.get_size(element)
            }

            fn fill(
                &self,
                element: &T,
                offset: u64,
                length: u32,
                buffer: &mut gst::BufferRef,
            ) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.fill(element, offset, length, buffer)
            }

            fn do_seek(&self, element: &T, segment: &mut gst::Segment) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.do_seek(element, segment)
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
box_base_src_impl!(BaseSrcImpl);

impl ObjectType for RsBaseSrc {
    const NAME: &'static str = "RsBaseSrc";
    type GlibType = gst_base_ffi::GstBaseSrc;
    type GlibClassType = gst_base_ffi::GstBaseSrcClass;
    type ImplType = Box<BaseSrcImpl<Self>>;

    fn glib_type() -> glib::Type {
        unsafe { from_glib(gst_base_ffi::gst_base_src_get_type()) }
    }

    fn class_init(token: &ClassInitToken, klass: &mut RsBaseSrcClass) {
        ElementClass::override_vfuncs(klass, token);
        BaseSrcClass::override_vfuncs(klass, token);
    }

    object_type_fns!();
}

unsafe extern "C" fn base_src_start<T: BaseSrc>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, { imp.start(&wrap) }).to_glib()
}

unsafe extern "C" fn base_src_stop<T: BaseSrc>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, { imp.stop(&wrap) }).to_glib()
}

unsafe extern "C" fn base_src_is_seekable<T: BaseSrc>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, { imp.is_seekable(&wrap) }).to_glib()
}

unsafe extern "C" fn base_src_get_size<T: BaseSrc>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    size: *mut u64,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, {
        match imp.get_size(&wrap) {
            Some(s) => {
                *size = s;
                true
            }
            None => false,
        }
    }).to_glib()
}

unsafe extern "C" fn base_src_fill<T: BaseSrc>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    offset: u64,
    length: u32,
    buffer: *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: BaseSrcImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;
    let buffer = gst::BufferRef::from_mut_ptr(buffer);

    panic_to_error!(&wrap, &element.panicked, gst::FlowReturn::Error, {
        imp.fill(&wrap, offset, length, buffer)
    }).to_glib()
}

unsafe extern "C" fn base_src_do_seek<T: BaseSrc>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    segment: *mut gst_ffi::GstSegment,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, {
        imp.do_seek(&wrap, &mut from_glib_borrow(segment))
    }).to_glib()
}

unsafe extern "C" fn base_src_query<T: BaseSrc>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    query_ptr: *mut gst_ffi::GstQuery,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;
    let query = gst::QueryRef::from_mut_ptr(query_ptr);

    panic_to_error!(&wrap, &element.panicked, false, { imp.query(&wrap, query) }).to_glib()
}

unsafe extern "C" fn base_src_event<T: BaseSrc>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    event_ptr: *mut gst_ffi::GstEvent,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
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
