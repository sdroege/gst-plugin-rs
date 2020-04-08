// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

#[macro_use]
extern crate glib;
#[macro_use]
extern crate gstreamer as gst;

extern crate gstreamer_audio as gst_audio;
extern crate gstreamer_video as gst_video;

#[cfg(not(feature = "v1_18"))]
extern crate glib_sys;
#[cfg(not(feature = "v1_18"))]
extern crate gobject_sys;
#[cfg(feature = "v1_18")]
extern crate gstreamer_base as gst_base;
#[cfg(not(feature = "v1_18"))]
extern crate gstreamer_sys as gst_sys;
#[cfg(not(feature = "v1_18"))]
#[allow(dead_code)]
mod base;
#[cfg(not(feature = "v1_18"))]
mod gst_base {
    pub use super::base::*;
}

extern crate once_cell;

mod fallbacksrc;
mod fallbackswitch;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    fallbackswitch::register(plugin)?;
    fallbacksrc::register(plugin)?;
    Ok(())
}

gst_plugin_define!(
    fallbackswitch,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "LGPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
