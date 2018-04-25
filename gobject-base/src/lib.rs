// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

extern crate byteorder;
pub extern crate glib_sys as glib_ffi;
pub extern crate gobject_sys as gobject_ffi;

#[macro_use]
extern crate lazy_static;
extern crate libc;

#[macro_use]
pub extern crate glib;

#[macro_use]
pub mod anyimpl;

#[macro_use]
pub mod guard;
pub use guard::FloatingReferenceGuard;

pub mod properties;
#[macro_use]
pub mod object;
