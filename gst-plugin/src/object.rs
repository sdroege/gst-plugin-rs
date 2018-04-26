use std::ptr;
use std::sync::atomic::AtomicBool;

pub use gobject_base::object::*;



#[repr(C)]
pub struct InstanceStruct<T: ObjectType>
{
    _parent: T::GlibType,
    _imp: ptr::NonNull<T::ImplType>,

    _panicked: AtomicBool,
}

pub trait PanicPoison{
    fn panicked(&mut self) -> &mut AtomicBool;
}


impl<T: ObjectType> Instance<T> for InstanceStruct<T>
{
    fn get_impl(&self) -> &T::ImplType {
        unsafe { self._imp.as_ref() }
    }

    fn get_impl_ptr(&self) -> *mut T::ImplType {
        self._imp.as_ptr()
    }

    fn set_impl(&mut self, imp:ptr::NonNull<T::ImplType>){
        self._imp = imp;
    }

    fn parent(&self) -> &T::GlibType{
        &self._parent
    }

    unsafe fn get_class(&self) -> *const ClassStruct<T> {
        *(self as *const _ as *const *const ClassStruct<T>)
    }

}


impl<T: ObjectType> PanicPoison for InstanceStruct<T>
{
    fn panicked(&mut self) -> &mut AtomicBool{
        self._panicked
    }

}
