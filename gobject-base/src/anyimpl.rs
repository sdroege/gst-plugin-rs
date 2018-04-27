// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
//
// Heavily inspired by the mopa trait

use std::any::{Any, TypeId};

pub trait AnyImpl: Any {
    fn get_type_id(&self) -> TypeId;
}

impl<T: Any> AnyImpl for T {
    fn get_type_id(&self) -> TypeId {
        TypeId::of::<T>()
    }
}

#[macro_export]
macro_rules! any_impl {
    ($bound:ident, $trait:ident, $constraints:ident) => {
        impl<T: $bound> $trait<T>
        where T::InstanceStructType: $constraints
         {
            #[inline]
            pub fn downcast_ref<U: $trait<T>>(&self) -> Option<&U> {
                if self.is::<U>() {
                    unsafe { Some(self.downcast_ref_unchecked()) }
                } else {
                    None
                }
            }

            #[inline]
            pub unsafe fn downcast_ref_unchecked<U: $trait<T>>(&self) -> &U {
                &*(self as *const Self as *const U)
            }

            #[inline]
            pub fn is<U: $trait<T>>(&self) -> bool {
                use std::any::TypeId;
                TypeId::of::<U>() == $crate::anyimpl::AnyImpl::get_type_id(self)
            }
        }
    };

    ($bound:ident, $trait:ident) => {
        impl<T: $bound> $trait<T>
         {
            #[inline]
            pub fn downcast_ref<U: $trait<T>>(&self) -> Option<&U> {
                if self.is::<U>() {
                    unsafe { Some(self.downcast_ref_unchecked()) }
                } else {
                    None
                }
            }

            #[inline]
            pub unsafe fn downcast_ref_unchecked<U: $trait<T>>(&self) -> &U {
                &*(self as *const Self as *const U)
            }

            #[inline]
            pub fn is<U: $trait<T>>(&self) -> bool {
                use std::any::TypeId;
                TypeId::of::<U>() == $crate::anyimpl::AnyImpl::get_type_id(self)
            }
        }
    };

    ($trait:ident) => {
        impl $trait {
            #[inline]
            pub fn downcast_ref<U: $trait>(&self) -> Option<&U> {
                if self.is::<U>() {
                    unsafe { Some(self.downcast_ref_unchecked()) }
                } else {
                    None
                }
            }

            #[inline]
            pub unsafe fn downcast_ref_unchecked<U: $trait>(&self) -> &U {
                &*(self as *const Self as *const U)
            }

            #[inline]
            pub fn is<U: $trait>(&self) -> bool {
                use std::any::TypeId;
                TypeId::of::<U>() == $crate::anyimpl::AnyImpl::get_type_id(self)
            }
        }
    };
}
