// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::mem;
use std::ptr;

use libc;

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

pub trait AggregatorImpl<T: AggregatorBase>:
    AnyImpl + ObjectImpl<T> + ElementImpl<T> + Send + Sync + 'static
where
    T::InstanceStructType: PanicPoison,
{
    fn flush(&self, aggregator: &T) -> gst::FlowReturn {
        aggregator.parent_flush()
    }

    fn clip(
        &self,
        aggregator: &T,
        aggregator_pad: &gst_base::AggregatorPad,
        buffer: gst::Buffer,
    ) -> Option<gst::Buffer> {
        aggregator.parent_clip(aggregator_pad, buffer)
    }

    fn finish_buffer(&self, aggregator: &T, buffer: gst::Buffer) -> gst::FlowReturn {
        aggregator.parent_finish_buffer(buffer)
    }

    fn sink_event(
        &self,
        aggregator: &T,
        aggregator_pad: &gst_base::AggregatorPad,
        event: gst::Event,
    ) -> bool {
        aggregator.parent_sink_event(aggregator_pad, event)
    }

    fn sink_query(
        &self,
        aggregator: &T,
        aggregator_pad: &gst_base::AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        aggregator.parent_sink_query(aggregator_pad, query)
    }

    fn src_event(&self, aggregator: &T, event: gst::Event) -> bool {
        aggregator.parent_src_event(event)
    }

    fn src_query(&self, aggregator: &T, query: &mut gst::QueryRef) -> bool {
        aggregator.parent_src_query(query)
    }

    fn src_activate(&self, aggregator: &T, mode: gst::PadMode, active: bool) -> bool {
        aggregator.parent_src_activate(mode, active)
    }

    fn aggregate(&self, aggregator: &T, timeout: bool) -> gst::FlowReturn;

    fn start(&self, aggregator: &T) -> bool {
        aggregator.parent_start()
    }

    fn stop(&self, aggregator: &T) -> bool {
        aggregator.parent_stop()
    }

    fn get_next_time(&self, aggregator: &T) -> gst::ClockTime {
        aggregator.parent_get_next_time()
    }

    fn create_new_pad(
        &self,
        aggregator: &T,
        templ: &gst::PadTemplate,
        req_name: Option<&str>,
        caps: Option<&gst::CapsRef>,
    ) -> Option<gst_base::AggregatorPad> {
        aggregator.parent_create_new_pad(templ, req_name, caps)
    }

    fn update_src_caps(
        &self,
        aggregator: &T,
        caps: &gst::CapsRef,
    ) -> Result<gst::Caps, gst::FlowError> {
        aggregator.parent_update_src_caps(caps)
    }

    fn fixate_src_caps(&self, aggregator: &T, caps: gst::Caps) -> gst::Caps {
        aggregator.parent_fixate_src_caps(caps)
    }

    fn negotiated_src_caps(&self, aggregator: &T, caps: &gst::CapsRef) -> bool {
        aggregator.parent_negotiated_src_caps(caps)
    }
}

any_impl!(AggregatorBase, AggregatorImpl, PanicPoison);

pub unsafe trait AggregatorBase:
    IsA<gst::Element> + IsA<gst_base::Aggregator> + ObjectType
{
    fn parent_flush(&self) -> gst::FlowReturn {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .flush
                .map(|f| from_glib(f(self.to_glib_none().0)))
                .unwrap_or(gst::FlowReturn::Ok)
        }
    }

    fn parent_clip(
        &self,
        aggregator_pad: &gst_base::AggregatorPad,
        buffer: gst::Buffer,
    ) -> Option<gst::Buffer> {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            match (*parent_klass).clip {
                None => Some(buffer),
                Some(ref func) => from_glib_full(func(
                    self.to_glib_none().0,
                    aggregator_pad.to_glib_none().0,
                    buffer.into_ptr(),
                )),
            }
        }
    }

    fn parent_finish_buffer(&self, buffer: gst::Buffer) -> gst::FlowReturn {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .finish_buffer
                .map(|f| from_glib(f(self.to_glib_none().0, buffer.into_ptr())))
                .unwrap_or(gst::FlowReturn::Ok)
        }
    }

    fn parent_sink_event(
        &self,
        aggregator_pad: &gst_base::AggregatorPad,
        event: gst::Event,
    ) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .sink_event
                .map(|f| {
                    from_glib(f(
                        self.to_glib_none().0,
                        aggregator_pad.to_glib_none().0,
                        event.into_ptr(),
                    ))
                })
                .unwrap_or(false)
        }
    }

    fn parent_sink_query(
        &self,
        aggregator_pad: &gst_base::AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .sink_query
                .map(|f| {
                    from_glib(f(
                        self.to_glib_none().0,
                        aggregator_pad.to_glib_none().0,
                        query.as_mut_ptr(),
                    ))
                })
                .unwrap_or(false)
        }
    }

    fn parent_src_event(&self, event: gst::Event) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .src_event
                .map(|f| from_glib(f(self.to_glib_none().0, event.into_ptr())))
                .unwrap_or(false)
        }
    }

    fn parent_src_query(&self, query: &mut gst::QueryRef) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .src_query
                .map(|f| from_glib(f(self.to_glib_none().0, query.as_mut_ptr())))
                .unwrap_or(false)
        }
    }

    fn parent_src_activate(&self, mode: gst::PadMode, active: bool) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .src_activate
                .map(|f| from_glib(f(self.to_glib_none().0, mode.to_glib(), active.to_glib())))
                .unwrap_or(false)
        }
    }

    fn parent_aggregate(&self, timeout: bool) -> gst::FlowReturn {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .aggregate
                .map(|f| from_glib(f(self.to_glib_none().0, timeout.to_glib())))
                .unwrap_or(gst::FlowReturn::Error)
        }
    }

    fn parent_start(&self) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .start
                .map(|f| from_glib(f(self.to_glib_none().0)))
                .unwrap_or(false)
        }
    }

    fn parent_stop(&self) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .stop
                .map(|f| from_glib(f(self.to_glib_none().0)))
                .unwrap_or(false)
        }
    }

    fn parent_get_next_time(&self) -> gst::ClockTime {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .get_next_time
                .map(|f| from_glib(f(self.to_glib_none().0)))
                .unwrap_or(gst::CLOCK_TIME_NONE)
        }
    }

    fn parent_create_new_pad(
        &self,
        templ: &gst::PadTemplate,
        req_name: Option<&str>,
        caps: Option<&gst::CapsRef>,
    ) -> Option<gst_base::AggregatorPad> {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .create_new_pad
                .map(|f| {
                    from_glib_full(f(
                        self.to_glib_none().0,
                        templ.to_glib_none().0,
                        req_name.to_glib_none().0,
                        caps.map(|c| c.as_ptr()).unwrap_or(ptr::null()),
                    ))
                })
                .unwrap_or(None)
        }
    }

    fn parent_update_src_caps(&self, caps: &gst::CapsRef) -> Result<gst::Caps, gst::FlowError> {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .update_src_caps
                .map(|f| {
                    let mut out_caps = ptr::null_mut();
                    let flow_ret =
                        from_glib(f(self.to_glib_none().0, caps.as_mut_ptr(), &mut out_caps));
                    flow_ret.into_result_value(|| from_glib_full(out_caps))
                })
                .unwrap_or(Err(gst::FlowError::Error))
        }
    }

    fn parent_fixate_src_caps(&self, caps: gst::Caps) -> gst::Caps {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;

            match (*parent_klass).fixate_src_caps {
                Some(ref f) => from_glib_full(f(self.to_glib_none().0, caps.into_ptr())),
                None => caps,
            }
        }
    }

    fn parent_negotiated_src_caps(&self, caps: &gst::CapsRef) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorClass;
            (*parent_klass)
                .negotiated_src_caps
                .map(|f| from_glib(f(self.to_glib_none().0, caps.as_mut_ptr())))
                .unwrap_or(false)
        }
    }
}

pub unsafe trait AggregatorClassExt<T: AggregatorBase>
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    fn override_vfuncs(&mut self, _: &ClassInitToken) {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gst_base_ffi::GstAggregatorClass);
            klass.flush = Some(aggregator_flush::<T>);
            klass.clip = Some(aggregator_clip::<T>);
            klass.finish_buffer = Some(aggregator_finish_buffer::<T>);
            klass.sink_event = Some(aggregator_sink_event::<T>);
            klass.sink_query = Some(aggregator_sink_query::<T>);
            klass.src_event = Some(aggregator_src_event::<T>);
            klass.src_query = Some(aggregator_src_query::<T>);
            klass.src_activate = Some(aggregator_src_activate::<T>);
            klass.aggregate = Some(aggregator_aggregate::<T>);
            klass.start = Some(aggregator_start::<T>);
            klass.stop = Some(aggregator_stop::<T>);
            klass.get_next_time = Some(aggregator_get_next_time::<T>);
            klass.create_new_pad = Some(aggregator_create_new_pad::<T>);
            klass.update_src_caps = Some(aggregator_update_src_caps::<T>);
            klass.fixate_src_caps = Some(aggregator_fixate_src_caps::<T>);
            klass.negotiated_src_caps = Some(aggregator_negotiated_src_caps::<T>);
        }
    }
}

glib_wrapper! {
    pub struct Aggregator(Object<ElementInstanceStruct<Aggregator>>):
        [gst_base::Aggregator => gst_base_ffi::GstAggregator,
         gst::Element => gst_ffi::GstElement,
         gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || get_type::<Aggregator>(),
    }
}

unsafe impl<T: IsA<gst::Element> + IsA<gst_base::Aggregator> + ObjectType> AggregatorBase for T {}
pub type AggregatorClass = ClassStruct<Aggregator>;

// FIXME: Boilerplate
unsafe impl AggregatorClassExt<Aggregator> for AggregatorClass {}
unsafe impl ElementClassExt<Aggregator> for AggregatorClass {}
unsafe impl ObjectClassExt<Aggregator> for AggregatorClass {}

unsafe impl Send for Aggregator {}
unsafe impl Sync for Aggregator {}

#[macro_export]
macro_rules! box_aggregator_impl(
    ($name:ident) => {
        box_element_impl!($name);

        impl<T: AggregatorBase> AggregatorImpl<T> for Box<$name<T>>
        where
            T::InstanceStructType: PanicPoison
        {
            fn flush(&self, aggregator: &T) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.flush(aggregator)
            }

            fn clip(&self, aggregator: &T, aggregator_pad: &gst_base::AggregatorPad, buffer: gst::Buffer) -> Option<gst::Buffer> {
                let imp: &$name<T> = self.as_ref();
                imp.clip(aggregator, aggregator_pad, buffer)
            }

            fn finish_buffer(&self, aggregator: &T, buffer: gst::Buffer) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.finish_buffer(aggregator, buffer)
            }

            fn sink_event(&self, aggregator: &T, aggregator_pad: &gst_base::AggregatorPad, event: gst::Event) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.sink_event(aggregator, aggregator_pad, event)
            }

            fn sink_query(&self, aggregator: &T, aggregator_pad: &gst_base::AggregatorPad, query: &mut gst::QueryRef) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.sink_query(aggregator, aggregator_pad, query)
            }

            fn src_event(&self, aggregator: &T, event: gst::Event) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.src_event(aggregator, event)
            }

            fn src_query(&self, aggregator: &T, query: &mut gst::QueryRef) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.src_query(aggregator, query)
            }

            fn src_activate(&self, aggregator: &T, mode: gst::PadMode, active: bool) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.src_activate(aggregator, mode, active)
            }

            fn aggregate(&self, aggregator: &T, timeout: bool) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.aggregate(aggregator, timeout)
            }

            fn start(&self, aggregator: &T) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.start(aggregator)
            }

            fn stop(&self, aggregator: &T) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.stop(aggregator)
            }

            fn get_next_time(&self, aggregator: &T) -> gst::ClockTime {
                let imp: &$name<T> = self.as_ref();
                imp.get_next_time(aggregator)
            }

            fn create_new_pad(&self, aggregator: &T, templ: &gst::PadTemplate, req_name: Option<&str>, caps: Option<&gst::CapsRef>) -> Option<gst_base::AggregatorPad> {
                let imp: &$name<T> = self.as_ref();
                imp.create_new_pad(aggregator, templ, req_name, caps)
            }

            fn update_src_caps(&self, aggregator: &T, caps: &gst::CapsRef) -> Result<gst::Caps, gst::FlowReturn> {
                let imp: &$name<T> = self.as_ref();
                imp.update_src_caps(aggregator, caps)
            }

            fn fixate_src_caps(&self, aggregator: &T, caps: gst::Caps) -> gst::Caps {
                let imp: &$name<T> = self.as_ref();
                imp.fixate_src_caps(aggregator, caps)
            }

            fn negotiated_src_caps(&self, aggregator: &T, caps: &gst::CapsRef) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.negotiated_src_caps(aggregator, caps)
            }
        }
    };
);
box_aggregator_impl!(AggregatorImpl);

impl ObjectType for Aggregator {
    const NAME: &'static str = "RsAggregator";
    type ParentType = gst_base::Aggregator;
    type ImplType = Box<AggregatorImpl<Self>>;
    type InstanceStructType = ElementInstanceStruct<Self>;

    fn class_init(token: &ClassInitToken, klass: &mut AggregatorClass) {
        ElementClassExt::override_vfuncs(klass, token);
        AggregatorClassExt::override_vfuncs(klass, token);
    }

    object_type_fns!();
}

unsafe extern "C" fn aggregator_flush<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), gst::FlowReturn::Error, {
        imp.flush(&wrap)
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_clip<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
    aggregator_pad: *mut gst_base_ffi::GstAggregatorPad,
    buffer: *mut gst_ffi::GstBuffer,
) -> *mut gst_ffi::GstBuffer
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    let ret = panic_to_error!(&wrap, &aggregator.panicked(), None, {
        imp.clip(
            &wrap,
            &from_glib_borrow(aggregator_pad),
            from_glib_full(buffer),
        )
    });

    ret.map(|r| r.into_ptr()).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn aggregator_finish_buffer<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
    buffer: *mut gst_ffi::GstBuffer,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), gst::FlowReturn::Error, {
        imp.finish_buffer(&wrap, from_glib_full(buffer))
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_sink_event<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
    aggregator_pad: *mut gst_base_ffi::GstAggregatorPad,
    event: *mut gst_ffi::GstEvent,
) -> glib_ffi::gboolean
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), false, {
        imp.sink_event(
            &wrap,
            &from_glib_borrow(aggregator_pad),
            from_glib_full(event),
        )
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_sink_query<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
    aggregator_pad: *mut gst_base_ffi::GstAggregatorPad,
    query: *mut gst_ffi::GstQuery,
) -> glib_ffi::gboolean
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), false, {
        imp.sink_query(
            &wrap,
            &from_glib_borrow(aggregator_pad),
            gst::QueryRef::from_mut_ptr(query),
        )
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_src_event<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
    event: *mut gst_ffi::GstEvent,
) -> glib_ffi::gboolean
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), false, {
        imp.src_event(&wrap, from_glib_full(event))
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_src_query<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
    query: *mut gst_ffi::GstQuery,
) -> glib_ffi::gboolean
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), false, {
        imp.src_query(&wrap, gst::QueryRef::from_mut_ptr(query))
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_src_activate<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
    mode: gst_ffi::GstPadMode,
    active: glib_ffi::gboolean,
) -> glib_ffi::gboolean
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), false, {
        imp.src_activate(&wrap, from_glib(mode), from_glib(active))
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_aggregate<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
    timeout: glib_ffi::gboolean,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), gst::FlowReturn::Error, {
        imp.aggregate(&wrap, from_glib(timeout))
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_start<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
) -> glib_ffi::gboolean
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), false, { imp.start(&wrap) }).to_glib()
}

unsafe extern "C" fn aggregator_stop<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
) -> glib_ffi::gboolean
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), false, { imp.stop(&wrap) }).to_glib()
}

unsafe extern "C" fn aggregator_get_next_time<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
) -> gst_ffi::GstClockTime
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), gst::CLOCK_TIME_NONE, {
        imp.get_next_time(&wrap)
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_create_new_pad<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
    templ: *mut gst_ffi::GstPadTemplate,
    req_name: *const libc::c_char,
    caps: *const gst_ffi::GstCaps,
) -> *mut gst_base_ffi::GstAggregatorPad
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), None, {
        let req_name: Option<String> = from_glib_none(req_name);

        // FIXME: Easier way to convert Option<String> to Option<&str>?
        let mut _tmp = String::new();
        let req_name = match req_name {
            Some(n) => {
                _tmp = n;
                Some(_tmp.as_str())
            }
            None => None,
        };

        imp.create_new_pad(
            &wrap,
            &from_glib_borrow(templ),
            req_name,
            if caps.is_null() {
                None
            } else {
                Some(gst::CapsRef::from_ptr(caps))
            },
        )
    })
    .to_glib_full()
}

unsafe extern "C" fn aggregator_update_src_caps<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
    caps: *mut gst_ffi::GstCaps,
    res: *mut *mut gst_ffi::GstCaps,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    *res = ptr::null_mut();

    panic_to_error!(&wrap, &aggregator.panicked(), gst::FlowReturn::Error, {
        match imp.update_src_caps(&wrap, gst::CapsRef::from_ptr(caps)) {
            Ok(res_caps) => {
                *res = res_caps.into_ptr();
                gst::FlowReturn::Ok
            }
            Err(err) => err,
        }
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_fixate_src_caps<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
    caps: *mut gst_ffi::GstCaps,
) -> *mut gst_ffi::GstCaps
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), gst::Caps::new_empty(), {
        imp.fixate_src_caps(&wrap, from_glib_full(caps))
    })
    .into_ptr()
}

unsafe extern "C" fn aggregator_negotiated_src_caps<T: AggregatorBase>(
    ptr: *mut gst_base_ffi::GstAggregator,
    caps: *mut gst_ffi::GstCaps,
) -> glib_ffi::gboolean
where
    T::ImplType: AggregatorImpl<T>,
    T::InstanceStructType: PanicPoison,
{
    floating_reference_guard!(ptr);
    let aggregator = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator.get_impl();

    panic_to_error!(&wrap, &aggregator.panicked(), false, {
        imp.negotiated_src_caps(&wrap, gst::CapsRef::from_ptr(caps))
    })
    .to_glib()
}
