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
use gst_base_ffi;
use gst_ffi;

use glib;
use glib::translate::*;
use gst;
use gst::prelude::*;
use gst_base;

use gobject_subclass::anyimpl::*;
use gobject_subclass::object::*;

use element::*;
use object::*;

pub trait BaseSrcImpl<T: BaseSrcBase>:
    AnyImpl + ObjectImpl<T> + ElementImpl<T> + Send + Sync + 'static
where
    T::InstanceStructType: PanicPoison,
{
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
        _element: &T,
        _offset: u64,
        _length: u32,
        _buffer: &mut gst::BufferRef,
    ) -> gst::FlowReturn {
        unimplemented!()
    }

    fn create(
        &self,
        element: &T,
        offset: u64,
        length: u32,
    ) -> Result<gst::Buffer, gst::FlowReturn> {
        element.parent_create(offset, length)
    }

    fn do_seek(&self, element: &T, segment: &mut gst::Segment) -> bool {
        element.parent_do_seek(segment)
    }

    fn query(&self, element: &T, query: &mut gst::QueryRef) -> bool {
        element.parent_query(query)
    }

    fn event(&self, element: &T, event: &gst::Event) -> bool {
        element.parent_event(event)
    }

    fn get_caps(&self, element: &T, filter: Option<&gst::CapsRef>) -> Option<gst::Caps> {
        element.parent_get_caps(filter)
    }

    fn negotiate(&self, element: &T) -> bool {
        element.parent_negotiate()
    }

    fn set_caps(&self, element: &T, caps: &gst::CapsRef) -> bool {
        element.parent_set_caps(caps)
    }

    fn fixate(&self, element: &T, caps: gst::Caps) -> gst::Caps {
        element.parent_fixate(caps)
    }

    fn unlock(&self, _element: &T) -> bool {
        true
    }

    fn unlock_stop(&self, _element: &T) -> bool {
        true
    }
}

any_impl!(BaseSrcBase, BaseSrcImpl, PanicPoison);

pub unsafe trait BaseSrcBase:
    IsA<gst::Element> + IsA<gst_base::BaseSrc> + ObjectType
{
    fn parent_create(&self, offset: u64, length: u32) -> Result<gst::Buffer, gst::FlowReturn> {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSrcClass;
            (*parent_klass)
                .create
                .map(|f| {
                    let mut buffer: *mut gst_ffi::GstBuffer = ptr::null_mut();
                    // FIXME: Wrong signature in -sys bindings
                    // https://github.com/sdroege/gstreamer-sys/issues/3
                    let buffer_ref = &mut buffer as *mut _ as *mut gst_ffi::GstBuffer;
                    match from_glib(f(self.to_glib_none().0, offset, length, buffer_ref)) {
                        gst::FlowReturn::Ok => Ok(from_glib_full(buffer)),
                        ret => Err(ret),
                    }
                })
                .unwrap_or(Err(gst::FlowReturn::Error))
        }
    }

    fn parent_do_seek(&self, segment: &mut gst::Segment) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSrcClass;
            (*parent_klass)
                .do_seek
                .map(|f| from_glib(f(self.to_glib_none().0, segment.to_glib_none_mut().0)))
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
                .map(|f| from_glib(f(self.to_glib_none().0, event.to_glib_none().0)))
                .unwrap_or(false)
        }
    }

    fn parent_get_caps(&self, filter: Option<&gst::CapsRef>) -> Option<gst::Caps> {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSrcClass;
            let filter_ptr = if let Some(filter) = filter {
                filter.as_mut_ptr()
            } else {
                ptr::null_mut()
            };

            (*parent_klass)
                .get_caps
                .map(|f| from_glib_full(f(self.to_glib_none().0, filter_ptr)))
                .unwrap_or(None)
        }
    }

    fn parent_negotiate(&self) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSrcClass;
            (*parent_klass)
                .negotiate
                .map(|f| from_glib(f(self.to_glib_none().0)))
                .unwrap_or(false)
        }
    }

    fn parent_set_caps(&self, caps: &gst::CapsRef) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSrcClass;
            (*parent_klass)
                .set_caps
                .map(|f| from_glib(f(self.to_glib_none().0, caps.as_mut_ptr())))
                .unwrap_or(true)
        }
    }

    fn parent_fixate(&self, caps: gst::Caps) -> gst::Caps {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSrcClass;

            match (*parent_klass).fixate {
                Some(fixate) => from_glib_full(fixate(self.to_glib_none().0, caps.into_ptr())),
                None => caps,
            }
        }
    }
}

pub unsafe trait BaseSrcClassExt<T: BaseSrcBase>
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    fn override_vfuncs(&mut self, _: &ClassInitToken) {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gst_base_ffi::GstBaseSrcClass);
            klass.start = Some(base_src_start::<T>);
            klass.stop = Some(base_src_stop::<T>);
            klass.is_seekable = Some(base_src_is_seekable::<T>);
            klass.get_size = Some(base_src_get_size::<T>);
            klass.fill = Some(base_src_fill::<T>);
            klass.create = Some(base_src_create::<T>);
            klass.do_seek = Some(base_src_do_seek::<T>);
            klass.query = Some(base_src_query::<T>);
            klass.event = Some(base_src_event::<T>);
            klass.get_caps = Some(base_src_get_caps::<T>);
            klass.negotiate = Some(base_src_negotiate::<T>);
            klass.set_caps = Some(base_src_set_caps::<T>);
            klass.fixate = Some(base_src_fixate::<T>);
            klass.unlock = Some(base_src_unlock::<T>);
            klass.unlock_stop = Some(base_src_unlock_stop::<T>);
        }
    }
}

glib_wrapper! {
    pub struct BaseSrc(Object<ElementInstanceStruct<BaseSrc>>):
        [gst_base::BaseSrc => gst_base_ffi::GstBaseSrc,
         gst::Element => gst_ffi::GstElement,
         gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || get_type::<BaseSrc>(),
    }
}

unsafe impl<T: IsA<gst::Element> + IsA<gst_base::BaseSrc> + ObjectType> BaseSrcBase for T {}
pub type BaseSrcClass = ClassStruct<BaseSrc>;

// FIXME: Boilerplate
unsafe impl BaseSrcClassExt<BaseSrc> for BaseSrcClass {}
unsafe impl ElementClassExt<BaseSrc> for BaseSrcClass {}

unsafe impl Send for BaseSrc {}
unsafe impl Sync for BaseSrc {}

#[macro_export]
macro_rules! box_base_src_impl(
    ($name:ident) => {
        box_element_impl!($name);

        impl<T: BaseSrcBase> BaseSrcImpl<T> for Box<$name<T>>
        where
            T::InstanceStructType: PanicPoison
        {
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

            fn create(
                &self,
                element: &T,
                offset: u64,
                length: u32,
            ) -> Result<gst::Buffer, gst::FlowReturn> {
                let imp: &$name<T> = self.as_ref();
                imp.create(element, offset, length)
            }

            fn do_seek(&self, element: &T, segment: &mut gst::Segment) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.do_seek(element, segment)
            }

            fn query(&self, element: &T, query: &mut gst::QueryRef) -> bool {
                let imp: &$name<T> = self.as_ref();
                BaseSrcImpl::query(imp, element, query)
            }

            fn event(&self, element: &T, event: &gst::Event) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.event(element, event)
            }

            fn get_caps(&self, element: &T, filter: Option<&gst::CapsRef>) -> Option<gst::Caps> {
                let imp: &$name<T> = self.as_ref();
                imp.get_caps(element, filter)
            }

            fn negotiate(&self, element: &T) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.negotiate(element)
            }

            fn set_caps(&self, element: &T, caps: &gst::CapsRef) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.set_caps(element, caps)
            }

            fn fixate(&self, element: &T, caps: gst::Caps) -> gst::Caps {
                let imp: &$name<T> = self.as_ref();
                imp.fixate(element, caps)
            }

            fn unlock(&self, element: &T) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.unlock(element)
            }

            fn unlock_stop(&self, element: &T) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.unlock_stop(element)
            }
        }
    };
);
box_base_src_impl!(BaseSrcImpl);

impl ObjectType for BaseSrc {
    const NAME: &'static str = "RsBaseSrc";
    type GlibType = gst_base_ffi::GstBaseSrc;
    type GlibClassType = gst_base_ffi::GstBaseSrcClass;
    type ImplType = Box<BaseSrcImpl<Self>>;
    type InstanceStructType = ElementInstanceStruct<Self>;

    fn glib_type() -> glib::Type {
        unsafe { from_glib(gst_base_ffi::gst_base_src_get_type()) }
    }

    fn class_init(token: &ClassInitToken, klass: &mut BaseSrcClass) {
        ElementClassExt::override_vfuncs(klass, token);
        BaseSrcClassExt::override_vfuncs(klass, token);
    }

    object_type_fns!();
}

unsafe extern "C" fn base_src_start<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, { imp.start(&wrap) }).to_glib()
}

unsafe extern "C" fn base_src_stop<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, { imp.stop(&wrap) }).to_glib()
}

unsafe extern "C" fn base_src_is_seekable<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, {
        imp.is_seekable(&wrap)
    }).to_glib()
}

unsafe extern "C" fn base_src_get_size<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    size: *mut u64,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, {
        match imp.get_size(&wrap) {
            Some(s) => {
                *size = s;
                true
            }
            None => false,
        }
    }).to_glib()
}

unsafe extern "C" fn base_src_fill<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    offset: u64,
    length: u32,
    buffer: *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();
    let buffer = gst::BufferRef::from_mut_ptr(buffer);

    panic_to_error!(&wrap, &element.panicked(), gst::FlowReturn::Error, {
        imp.fill(&wrap, offset, length, buffer)
    }).to_glib()
}

unsafe extern "C" fn base_src_create<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    offset: u64,
    length: u32,
    buffer_ptr: *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();
    // FIXME: Wrong signature in -sys bindings
    // https://github.com/sdroege/gstreamer-sys/issues/3
    let buffer_ptr = buffer_ptr as *mut *mut gst_ffi::GstBuffer;

    panic_to_error!(&wrap, &element.panicked(), gst::FlowReturn::Error, {
        match imp.create(&wrap, offset, length) {
            Ok(buffer) => {
                *buffer_ptr = buffer.into_ptr();
                gst::FlowReturn::Ok
            }
            Err(err) => err,
        }
    }).to_glib()
}

unsafe extern "C" fn base_src_do_seek<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    segment: *mut gst_ffi::GstSegment,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, {
        imp.do_seek(&wrap, &mut from_glib_borrow(segment))
    }).to_glib()
}

unsafe extern "C" fn base_src_query<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    query_ptr: *mut gst_ffi::GstQuery,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();
    let query = gst::QueryRef::from_mut_ptr(query_ptr);

    panic_to_error!(&wrap, &element.panicked(), false, {
        BaseSrcImpl::query(imp, &wrap, query)
    }).to_glib()
}

unsafe extern "C" fn base_src_event<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    event_ptr: *mut gst_ffi::GstEvent,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, {
        imp.event(&wrap, &from_glib_none(event_ptr))
    }).to_glib()
}

unsafe extern "C" fn base_src_get_caps<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    filter: *mut gst_ffi::GstCaps,
) -> *mut gst_ffi::GstCaps
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();
    let filter = if filter.is_null() {
        None
    } else {
        Some(gst::CapsRef::from_ptr(filter))
    };

    panic_to_error!(&wrap, &element.panicked(), None, {
        imp.get_caps(&wrap, filter)
    }).map(|caps| caps.into_ptr())
        .unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn base_src_negotiate<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, { imp.negotiate(&wrap) }).to_glib()
}

unsafe extern "C" fn base_src_set_caps<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    caps: *mut gst_ffi::GstCaps,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();
    let caps = gst::CapsRef::from_ptr(caps);

    panic_to_error!(&wrap, &element.panicked(), false, {
        imp.set_caps(&wrap, caps)
    }).to_glib()
}

unsafe extern "C" fn base_src_fixate<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
    caps: *mut gst_ffi::GstCaps,
) -> *mut gst_ffi::GstCaps
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();
    let caps = from_glib_full(caps);

    panic_to_error!(&wrap, &element.panicked(), gst::Caps::new_empty(), {
        imp.fixate(&wrap, caps)
    }).into_ptr()
}

unsafe extern "C" fn base_src_unlock<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, { imp.unlock(&wrap) }).to_glib()
}

unsafe extern "C" fn base_src_unlock_stop<T: BaseSrcBase>(
    ptr: *mut gst_base_ffi::GstBaseSrc,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSrcImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, {
        imp.unlock_stop(&wrap)
    }).to_glib()
}
