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
    fn start(&self, _element: &gst_base::BaseSrc) -> bool {
        true
    }
    fn stop(&self, _element: &gst_base::BaseSrc) -> bool {
        true
    }
    fn is_seekable(&self, _element: &gst_base::BaseSrc) -> bool {
        false
    }
    fn get_size(&self, _element: &gst_base::BaseSrc) -> Option<u64> {
        None
    }
    fn fill(
        &self,
        element: &gst_base::BaseSrc,
        offset: u64,
        length: u32,
        buffer: &mut gst::BufferRef,
    ) -> gst::FlowReturn;
    fn do_seek(&self, element: &gst_base::BaseSrc, segment: &mut gst::Segment) -> bool {
        element.parent_do_seek(segment)
    }
    fn query(&self, element: &gst_base::BaseSrc, query: &mut gst::QueryRef) -> bool {
        element.parent_query(query)
    }
    fn event(&self, element: &gst_base::BaseSrc, event: &gst::Event) -> bool {
        element.parent_event(event)
    }
}

mopafy!(BaseSrcImpl);

pub unsafe trait BaseSrc: IsA<gst_base::BaseSrc> {
    fn parent_do_seek(&self, segment: &mut gst::Segment) -> bool {
        unsafe {
            // Our class
            let klass = *(self.to_glib_none().0 as *const glib_ffi::gpointer);
            // The parent class, RsElement or any other first-level Rust implementation
            let parent_klass = gobject_ffi::g_type_class_peek_parent(klass);
            // The actual parent class as defined in C
            let parent_klass = &*(gobject_ffi::g_type_class_peek_parent(parent_klass) as
                *const gst_base_ffi::GstBaseSrcClass);
            parent_klass
                .do_seek
                .map(|f| {
                    from_glib(f(self.to_glib_none().0, segment.to_glib_none_mut().0))
                })
                .unwrap_or(false)
        }
    }

    fn parent_query(&self, query: &mut gst::QueryRef) -> bool {
        unsafe {
            // Our class
            let klass = *(self.to_glib_none().0 as *const glib_ffi::gpointer);
            // The parent class, RsElement or any other first-level Rust implementation
            let parent_klass = gobject_ffi::g_type_class_peek_parent(klass);
            // The actual parent class as defined in C
            let parent_klass = &*(gobject_ffi::g_type_class_peek_parent(parent_klass) as
                *const gst_base_ffi::GstBaseSrcClass);
            parent_klass
                .query
                .map(|f| from_glib(f(self.to_glib_none().0, query.as_mut_ptr())))
                .unwrap_or(false)
        }
    }

    fn parent_event(&self, event: &gst::Event) -> bool {
        unsafe {
            // Our class
            let klass = *(self.to_glib_none().0 as *const glib_ffi::gpointer);
            // The parent class, RsElement or any other first-level Rust implementation
            let parent_klass = gobject_ffi::g_type_class_peek_parent(klass);
            // The actual parent class as defined in C
            let parent_klass = &*(gobject_ffi::g_type_class_peek_parent(parent_klass) as
                *const gst_base_ffi::GstBaseSrcClass);
            parent_klass
                .event
                .map(|f| {
                    from_glib(f(self.to_glib_none().0, event.to_glib_none().0))
                })
                .unwrap_or(false)
        }
    }
}

pub unsafe trait BaseSrcClass<T: ObjectType>
where
    T::RsType: IsA<gst_base::BaseSrc>,
    T::ImplType: BaseSrcImpl,
{
    fn override_vfuncs(&mut self) {
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

unsafe impl<T: IsA<gst_base::BaseSrc>> BaseSrc for T {}
pub type RsBaseSrcClass = ClassStruct<RsBaseSrc>;

// FIXME: Boilerplate
unsafe impl BaseSrcClass<RsBaseSrc> for gst_base_ffi::GstBaseSrcClass {}
unsafe impl BaseSrcClass<RsBaseSrc> for RsBaseSrcClass {}
unsafe impl ElementClass<RsBaseSrc> for gst_base_ffi::GstBaseSrcClass {}
unsafe impl ElementClass<RsBaseSrc> for RsBaseSrcClass {}
unsafe impl ObjectClass for gst_base_ffi::GstBaseSrcClass {}

// FIXME: Boilerplate
impl BaseSrcImpl for Box<BaseSrcImpl> {
    fn start(&self, element: &gst_base::BaseSrc) -> bool {
        let imp: &BaseSrcImpl = self.as_ref();
        imp.start(element)
    }

    fn stop(&self, element: &gst_base::BaseSrc) -> bool {
        let imp: &BaseSrcImpl = self.as_ref();
        imp.stop(element)
    }

    fn is_seekable(&self, element: &gst_base::BaseSrc) -> bool {
        let imp: &BaseSrcImpl = self.as_ref();
        imp.is_seekable(element)
    }

    fn get_size(&self, element: &gst_base::BaseSrc) -> Option<u64> {
        let imp: &BaseSrcImpl = self.as_ref();
        imp.get_size(element)
    }

    fn fill(
        &self,
        element: &gst_base::BaseSrc,
        offset: u64,
        length: u32,
        buffer: &mut gst::BufferRef,
    ) -> gst::FlowReturn {
        let imp: &BaseSrcImpl = self.as_ref();
        imp.fill(element, offset, length, buffer)
    }

    fn do_seek(&self, element: &gst_base::BaseSrc, segment: &mut gst::Segment) -> bool {
        let imp: &BaseSrcImpl = self.as_ref();
        imp.do_seek(element, segment)
    }

    fn query(&self, element: &gst_base::BaseSrc, query: &mut gst::QueryRef) -> bool {
        let imp: &BaseSrcImpl = self.as_ref();
        imp.query(element, query)
    }
    fn event(&self, element: &gst_base::BaseSrc, event: &gst::Event) -> bool {
        let imp: &BaseSrcImpl = self.as_ref();
        imp.event(element, event)
    }
}

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

// FIXME: Boilerplate
impl ObjectImpl for Box<BaseSrcImpl> {
    fn set_property(&self, obj: &glib::Object, id: u32, value: &glib::Value) {
        let imp: &BaseSrcImpl = self.as_ref();
        imp.set_property(obj, id, value);
    }

    fn get_property(&self, obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let imp: &BaseSrcImpl = self.as_ref();
        imp.get_property(obj, id)
    }
}

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
        BaseSrcClass::override_vfuncs(klass);
    }
}

unsafe extern "C" fn base_src_start<T: ObjectType>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
) -> glib_ffi::gboolean
where
    T::RsType: IsA<gst_base::BaseSrc>,
    T::ImplType: BaseSrcImpl,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: gst_base::BaseSrc = from_glib_borrow(ptr);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, { imp.start(&wrap) }).to_glib()
}

unsafe extern "C" fn base_src_stop<T: ObjectType>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
) -> glib_ffi::gboolean
where
    T::RsType: IsA<gst_base::BaseSrc>,
    T::ImplType: BaseSrcImpl,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: gst_base::BaseSrc = from_glib_borrow(ptr);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, { imp.stop(&wrap) }).to_glib()
}

unsafe extern "C" fn base_src_is_seekable<T: ObjectType>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
) -> glib_ffi::gboolean
where
    T::RsType: IsA<gst_base::BaseSrc>,
    T::ImplType: BaseSrcImpl,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: gst_base::BaseSrc = from_glib_borrow(ptr);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, { imp.is_seekable(&wrap) }).to_glib()
}

unsafe extern "C" fn base_src_get_size<T: ObjectType>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    size: *mut u64,
) -> glib_ffi::gboolean
where
    T::RsType: IsA<gst_base::BaseSrc>,
    T::ImplType: BaseSrcImpl,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: gst_base::BaseSrc = from_glib_borrow(ptr);
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

unsafe extern "C" fn base_src_fill<T: ObjectType>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    offset: u64,
    length: u32,
    buffer: *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn
where
    T::RsType: IsA<gst_base::BaseSrc>,
    T::ImplType: BaseSrcImpl,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: gst_base::BaseSrc = from_glib_borrow(ptr);
    let imp = &*element.imp;
    let buffer = gst::BufferRef::from_mut_ptr(buffer);

    panic_to_error!(&wrap, &element.panicked, gst::FlowReturn::Error, {
        imp.fill(&wrap, offset, length, buffer)
    }).to_glib()
}

unsafe extern "C" fn base_src_do_seek<T: ObjectType>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    segment: *mut gst_ffi::GstSegment,
) -> glib_ffi::gboolean
where
    T::RsType: IsA<gst_base::BaseSrc>,
    T::ImplType: BaseSrcImpl,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: gst_base::BaseSrc = from_glib_borrow(ptr);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, {
        imp.do_seek(&wrap, &mut from_glib_borrow(segment))
    }).to_glib()
}

unsafe extern "C" fn base_src_query<T: ObjectType>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    query_ptr: *mut gst_ffi::GstQuery,
) -> glib_ffi::gboolean
where
    T::RsType: IsA<gst_base::BaseSrc>,
    T::ImplType: BaseSrcImpl,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: gst_base::BaseSrc = from_glib_borrow(ptr);
    let imp = &*element.imp;
    let query = gst::QueryRef::from_mut_ptr(query_ptr);

    panic_to_error!(&wrap, &element.panicked, false, { imp.query(&wrap, query) }).to_glib()
}

unsafe extern "C" fn base_src_event<T: ObjectType>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    event_ptr: *mut gst_ffi::GstEvent,
) -> glib_ffi::gboolean
where
    T::RsType: IsA<gst_base::BaseSrc>,
    T::ImplType: BaseSrcImpl,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: gst_base::BaseSrc = from_glib_borrow(ptr);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, {
        imp.event(&wrap, &from_glib_none(event_ptr))
    }).to_glib()
}
