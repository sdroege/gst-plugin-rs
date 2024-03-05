// SPDX-License-Identifier: MPL-2.0

use crate::signaller::Signallable;
use gst::{glib, glib::subclass::prelude::*};

mod imp;

// base class
glib::wrapper! {
    pub struct JanusVRSignaller(ObjectSubclass<imp::Signaller>) @implements Signallable;
}

trait JanusVRSignallerImpl: ObjectImpl {}

#[repr(C)]
pub struct JanusVRSignallerClass {
    parent: glib::object::ObjectClass,
}

unsafe impl ClassStruct for JanusVRSignallerClass {
    type Type = imp::Signaller;
}

impl std::ops::Deref for JanusVRSignallerClass {
    type Target = glib::Class<<<Self as ClassStruct>::Type as ObjectSubclass>::ParentType>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*(&self.parent as *const _ as *const _) }
    }
}

unsafe impl<T: JanusVRSignallerImpl> IsSubclassable<T> for JanusVRSignaller {
    fn class_init(class: &mut glib::Class<Self>) {
        Self::parent_class_init::<T>(class);
    }
}

impl Default for JanusVRSignaller {
    fn default() -> Self {
        glib::Object::new()
    }
}

// default signaller using `u64` ids
glib::wrapper! {
    pub struct JanusVRSignallerU64(ObjectSubclass<imp::signaller_u64::SignallerU64>) @extends JanusVRSignaller, @implements Signallable;
}

impl Default for JanusVRSignallerU64 {
    fn default() -> Self {
        glib::Object::new()
    }
}

// signaller using strings ids, used when `use-string-ids=true` is set on `janusvrwebrtcsink`
glib::wrapper! {
    pub struct JanusVRSignallerStr(ObjectSubclass<imp::signaller_str::SignallerStr>) @extends JanusVRSignaller, @implements Signallable;
}

impl Default for JanusVRSignallerStr {
    fn default() -> Self {
        glib::Object::new()
    }
}
