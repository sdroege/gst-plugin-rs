// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//               2016 Luis de Bethencourt <luisbg@osg.samsung.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#[macro_use]
extern crate bitflags;
extern crate byteorder;
#[macro_use]
extern crate derivative;
#[macro_use]
extern crate lazy_static;
extern crate libc;
extern crate num_rational;
#[macro_use]
extern crate slog;
extern crate url;
pub extern crate glib_sys as glib_ffi;
pub extern crate gobject_sys as gobject_ffi;
pub extern crate gstreamer_base_sys as gst_base_ffi;
pub extern crate gstreamer_sys as gst_ffi;

pub extern crate glib as glib;
pub extern crate gstreamer as gst;

#[macro_use]
pub mod utils;
#[macro_use]
pub mod error;
pub mod buffer;
pub mod adapter;
#[macro_use]
pub mod plugin;
pub mod source;
pub mod sink;
pub mod demuxer;
pub mod log;
pub mod value;
pub mod caps;
pub mod bytes;
pub mod tags;
pub mod streams;
pub mod miniobject;
pub mod structure;

pub mod ffi {
    pub use glib_ffi as glib;
    pub use gobject_ffi as gobject;
    pub use gst_ffi as gst;
}
