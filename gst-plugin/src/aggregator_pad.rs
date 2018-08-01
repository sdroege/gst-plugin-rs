// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
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
use gst_ffi;
use gst_base_ffi;

use glib;
use glib::translate::*;
use gst;
use gst::prelude::*;
use gst_base;

use gobject_subclass::anyimpl::*;
use gobject_subclass::object::*;

use pad::*;

pub trait AggregatorPadImpl<T: AggregatorPadBase>:
    AnyImpl + ObjectImpl<T> + PadImpl<T> + Send + Sync + 'static
{
    fn flush(&self, aggregator_pad: &T, aggregator: &gst_base::Aggregator) -> gst::FlowReturn {
        aggregator_pad.parent_flush(aggregator)
    }

    fn skip_buffer(&self, aggregator_pad: &T, aggregator: &gst_base::Aggregator, buffer: &gst::BufferRef) -> bool {
        aggregator_pad.parent_skip_buffer(aggregator, buffer)
    }
}

any_impl!(AggregatorPadBase, AggregatorPadImpl);

pub unsafe trait AggregatorPadBase: IsA<gst_base::AggregatorPad> + IsA<gst::Pad> + ObjectType {
    fn parent_flush(&self, aggregator: &gst_base::Aggregator) -> gst::FlowReturn {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorPadClass;
            (*parent_klass)
                .flush
                .map(|f| from_glib(f(self.to_glib_none().0, aggregator.to_glib_none().0)))
                .unwrap_or(gst::FlowReturn::Ok)
        }
    }

    fn parent_skip_buffer(&self, aggregator: &gst_base::Aggregator, buffer: &gst::BufferRef) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass =
                (*klass).get_parent_class() as *const gst_base_ffi::GstAggregatorPadClass;
            (*parent_klass)
                .skip_buffer
                .map(|f| from_glib(f(self.to_glib_none().0, aggregator.to_glib_none().0, buffer.as_mut_ptr())))
                .unwrap_or(false)
        }
    }
}

pub unsafe trait AggregatorPadClassExt<T: AggregatorPadBase>
where
    T::ImplType: AggregatorPadImpl<T>,
{
    fn override_vfuncs(&mut self, _: &ClassInitToken) {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gst_base_ffi::GstAggregatorPadClass);
            klass.flush = Some(aggregator_pad_flush::<T>);
            klass.skip_buffer = Some(aggregator_pad_skip_buffer::<T>);
        }
    }
}

glib_wrapper! {
    pub struct AggregatorPad(Object<InstanceStruct<AggregatorPad>>):
        [gst_base::AggregatorPad => gst_base_ffi::GstAggregatorPad,
         gst::Pad => gst_ffi::GstPad,
         gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || get_type::<AggregatorPad>(),
    }
}

unsafe impl<T: IsA<gst_base::AggregatorPad> + IsA<gst::Pad> + ObjectType> AggregatorPadBase for T {}
pub type AggregatorPadClass = ClassStruct<AggregatorPad>;

// FIXME: Boilerplate
unsafe impl AggregatorPadClassExt<AggregatorPad> for AggregatorPadClass {}
unsafe impl PadClassExt<AggregatorPad> for AggregatorPadClass {}
unsafe impl ObjectClassExt<AggregatorPad> for AggregatorPadClass {}

unsafe impl Send for AggregatorPad {}
unsafe impl Sync for AggregatorPad {}

#[macro_export]
macro_rules! box_aggregator_pad_impl(
    ($name:ident) => {
        box_pad_impl!($name);

        impl<T: AggregatorPadBase> AggregatorPadImpl<T> for Box<$name<T>>
        {
            fn flush(&self, aggregator_pad: &T, aggregator: &gst_base::Aggregator) -> gst::FlowReturn {
                let imp: &$name<T> = self.as_ref();
                imp.flush(aggregator_pad, aggregator)
            }

            fn skip_buffer(&self, aggregator_pad: &T, aggregator: &gst_base::Aggregator, buffer: &gst::BufferRef) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.skip_buffer(aggregator_pad, aggregator, buffer)
            }
        }
    };
);
box_aggregator_pad_impl!(AggregatorPadImpl);

impl ObjectType for AggregatorPad {
    const NAME: &'static str = "RsAggregatorPad";
    type ParentType = gst_base::AggregatorPad;
    type ImplType = Box<AggregatorPadImpl<Self>>;
    type InstanceStructType = InstanceStruct<Self>;

    fn class_init(token: &ClassInitToken, klass: &mut AggregatorPadClass) {
        PadClassExt::override_vfuncs(klass, token);
        AggregatorPadClassExt::override_vfuncs(klass, token);
    }

    object_type_fns!();
}

unsafe extern "C" fn aggregator_pad_flush<T: AggregatorPadBase>(
    ptr: *mut gst_base_ffi::GstAggregatorPad,
    aggregator: *mut gst_base_ffi::GstAggregator,
) -> gst_ffi::GstFlowReturn
where
    T::ImplType: AggregatorPadImpl<T>,
{
    floating_reference_guard!(ptr);
    let aggregator_pad = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator_pad.get_impl();

    imp.flush(&wrap, &from_glib_borrow(aggregator)).to_glib()
}

unsafe extern "C" fn aggregator_pad_skip_buffer<T: AggregatorPadBase>(
    ptr: *mut gst_base_ffi::GstAggregatorPad,
    aggregator: *mut gst_base_ffi::GstAggregator,
    buffer: *mut gst_ffi::GstBuffer,
) -> glib_ffi::gboolean
where
    T::ImplType: AggregatorPadImpl<T>,
{
    floating_reference_guard!(ptr);
    let aggregator_pad = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = aggregator_pad.get_impl();

    imp.skip_buffer(&wrap, &from_glib_borrow(aggregator), gst::BufferRef::from_ptr(buffer)).to_glib()
}
