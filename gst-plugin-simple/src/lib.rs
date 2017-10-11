// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

extern crate glib;
#[macro_use]
extern crate gstreamer as gst;
extern crate gstreamer_base as gst_base;
#[macro_use]
extern crate gst_plugin;

extern crate url;

pub mod source;
pub mod sink;
pub mod demuxer;

pub type UriValidator = Fn(&url::Url) -> Result<(), gst_plugin::error::UriError> + Send + Sync + 'static;
