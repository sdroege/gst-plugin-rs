// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//               2016 Luis de Bethencourt <luisbg@osg.samsung.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

extern crate byteorder;
pub extern crate glib_sys as glib_ffi;
pub extern crate gobject_sys as gobject_ffi;
extern crate gstreamer_base_sys as gst_base_ffi;
pub extern crate gstreamer_sys as gst_ffi;
#[macro_use]
extern crate lazy_static;
extern crate libc;

#[macro_use]
pub extern crate glib;
#[macro_use]
pub extern crate gstreamer as gst;
extern crate gstreamer_base as gst_base;

#[macro_use]
extern crate gobject_base;

#[macro_use]
pub use gobject_base::anyimpl;
pub use gobject_base::properties;

#[macro_use]
pub use gobject_base::guard;

pub mod object;

#[macro_use]
pub mod error;
pub mod adapter;
#[macro_use]
pub mod plugin;
pub mod bytes;

#[macro_use]
pub mod element;
#[macro_use]
pub mod bin;
#[macro_use]
pub mod pipeline;
#[macro_use]
pub mod base_src;
#[macro_use]
pub mod base_sink;
#[macro_use]
pub mod base_transform;
pub mod uri_handler;
