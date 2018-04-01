// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ptr;
use std::mem;

use glib_ffi;
use gobject_ffi;
use gst_ffi;
use gst_base_ffi;

use glib;
use glib::translate::*;
use gst;
use gst::prelude::*;
use gst_base;

use object::*;
use element::*;
use anyimpl::*;

pub trait BaseTransformImpl<T: BaseTransformBase>
    : AnyImpl + ObjectImpl<T> + ElementImpl<T> + Send + Sync + 'static {
    fn start(&self, _element: &T) -> bool {
        true
    }

    fn stop(&self, _element: &T) -> bool {
        true
    }

    fn transform_caps(
        &self,
        element: &T,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> gst::Caps {
        element.parent_transform_caps(direction, caps, filter)
    }

    fn fixate_caps(
        &self,
        element: &T,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        othercaps: gst::Caps,
    ) -> gst::Caps {
        element.parent_fixate_caps(direction, caps, othercaps)
    }

    fn set_caps(&self, _element: &T, _incaps: &gst::Caps, _outcaps: &gst::Caps) -> bool {
        true
    }

    fn accept_caps(&self, element: &T, direction: gst::PadDirection, caps: &gst::Caps) -> bool {
        element.parent_accept_caps(direction, caps)
    }

    fn query(&self, element: &T, direction: gst::PadDirection, query: &mut gst::QueryRef) -> bool {
        element.parent_query(direction, query)
    }

    fn transform_size(
        &self,
        element: &T,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        size: usize,
        othercaps: &gst::Caps,
    ) -> Option<usize> {
        element.parent_transform_size(direction, caps, size, othercaps)
    }

    fn get_unit_size(&self, _element: &T, _caps: &gst::Caps) -> Option<usize> {
        unimplemented!();
    }

    fn sink_event(&self, element: &T, event: gst::Event) -> bool {
        element.parent_sink_event(event)
    }

    fn src_event(&self, element: &T, event: gst::Event) -> bool {
        element.parent_src_event(event)
    }

    fn transform(
        &self,
        _element: &T,
        _inbuf: &gst::Buffer,
        _outbuf: &mut gst::BufferRef,
    ) -> gst::FlowReturn {
        unimplemented!();
    }

    fn transform_ip(&self, _element: &T, _buf: &mut gst::BufferRef) -> gst::FlowReturn {
        unimplemented!();
    }

    fn transform_ip_passthrough(&self, _element: &T, _buf: &gst::BufferRef) -> gst::FlowReturn {
        unimplemented!();
    }
}

any_impl!(BaseTransformBase, BaseTransformImpl);

pub unsafe trait BaseTransformBase
    : IsA<gst::Element> + IsA<gst_base::BaseTransform> + ObjectType {
    fn parent_transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> gst::Caps {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstBaseTransformClass;
            match (*parent_klass).transform_caps {
                Some(f) => from_glib_full(f(
                    self.to_glib_none().0,
                    direction.to_glib(),
                    caps.to_glib_none().0,
                    filter.to_glib_none().0,
                )),
                None => caps.clone(),
            }
        }
    }

    fn parent_fixate_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        othercaps: gst::Caps,
    ) -> gst::Caps {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstBaseTransformClass;
            match (*parent_klass).fixate_caps {
                Some(f) => from_glib_full(f(
                    self.to_glib_none().0,
                    direction.to_glib(),
                    caps.to_glib_none().0,
                    othercaps.into_ptr(),
                )),
                None => othercaps,
            }
        }
    }

    fn parent_accept_caps(&self, direction: gst::PadDirection, caps: &gst::Caps) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstBaseTransformClass;
            (*parent_klass)
                .accept_caps
                .map(|f| {
                    from_glib(f(
                        self.to_glib_none().0,
                        direction.to_glib(),
                        caps.to_glib_none().0,
                    ))
                })
                .unwrap_or(false)
        }
    }

    fn parent_query(&self, direction: gst::PadDirection, query: &mut gst::QueryRef) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstBaseTransformClass;
            (*parent_klass)
                .query
                .map(|f| {
                    from_glib(f(
                        self.to_glib_none().0,
                        direction.to_glib(),
                        query.as_mut_ptr(),
                    ))
                })
                .unwrap_or(false)
        }
    }

    fn parent_transform_size(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        size: usize,
        othercaps: &gst::Caps,
    ) -> Option<usize> {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstBaseTransformClass;
            (*parent_klass)
                .transform_size
                .map(|f| {
                    let mut othersize = 0;
                    let res: bool = from_glib(f(
                        self.to_glib_none().0,
                        direction.to_glib(),
                        caps.to_glib_none().0,
                        size,
                        othercaps.to_glib_none().0,
                        &mut othersize,
                    ));
                    if res {
                        Some(othersize)
                    } else {
                        None
                    }
                })
                .unwrap_or(None)
        }
    }

    fn parent_sink_event(&self, event: gst::Event) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstBaseTransformClass;
            (*parent_klass)
                .sink_event
                .map(|f| from_glib(f(self.to_glib_none().0, event.into_ptr())))
                .unwrap_or(false)
        }
    }

    fn parent_src_event(&self, event: gst::Event) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstBaseTransformClass;
            (*parent_klass)
                .src_event
                .map(|f| from_glib(f(self.to_glib_none().0, event.into_ptr())))
                .unwrap_or(false)
        }
    }
}

pub enum BaseTransformMode {
    AlwaysInPlace,
    NeverInPlace,
    Both,
}

pub unsafe trait BaseTransformClassExt<T: BaseTransformBase>
where
    T::ImplType: BaseTransformImpl<T>,
{
    fn configure(
        &mut self,
        mode: BaseTransformMode,
        passthrough_on_same_caps: bool,
        transform_ip_on_passthrough: bool,
    ) {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gst_base_ffi::GstBaseTransformClass);

            klass.passthrough_on_same_caps = passthrough_on_same_caps.to_glib();
            klass.transform_ip_on_passthrough = transform_ip_on_passthrough.to_glib();

            match mode {
                BaseTransformMode::AlwaysInPlace => {
                    klass.transform_ip = Some(base_transform_transform_ip::<T>);
                }
                BaseTransformMode::NeverInPlace => {
                    klass.transform = Some(base_transform_transform::<T>);
                }
                BaseTransformMode::Both => {
                    klass.transform = Some(base_transform_transform::<T>);
                    klass.transform_ip = Some(base_transform_transform_ip::<T>);
                }
            }
        }
    }

    fn override_vfuncs(&mut self, _: &ClassInitToken) {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gst_base_ffi::GstBaseTransformClass);
            klass.start = Some(base_transform_start::<T>);
            klass.stop = Some(base_transform_stop::<T>);
            klass.transform_caps = Some(base_transform_transform_caps::<T>);
            klass.fixate_caps = Some(base_transform_fixate_caps::<T>);
            klass.set_caps = Some(base_transform_set_caps::<T>);
            klass.accept_caps = Some(base_transform_accept_caps::<T>);
            klass.query = Some(base_transform_query::<T>);
            klass.transform_size = Some(base_transform_transform_size::<T>);
            klass.get_unit_size = Some(base_transform_get_unit_size::<T>);
            klass.sink_event = Some(base_transform_sink_event::<T>);
            klass.src_event = Some(base_transform_src_event::<T>);
        }
    }
}

glib_wrapper! {
    pub struct BaseTransform(Object<InstanceStruct<BaseTransform>>): [gst_base::BaseTransform => gst_base_ffi::GstBaseTransform,
                                                                      gst::Element => gst_ffi::GstElement,
                                                                      gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || get_type::<BaseTransform>(),
    }
}

unsafe impl<T: IsA<gst::Element> + IsA<gst_base::BaseTransform> + ObjectType> BaseTransformBase
    for T
{
}
pub type BaseTransformClass = ClassStruct<BaseTransform>;

// FIXME: Boilerplate
unsafe impl BaseTransformClassExt<BaseTransform> for BaseTransformClass {}
unsafe impl ElementClassExt<BaseTransform> for BaseTransformClass {}

unsafe impl Send for BaseTransform {}
unsafe impl Sync for BaseTransform {}

#[macro_export]
macro_rules! box_base_transform_impl(
    ($name:ident) => {
        box_element_impl!($name);

        impl<T: BaseTransformBase> BaseTransformImpl<T> for Box<$name<T>> {
            fn start(&self, element: &T) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.start(element)
            }

            fn stop(&self, element: &T) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.stop(element)
            }

            fn transform_caps(&self, element: &T, direction: gst::PadDirection, caps: &gst::Caps, filter: Option<&gst::Caps>) -> gst::Caps {
                let imp: &$name<T> = self.as_ref();
                imp.transform_caps(element, direction, caps, filter)
            }

            fn fixate_caps(&self, element: &T, direction: gst::PadDirection, caps: &gst::Caps, othercaps: gst::Caps) -> gst::Caps {
                let imp: &$name<T> = self.as_ref();
                imp.fixate_caps(element, direction, caps, othercaps)
            }

            fn set_caps(&self, element: &T, incaps: &gst::Caps, outcaps: &gst::Caps) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.set_caps(element, incaps, outcaps)
            }

            fn accept_caps(&self, element: &T, direction: gst::PadDirection, caps: &gst::Caps) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.accept_caps(element, direction, caps)
            }

            fn query(&self, element: &T, direction: gst::PadDirection, query: &mut gst::QueryRef) -> bool {
                let imp: &$name<T> = self.as_ref();
                BaseTransformImpl::query(imp, element, direction, query)
            }

            fn transform_size(&self, element: &T, direction: gst::PadDirection, caps: &gst::Caps, size: usize, othercaps: &gst::Caps) -> Option<usize> {
                let imp: &$name<T> = self.as_ref();
                imp.transform_size(element, direction, caps, size, othercaps)
            }

            fn get_unit_size(&self, element: &T, caps: &gst::Caps) -> Option<usize> {
                let imp: &$name<T> = self.as_ref();
                imp.get_unit_size(element, caps)
            }

            fn sink_event(&self, element: &T, event: gst::Event) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.sink_event(element, event)
            }

            fn src_event(&self, element: &T, event: gst::Event) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.src_event(element, event)
            }

            fn transform(&self, element: &T, inbuf: &gst::Buffer, outbuf: &mut gst::BufferRef) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.transform(element, inbuf, outbuf)
            }

            fn transform_ip(&self, element: &T, buf: &mut gst::BufferRef) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.transform_ip(element, buf)
            }

            fn transform_ip_passthrough(&self, element: &T, buf: &gst::BufferRef) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.transform_ip_passthrough(element, buf)
            }
        }
    };
);
box_base_transform_impl!(BaseTransformImpl);

impl ObjectType for BaseTransform {
    const NAME: &'static str = "RsBaseTransform";
    type GlibType = gst_base_ffi::GstBaseTransform;
    type GlibClassType = gst_base_ffi::GstBaseTransformClass;
    type ImplType = Box<BaseTransformImpl<Self>>;

    fn glib_type() -> glib::Type {
        unsafe { from_glib(gst_base_ffi::gst_base_transform_get_type()) }
    }

    fn class_init(token: &ClassInitToken, klass: &mut BaseTransformClass) {
        ElementClassExt::override_vfuncs(klass, token);
        BaseTransformClassExt::override_vfuncs(klass, token);
    }

    object_type_fns!();
}

unsafe extern "C" fn base_transform_start<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    panic_to_error!(&wrap, &element.panicked, false, { imp.start(&wrap) }).to_glib()
}

unsafe extern "C" fn base_transform_stop<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    panic_to_error!(&wrap, &element.panicked, false, { imp.stop(&wrap) }).to_glib()
}

unsafe extern "C" fn base_transform_transform_caps<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
    direction: gst_ffi::GstPadDirection,
    caps: *mut gst_ffi::GstCaps,
    filter: *mut gst_ffi::GstCaps,
) -> *mut gst_ffi::GstCaps
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    panic_to_error!(&wrap, &element.panicked, gst::Caps::new_empty(), {
        let filter = if filter.is_null() {
            None
        } else {
            Some(from_glib_borrow(filter))
        };

        imp.transform_caps(
            &wrap,
            from_glib(direction),
            &from_glib_borrow(caps),
            filter.as_ref(),
        )
    }).into_ptr()
}

unsafe extern "C" fn base_transform_fixate_caps<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
    direction: gst_ffi::GstPadDirection,
    caps: *mut gst_ffi::GstCaps,
    othercaps: *mut gst_ffi::GstCaps,
) -> *mut gst_ffi::GstCaps
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    panic_to_error!(&wrap, &element.panicked, gst::Caps::new_empty(), {
        imp.fixate_caps(
            &wrap,
            from_glib(direction),
            &from_glib_borrow(caps),
            from_glib_full(othercaps),
        )
    }).into_ptr()
}

unsafe extern "C" fn base_transform_set_caps<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
    incaps: *mut gst_ffi::GstCaps,
    outcaps: *mut gst_ffi::GstCaps,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    panic_to_error!(&wrap, &element.panicked, false, {
        imp.set_caps(&wrap, &from_glib_borrow(incaps), &from_glib_borrow(outcaps))
    }).to_glib()
}

unsafe extern "C" fn base_transform_accept_caps<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
    direction: gst_ffi::GstPadDirection,
    caps: *mut gst_ffi::GstCaps,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    panic_to_error!(&wrap, &element.panicked, false, {
        imp.accept_caps(&wrap, from_glib(direction), &from_glib_borrow(caps))
    }).to_glib()
}

unsafe extern "C" fn base_transform_query<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
    direction: gst_ffi::GstPadDirection,
    query: *mut gst_ffi::GstQuery,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    panic_to_error!(&wrap, &element.panicked, false, {
        BaseTransformImpl::query(
            imp,
            &wrap,
            from_glib(direction),
            gst::QueryRef::from_mut_ptr(query),
        )
    }).to_glib()
}

unsafe extern "C" fn base_transform_transform_size<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
    direction: gst_ffi::GstPadDirection,
    caps: *mut gst_ffi::GstCaps,
    size: usize,
    othercaps: *mut gst_ffi::GstCaps,
    othersize: *mut usize,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    panic_to_error!(&wrap, &element.panicked, false, {
        match imp.transform_size(
            &wrap,
            from_glib(direction),
            &from_glib_borrow(caps),
            size,
            &from_glib_borrow(othercaps),
        ) {
            Some(s) => {
                *othersize = s;
                true
            }
            None => false,
        }
    }).to_glib()
}

unsafe extern "C" fn base_transform_get_unit_size<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
    caps: *mut gst_ffi::GstCaps,
    size: *mut usize,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    panic_to_error!(&wrap, &element.panicked, false, {
        match imp.get_unit_size(&wrap, &from_glib_borrow(caps)) {
            Some(s) => {
                *size = s;
                true
            }
            None => false,
        }
    }).to_glib()
}

unsafe extern "C" fn base_transform_sink_event<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
    event: *mut gst_ffi::GstEvent,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    panic_to_error!(&wrap, &element.panicked, false, {
        imp.sink_event(&wrap, from_glib_full(event))
    }).to_glib()
}

unsafe extern "C" fn base_transform_src_event<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
    event: *mut gst_ffi::GstEvent,
) -> glib_ffi::gboolean
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    panic_to_error!(&wrap, &element.panicked, false, {
        imp.src_event(&wrap, from_glib_full(event))
    }).to_glib()
}

unsafe extern "C" fn base_transform_transform<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
    inbuf: *mut gst_ffi::GstBuffer,
    outbuf: *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    panic_to_error!(&wrap, &element.panicked, gst::FlowReturn::Error, {
        imp.transform(
            &wrap,
            &from_glib_borrow(inbuf),
            gst::BufferRef::from_mut_ptr(outbuf),
        )
    }).to_glib()
}

unsafe extern "C" fn base_transform_transform_ip<T: BaseTransformBase>(
    ptr: *mut gst_base_ffi::GstBaseTransform,
    buf: *mut *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: BaseTransformImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = element.imp.as_ref();

    // FIXME: Wrong signature in FFI
    let buf = buf as *mut gst_ffi::GstBuffer;

    panic_to_error!(&wrap, &element.panicked, gst::FlowReturn::Error, {
        if from_glib(gst_base_ffi::gst_base_transform_is_passthrough(ptr)) {
            imp.transform_ip_passthrough(&wrap, gst::BufferRef::from_ptr(buf))
        } else {
            imp.transform_ip(&wrap, gst::BufferRef::from_mut_ptr(buf))
        }
    }).to_glib()
}
