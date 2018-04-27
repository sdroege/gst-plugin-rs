use std::ptr;
use std::sync::atomic::AtomicBool;

pub use gobject_base::object::*;


#[repr(C)]
pub struct ElementInstanceStruct<T: ObjectType>
{
    _parent: T::GlibType,
    _imp: ptr::NonNull<T::ImplType>,

    _panicked: AtomicBool,
}

pub trait PanicPoison{
    fn panicked(&self) -> &AtomicBool;
}


impl<T: ObjectType> Instance<T> for ElementInstanceStruct<T>
{
    fn parent(&self) -> &T::GlibType{
        &self._parent
    }

    fn get_impl(&self) -> &T::ImplType {
        unsafe { self._imp.as_ref() }
    }

    fn get_impl_ptr(&self) -> *mut T::ImplType {
        self._imp.as_ptr()
    }

    unsafe fn set_impl(&mut self, imp:ptr::NonNull<T::ImplType>){
        self._imp = imp;
    }

    unsafe fn get_class(&self) -> *const ClassStruct<T> {
        *(self as *const _ as *const *const ClassStruct<T>)
    }
}


impl<T: ObjectType> PanicPoison for ElementInstanceStruct<T>
{
    fn panicked(&self) -> &AtomicBool{
        &self._panicked
    }

}
