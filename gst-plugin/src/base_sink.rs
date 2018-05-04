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

pub trait BaseSinkImpl<T: BaseSinkBase>:
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

    fn render(&self, element: &T, buffer: &gst::BufferRef) -> gst::FlowReturn;

    fn prepare(&self, _element: &T, _buffer: &gst::BufferRef) -> gst::FlowReturn {
        gst::FlowReturn::Ok
    }

    fn render_list(&self, element: &T, list: &gst::BufferListRef) -> gst::FlowReturn {
        for buffer in list.iter() {
            let ret = self.render(element, buffer);
            if ret != gst::FlowReturn::Ok {
                return ret;
            }
        }

        gst::FlowReturn::Ok
    }

    fn prepare_list(&self, element: &T, list: &gst::BufferListRef) -> gst::FlowReturn {
        for buffer in list.iter() {
            let ret = self.prepare(element, buffer);
            if ret != gst::FlowReturn::Ok {
                return ret;
            }
        }

        gst::FlowReturn::Ok
    }

    fn query(&self, element: &T, query: &mut gst::QueryRef) -> bool {
        element.parent_query(query)
    }

    fn event(&self, element: &T, event: gst::Event) -> bool {
        element.parent_event(event)
    }

    fn get_caps(&self, element: &T, filter: Option<&gst::CapsRef>) -> Option<gst::Caps> {
        element.parent_get_caps(filter)
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

any_impl!(BaseSinkBase, BaseSinkImpl, PanicPoison);

pub unsafe trait BaseSinkBase:
    IsA<gst::Element> + IsA<gst_base::BaseSink> + ObjectType
{
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

    fn parent_event(&self, event: gst::Event) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSinkClass;
            (*parent_klass)
                .event
                .map(|f| from_glib(f(self.to_glib_none().0, event.into_ptr())))
                .unwrap_or(false)
        }
    }

    fn parent_get_caps(&self, filter: Option<&gst::CapsRef>) -> Option<gst::Caps> {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSinkClass;
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

    fn parent_set_caps(&self, caps: &gst::CapsRef) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSinkClass;
            (*parent_klass)
                .set_caps
                .map(|f| from_glib(f(self.to_glib_none().0, caps.as_mut_ptr())))
                .unwrap_or(true)
        }
    }

    fn parent_fixate(&self, caps: gst::Caps) -> gst::Caps {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_base_ffi::GstBaseSinkClass;

            match (*parent_klass).fixate {
                Some(fixate) => from_glib_full(fixate(self.to_glib_none().0, caps.into_ptr())),
                None => caps,
            }
        }
    }
}

pub unsafe trait BaseSinkClassExt<T: BaseSinkBase>
where
    T::ImplType: BaseSinkImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    fn override_vfuncs(&mut self, _: &ClassInitToken) {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gst_base_ffi::GstBaseSinkClass);
            klass.start = Some(base_sink_start::<T>);
            klass.stop = Some(base_sink_stop::<T>);
            klass.render = Some(base_sink_render::<T>);
            klass.render_list = Some(base_sink_render_list::<T>);
            klass.prepare = Some(base_sink_prepare::<T>);
            klass.prepare_list = Some(base_sink_prepare_list::<T>);
            klass.query = Some(base_sink_query::<T>);
            klass.event = Some(base_sink_event::<T>);
            klass.get_caps = Some(base_sink_get_caps::<T>);
            klass.set_caps = Some(base_sink_set_caps::<T>);
            klass.fixate = Some(base_sink_fixate::<T>);
            klass.unlock = Some(base_sink_unlock::<T>);
            klass.unlock_stop = Some(base_sink_unlock_stop::<T>);
        }
    }
}

glib_wrapper! {
    pub struct BaseSink(Object<ElementInstanceStruct<BaseSink>>):
        [gst_base::BaseSink => gst_base_ffi::GstBaseSink,
         gst::Element => gst_ffi::GstElement,
         gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || get_type::<BaseSink>(),
    }
}

unsafe impl<T: IsA<gst::Element> + IsA<gst_base::BaseSink> + ObjectType> BaseSinkBase for T {}
pub type BaseSinkClass = ClassStruct<BaseSink>;

// FIXME: Boilerplate
unsafe impl BaseSinkClassExt<BaseSink> for BaseSinkClass {}
unsafe impl ElementClassExt<BaseSink> for BaseSinkClass {}
unsafe impl ObjectClassExt<BaseSink> for BaseSinkClass {}

unsafe impl Send for BaseSink {}
unsafe impl Sync for BaseSink {}

#[macro_export]
macro_rules! box_base_sink_impl(
    ($name:ident) => {
        box_element_impl!($name);

        impl<T: BaseSinkBase> BaseSinkImpl<T> for Box<$name<T>>
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

            fn render(&self, element: &T, buffer: &gst::BufferRef) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.render(element, buffer)
            }

            fn prepare(&self, element: &T, buffer: &gst::BufferRef) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.prepare(element, buffer)
            }

            fn render_list(&self, element: &T, list: &gst::BufferListRef) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.render_list(element, list)
            }

            fn prepare_list(&self, element: &T, list: &gst::BufferListRef) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.prepare_list(element, list)
            }

            fn query(&self, element: &T, query: &mut gst::QueryRef) -> bool {
                let imp: &$name<T> = self.as_ref();
                BaseSinkImpl::query(imp, element, query)
            }

            fn event(&self, element: &T, event: gst::Event) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.event(element, event)
            }

            fn get_caps(&self, element: &T, filter: Option<&gst::CapsRef>) -> Option<gst::Caps> {
                let imp: &$name<T> = self.as_ref();
                imp.get_caps(element, filter)
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

box_base_sink_impl!(BaseSinkImpl);

impl ObjectType for BaseSink {
    const NAME: &'static str = "RsBaseSink";
    type ParentType = gst_base::BaseSink;
    type ImplType = Box<BaseSinkImpl<Self>>;
    type InstanceStructType = ElementInstanceStruct<Self>;

    fn class_init(token: &ClassInitToken, klass: &mut BaseSinkClass) {
        ElementClassExt::override_vfuncs(klass, token);
        BaseSinkClassExt::override_vfuncs(klass, token);
    }

    object_type_fns!();
}

unsafe extern "C" fn base_sink_start<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSinkImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, { imp.start(&wrap) }).to_glib()
}

unsafe extern "C" fn base_sink_stop<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSinkImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, { imp.stop(&wrap) }).to_glib()
}

unsafe extern "C" fn base_sink_render<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
    buffer: *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: BaseSinkImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();
    let buffer = gst::BufferRef::from_ptr(buffer);

    panic_to_error!(&wrap, &element.panicked(), gst::FlowReturn::Error, {
        imp.render(&wrap, buffer)
    }).to_glib()
}

unsafe extern "C" fn base_sink_prepare<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
    buffer: *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: BaseSinkImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();
    let buffer = gst::BufferRef::from_ptr(buffer);

    panic_to_error!(&wrap, &element.panicked(), gst::FlowReturn::Error, {
        imp.prepare(&wrap, buffer)
    }).to_glib()
}

unsafe extern "C" fn base_sink_render_list<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
    list: *mut gst_ffi::GstBufferList,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: BaseSinkImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();
    let list = gst::BufferListRef::from_ptr(list);

    panic_to_error!(&wrap, &element.panicked(), gst::FlowReturn::Error, {
        imp.render_list(&wrap, list)
    }).to_glib()
}

unsafe extern "C" fn base_sink_prepare_list<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
    list: *mut gst_ffi::GstBufferList,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: BaseSinkImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();
    let list = gst::BufferListRef::from_ptr(list);

    panic_to_error!(&wrap, &element.panicked(), gst::FlowReturn::Error, {
        imp.prepare_list(&wrap, list)
    }).to_glib()
}

unsafe extern "C" fn base_sink_query<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
    query_ptr: *mut gst_ffi::GstQuery,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSinkImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();
    let query = gst::QueryRef::from_mut_ptr(query_ptr);

    panic_to_error!(&wrap, &element.panicked(), false, {
        BaseSinkImpl::query(imp, &wrap, query)
    }).to_glib()
}

unsafe extern "C" fn base_sink_event<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
    event_ptr: *mut gst_ffi::GstEvent,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSinkImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, {
        imp.event(&wrap, from_glib_full(event_ptr))
    }).to_glib()
}

unsafe extern "C" fn base_sink_get_caps<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
    filter: *mut gst_ffi::GstCaps,
) -> *mut gst_ffi::GstCaps
where
    T::ImplType: BaseSinkImpl<T>,
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

unsafe extern "C" fn base_sink_set_caps<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
    caps: *mut gst_ffi::GstCaps,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSinkImpl<T>,
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

unsafe extern "C" fn base_sink_fixate<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
    caps: *mut gst_ffi::GstCaps,
) -> *mut gst_ffi::GstCaps
where
    T::ImplType: BaseSinkImpl<T>,
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

unsafe extern "C" fn base_sink_unlock<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSinkImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = element.get_impl();

    panic_to_error!(&wrap, &element.panicked(), false, { imp.unlock(&wrap) }).to_glib()
}

unsafe extern "C" fn base_sink_unlock_stop<T: BaseSinkBase>(
    ptr: *mut gst_base_ffi::GstBaseSink,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseSinkImpl<T>,
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
