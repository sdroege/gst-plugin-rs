// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
use glib_ffi;
use gobject_ffi;
use gst_ffi;

use glib;
use glib::translate::*;
use gst;
use libc;

use gobject_subclass::anyimpl::*;
use gobject_subclass::object::*;

pub trait ChildProxyImpl: AnyImpl + Send + Sync + 'static {
    fn get_child_by_name(&self, object: &gst::ChildProxy, name: &str) -> Option<glib::Object> {
        unsafe {
            let type_ = gst_ffi::gst_child_proxy_get_type();
            let iface = gobject_ffi::g_type_default_interface_ref(type_)
                as *mut gst_ffi::GstChildProxyInterface;
            assert!(!iface.is_null());

            let ret = ((*iface).get_child_by_name.as_ref().unwrap())(
                object.to_glib_none().0,
                name.to_glib_none().0,
            );

            gobject_ffi::g_type_default_interface_unref(iface as glib_ffi::gpointer);

            from_glib_full(ret)
        }
    }

    fn get_child_by_index(&self, object: &gst::ChildProxy, index: u32) -> Option<glib::Object>;
    fn get_children_count(&self, object: &gst::ChildProxy) -> u32;

    fn child_added(&self, object: &gst::ChildProxy, child: &glib::Object, name: &str);
    fn child_removed(&self, object: &gst::ChildProxy, child: &glib::Object, name: &str);
}

any_impl!(ChildProxyImpl);

pub trait ChildProxyImplStatic<T: ObjectType>: Send + Sync + 'static {
    fn get_impl<'a>(&self, imp: &'a T::ImplType) -> &'a ChildProxyImpl;
}

struct ChildProxyStatic<T: ObjectType> {
    imp_static: *const ChildProxyImplStatic<T>,
}

unsafe extern "C" fn child_proxy_get_child_by_name<T: ObjectType>(
    child_proxy: *mut gst_ffi::GstChildProxy,
    name: *const libc::c_char,
) -> *mut gobject_ffi::GObject {
    floating_reference_guard!(child_proxy);

    let klass = &**(child_proxy as *const *const ClassStruct<T>);
    let interface_static = klass.get_interface_static(gst_ffi::gst_child_proxy_get_type())
        as *const ChildProxyStatic<T>;

    let instance = &*(child_proxy as *const T::InstanceStructType);
    let imp = instance.get_impl();
    let imp = (*(*interface_static).imp_static).get_impl(imp);

    imp.get_child_by_name(
        &from_glib_borrow(child_proxy),
        String::from_glib_none(name).as_str(),
    ).to_glib_full()
}

unsafe extern "C" fn child_proxy_get_child_by_index<T: ObjectType>(
    child_proxy: *mut gst_ffi::GstChildProxy,
    index: u32,
) -> *mut gobject_ffi::GObject {
    floating_reference_guard!(child_proxy);

    let klass = &**(child_proxy as *const *const ClassStruct<T>);
    let interface_static = klass.get_interface_static(gst_ffi::gst_child_proxy_get_type())
        as *const ChildProxyStatic<T>;

    let instance = &*(child_proxy as *const T::InstanceStructType);
    let imp = instance.get_impl();
    let imp = (*(*interface_static).imp_static).get_impl(imp);

    imp.get_child_by_index(&from_glib_borrow(child_proxy), index)
        .to_glib_full()
}

unsafe extern "C" fn child_proxy_get_children_count<T: ObjectType>(
    child_proxy: *mut gst_ffi::GstChildProxy,
) -> u32 {
    floating_reference_guard!(child_proxy);

    let klass = &**(child_proxy as *const *const ClassStruct<T>);
    let interface_static = klass.get_interface_static(gst_ffi::gst_child_proxy_get_type())
        as *const ChildProxyStatic<T>;

    let instance = &*(child_proxy as *const T::InstanceStructType);
    let imp = instance.get_impl();
    let imp = (*(*interface_static).imp_static).get_impl(imp);

    imp.get_children_count(&from_glib_borrow(child_proxy))
}

unsafe extern "C" fn child_proxy_child_added<T: ObjectType>(
    child_proxy: *mut gst_ffi::GstChildProxy,
    child: *mut gobject_ffi::GObject,
    name: *const libc::c_char,
) {
    floating_reference_guard!(child_proxy);

    let klass = &**(child_proxy as *const *const ClassStruct<T>);
    let interface_static = klass.get_interface_static(gst_ffi::gst_child_proxy_get_type())
        as *const ChildProxyStatic<T>;

    let instance = &*(child_proxy as *const T::InstanceStructType);
    let imp = instance.get_impl();
    let imp = (*(*interface_static).imp_static).get_impl(imp);

    imp.child_added(
        &from_glib_borrow(child_proxy),
        &from_glib_borrow(child),
        String::from_glib_none(name).as_str(),
    )
}

unsafe extern "C" fn child_proxy_child_removed<T: ObjectType>(
    child_proxy: *mut gst_ffi::GstChildProxy,
    child: *mut gobject_ffi::GObject,
    name: *const libc::c_char,
) {
    floating_reference_guard!(child_proxy);

    let klass = &**(child_proxy as *const *const ClassStruct<T>);
    let interface_static = klass.get_interface_static(gst_ffi::gst_child_proxy_get_type())
        as *const ChildProxyStatic<T>;

    let instance = &*(child_proxy as *const T::InstanceStructType);
    let imp = instance.get_impl();
    let imp = (*(*interface_static).imp_static).get_impl(imp);

    imp.child_removed(
        &from_glib_borrow(child_proxy),
        &from_glib_borrow(child),
        String::from_glib_none(name).as_str(),
    )
}

unsafe extern "C" fn child_proxy_init<T: ObjectType>(
    iface: glib_ffi::gpointer,
    iface_data: glib_ffi::gpointer,
) {
    let child_proxy_iface = &mut *(iface as *mut gst_ffi::GstChildProxyInterface);

    let iface_type = (*(iface as *const gobject_ffi::GTypeInterface)).g_type;
    let type_ = (*(iface as *const gobject_ffi::GTypeInterface)).g_instance_type;
    let klass = &mut *(gobject_ffi::g_type_class_ref(type_) as *mut ClassStruct<T>);
    let interfaces_static = &mut *(klass.interfaces_static as *mut Vec<_>);
    interfaces_static.push((iface_type, iface_data));

    child_proxy_iface.get_child_by_name = Some(child_proxy_get_child_by_name::<T>);
    child_proxy_iface.get_child_by_index = Some(child_proxy_get_child_by_index::<T>);
    child_proxy_iface.get_children_count = Some(child_proxy_get_children_count::<T>);
    child_proxy_iface.child_added = Some(child_proxy_child_added::<T>);
    child_proxy_iface.child_removed = Some(child_proxy_child_removed::<T>);
}

pub fn register_child_proxy<T: ObjectType, I: ChildProxyImplStatic<T>>(
    _: &TypeInitToken,
    type_: glib::Type,
    imp: &I,
) {
    unsafe {
        let imp = imp as &ChildProxyImplStatic<T> as *const ChildProxyImplStatic<T>;
        let interface_static = Box::new(ChildProxyStatic { imp_static: imp });

        let iface_info = gobject_ffi::GInterfaceInfo {
            interface_init: Some(child_proxy_init::<T>),
            interface_finalize: None,
            interface_data: Box::into_raw(interface_static) as glib_ffi::gpointer,
        };
        gobject_ffi::g_type_add_interface_static(
            type_.to_glib(),
            gst_ffi::gst_child_proxy_get_type(),
            &iface_info,
        );
    }
}
