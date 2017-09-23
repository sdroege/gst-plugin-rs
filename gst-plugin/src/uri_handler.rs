// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use mopa;

use glib_ffi;
use gobject_ffi;
use gst_ffi;

use libc;
use glib;
use glib::translate::*;
use gst;
use gst::prelude::*;

use object::*;

pub trait URIHandlerImpl: mopa::Any + Send + Sync + 'static {
    fn get_uri(&self, element: &gst::URIHandler) -> Option<String>;
    fn set_uri(&self, element: &gst::URIHandler, uri: Option<String>) -> Result<(), glib::Error>;
}

pub trait URIHandlerImplStatic<T: ObjectType>: Send + Sync + 'static {
    fn get_impl(&self, imp: &T::ImplType) -> &URIHandlerImpl;
    fn get_type(&self) -> gst::URIType;
    fn get_protocols(&self) -> Vec<String>;
}

struct URIHandlerStatic<T: ObjectType> {
    imp_static: *const URIHandlerImplStatic<T>,
    protocols: *const Vec<*const libc::c_char>,
}

unsafe extern "C" fn uri_handler_get_type<T: ObjectType>(
    type_: glib_ffi::GType,
) -> gst_ffi::GstURIType {
    callback_guard!();
    let klass = gobject_ffi::g_type_class_peek(type_);
    let klass = &*(klass as *const ClassStruct<T>);
    let interface_static = klass.get_interface_static(gst_ffi::gst_uri_handler_get_type()) as
        *const URIHandlerStatic<T>;
    (*(*interface_static).imp_static).get_type().to_glib()
}

unsafe extern "C" fn uri_handler_get_protocols<T: ObjectType>(
    type_: glib_ffi::GType,
) -> *const *const libc::c_char {
    callback_guard!();
    let klass = gobject_ffi::g_type_class_peek(type_);
    let klass = &*(klass as *const ClassStruct<T>);
    let interface_static = klass.get_interface_static(gst_ffi::gst_uri_handler_get_type()) as
        *const URIHandlerStatic<T>;
    (*(*interface_static).protocols).as_ptr()
}

unsafe extern "C" fn uri_handler_get_uri<T: ObjectType>(
    uri_handler: *mut gst_ffi::GstURIHandler,
) -> *mut libc::c_char {
    callback_guard!();
    let klass = &**(uri_handler as *const *const ClassStruct<T>);
    let interface_static = klass.get_interface_static(gst_ffi::gst_uri_handler_get_type()) as
        *const URIHandlerStatic<T>;

    let instance = &*(uri_handler as *const InstanceStruct<T>);
    let imp = instance.get_impl();
    let imp = (*(*interface_static).imp_static).get_impl(imp);

    imp.get_uri(&from_glib_borrow(uri_handler)).to_glib_full()
}

unsafe extern "C" fn uri_handler_set_uri<T: ObjectType>(
    uri_handler: *mut gst_ffi::GstURIHandler,
    uri: *const libc::c_char,
    err: *mut *mut glib_ffi::GError,
) -> glib_ffi::gboolean {
    callback_guard!();

    let klass = &**(uri_handler as *const *const ClassStruct<T>);
    let interface_static = klass.get_interface_static(gst_ffi::gst_uri_handler_get_type()) as
        *const URIHandlerStatic<T>;

    let instance = &*(uri_handler as *const InstanceStruct<T>);
    let imp = instance.get_impl();
    let imp = (*(*interface_static).imp_static).get_impl(imp);

    match imp.set_uri(&from_glib_borrow(uri_handler), from_glib_none(uri)) {
        Ok(()) => true.to_glib(),
        Err(error) => {
            *err = error.to_glib_full() as *mut _;
            false.to_glib()
        }
    }
}

unsafe extern "C" fn uri_handler_init<T: ObjectType>(
    iface: glib_ffi::gpointer,
    iface_data: glib_ffi::gpointer,
) {
    callback_guard!();
    let uri_handler_iface = &mut *(iface as *mut gst_ffi::GstURIHandlerInterface);

    let iface_type = (*(iface as *const gobject_ffi::GTypeInterface)).g_type;
    let type_ = (*(iface as *const gobject_ffi::GTypeInterface)).g_instance_type;
    let klass = &mut *(gobject_ffi::g_type_class_ref(type_) as *mut ClassStruct<T>);
    let interfaces_static = &mut *(klass.interfaces_static as *mut Vec<_>);
    interfaces_static.push((iface_type, iface_data));

    uri_handler_iface.get_type = Some(uri_handler_get_type::<T>);
    uri_handler_iface.get_protocols = Some(uri_handler_get_protocols::<T>);
    uri_handler_iface.get_uri = Some(uri_handler_get_uri::<T>);
    uri_handler_iface.set_uri = Some(uri_handler_set_uri::<T>);
}

pub fn register_uri_handler<T: ObjectType, I: URIHandlerImplStatic<T>>(
    _: &TypeInitToken,
    type_: glib::Type,
    imp: &I,
) {
    unsafe {
        let protocols: Vec<_> = imp.get_protocols()
            .iter()
            .map(|s| s.to_glib_full())
            .collect();

        let imp = imp as &URIHandlerImplStatic<T> as *const URIHandlerImplStatic<T>;
        let interface_static = Box::new(URIHandlerStatic {
            imp_static: imp,
            protocols: Box::into_raw(Box::new(protocols)),
        });

        let iface_info = gobject_ffi::GInterfaceInfo {
            interface_init: Some(uri_handler_init::<T>),
            interface_finalize: None,
            interface_data: Box::into_raw(interface_static) as glib_ffi::gpointer,
        };
        gobject_ffi::g_type_add_interface_static(
            type_.to_glib(),
            gst_ffi::gst_uri_handler_get_type(),
            &iface_info,
        );
    }
}
