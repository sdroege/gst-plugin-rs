// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ffi::CString;
use std::ptr;
use std::mem;
use std::sync::atomic::AtomicBool;

use glib_ffi;
use gobject_ffi;

use glib;
use glib::translate::*;

pub trait ImplTypeStatic<T: ObjectType>: Send + Sync + 'static {
    fn get_name(&self) -> &str;
    fn new(&self, &T::RsType) -> T::ImplType;
    fn class_init(&self, &mut ClassStruct<T>);
}

pub trait ObjectType: 'static
where
    Self: Sized,
{
    const NAME: &'static str;
    type GlibType;
    type GlibClassType;
    type RsType: FromGlibPtrBorrow<*mut InstanceStruct<Self>>;
    type ImplType: 'static;

    fn glib_type() -> glib::Type;

    fn class_init(klass: &mut Self::GlibClassType);
}

#[repr(C)]
pub struct InstanceStruct<T: ObjectType> {
    pub parent: T::GlibType,
    pub imp: *const T::ImplType,
    pub panicked: AtomicBool,
}

impl<T: ObjectType> InstanceStruct<T> {
    pub fn get_impl(&self) -> &T::ImplType {
        unsafe { &*self.imp }
    }
}

#[repr(C)]
pub struct ClassStruct<T: ObjectType> {
    pub parent: T::GlibClassType,
    pub imp_static: *const Box<ImplTypeStatic<T>>,
}

unsafe extern "C" fn class_init<T: ObjectType>(
    klass: glib_ffi::gpointer,
    _klass_data: glib_ffi::gpointer,
) {
    callback_guard!();
    {
        let gobject_klass = &mut *(klass as *mut gobject_ffi::GObjectClass);

        gobject_klass.finalize = Some(finalize::<T>);
    }

    T::class_init(&mut *(klass as *mut T::GlibClassType));
}

unsafe extern "C" fn finalize<T: ObjectType>(obj: *mut gobject_ffi::GObject) {
    callback_guard!();
    let instance = &mut *(obj as *mut InstanceStruct<T>);

    drop(Box::from_raw(instance.imp as *mut T::ImplType));
    instance.imp = ptr::null_mut();

    let klass = *(obj as *const glib_ffi::gpointer);
    let parent_klass = gobject_ffi::g_type_class_peek_parent(klass);
    let parent_klass =
        &*(gobject_ffi::g_type_class_peek_parent(parent_klass) as *const gobject_ffi::GObjectClass);
    parent_klass.finalize.map(|f| f(obj));
}

pub unsafe fn get_type<T: ObjectType>() -> glib_ffi::GType {
    use std::sync::{Once, ONCE_INIT};

    static mut TYPE: glib_ffi::GType = gobject_ffi::G_TYPE_INVALID;
    static ONCE: Once = ONCE_INIT;

    ONCE.call_once(|| {
        let type_info = gobject_ffi::GTypeInfo {
            class_size: mem::size_of::<ClassStruct<T>>() as u16,
            base_init: None,
            base_finalize: None,
            class_init: Some(class_init::<T>),
            class_finalize: None,
            class_data: ptr::null_mut(),
            instance_size: mem::size_of::<InstanceStruct<T>>() as u16,
            n_preallocs: 0,
            instance_init: None,
            value_table: ptr::null(),
        };

        let type_name = {
            let mut idx = 0;

            loop {
                let type_name = CString::new(format!("{}-{}", T::NAME, idx)).unwrap();
                if gobject_ffi::g_type_from_name(type_name.as_ptr()) == gobject_ffi::G_TYPE_INVALID
                {
                    break type_name;
                }
                idx += 1;
            }
        };

        TYPE = gobject_ffi::g_type_register_static(
            T::glib_type().to_glib(),
            type_name.as_ptr(),
            &type_info,
            gobject_ffi::GTypeFlags::empty(),
        );
    });

    TYPE
}

unsafe extern "C" fn sub_class_init<T: ObjectType>(
    klass: glib_ffi::gpointer,
    klass_data: glib_ffi::gpointer,
) {
    callback_guard!();
    {
        let klass = &mut *(klass as *mut ClassStruct<T>);
        let imp_static = klass_data as *const Box<ImplTypeStatic<T>>;
        klass.imp_static = imp_static;

        (*imp_static).class_init(klass);
    }
}

unsafe extern "C" fn sub_init<T: ObjectType>(
    obj: *mut gobject_ffi::GTypeInstance,
    _klass: glib_ffi::gpointer,
) {
    callback_guard!();
    let instance = &mut *(obj as *mut InstanceStruct<T>);
    let klass = &**(obj as *const *const ClassStruct<T>);
    let rs_instance: T::RsType = from_glib_borrow(obj as *mut InstanceStruct<T>);

    let imp = (*klass.imp_static).new(&rs_instance);
    instance.imp = Box::into_raw(Box::new(imp));
}

pub fn register_type<T: ObjectType, I: ImplTypeStatic<T>>(imp: I) -> glib::Type {
    unsafe {
        let parent_type = get_type::<T>();
        let type_name = format!("{}-{}", T::NAME, imp.get_name());

        let imp: Box<ImplTypeStatic<T>> = Box::new(imp);

        let type_info = gobject_ffi::GTypeInfo {
            class_size: mem::size_of::<ClassStruct<T>>() as u16,
            base_init: None,
            base_finalize: None,
            class_init: Some(sub_class_init::<T>),
            class_finalize: None,
            class_data: Box::into_raw(Box::new(imp)) as glib_ffi::gpointer,
            instance_size: mem::size_of::<InstanceStruct<T>>() as u16,
            n_preallocs: 0,
            instance_init: Some(sub_init::<T>),
            value_table: ptr::null(),
        };

        let type_ = gobject_ffi::g_type_register_static(
            parent_type,
            type_name.to_glib_none().0,
            &type_info,
            gobject_ffi::GTypeFlags::empty(),
        );

        from_glib(type_)
    }
}
