use std::ptr;
use std::sync::atomic::AtomicBool;

use glib::wrapper::Wrapper;
use gobject_subclass::object::*;

#[repr(C)]
pub struct ElementInstanceStruct<T: ObjectType> {
    _parent: <T::ParentType as Wrapper>::GlibType,
    _imp: ptr::NonNull<T::ImplType>,

    _panicked: AtomicBool,
}

pub trait PanicPoison {
    fn panicked(&self) -> &AtomicBool;
}

unsafe impl<T: ObjectType> Instance<T> for ElementInstanceStruct<T> {
    fn parent(&self) -> &<T::ParentType as Wrapper>::GlibType {
        &self._parent
    }

    fn get_impl(&self) -> &T::ImplType {
        unsafe { self._imp.as_ref() }
    }

    unsafe fn set_impl(&mut self, imp: ptr::NonNull<T::ImplType>) {
        self._imp = imp;
    }

    unsafe fn get_class(&self) -> *const ClassStruct<T> {
        *(self as *const _ as *const *const ClassStruct<T>)
    }
}

impl<T: ObjectType> PanicPoison for ElementInstanceStruct<T> {
    fn panicked(&self) -> &AtomicBool {
        &self._panicked
    }
}
