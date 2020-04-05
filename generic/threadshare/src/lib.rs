// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
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

//! A collection of GStreamer plugins which leverage the `threadshare` [`runtime`].
//!
//! [`runtime`]: runtime/index.html

// Needed for `select!` in `Socket::next`
// see https://docs.rs/futures/0.3.1/futures/macro.select.html
#![recursion_limit = "1024"]
#![crate_type = "cdylib"]

pub use tokio;

#[macro_use]
pub mod runtime;

pub mod socket;
mod tcpclientsrc;
mod udpsink;
mod udpsrc;

mod appsrc;
pub mod dataqueue;
mod inputselector;
mod jitterbuffer;
mod proxy;
mod queue;

use glib::translate::*;
use glib_sys as glib_ffi;

use gst;
use gst::gst_plugin_define;
use gst::prelude::*;
use gstreamer_sys as gst_ffi;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    udpsrc::register(plugin)?;
    udpsink::register(plugin)?;
    tcpclientsrc::register(plugin)?;
    queue::register(plugin)?;
    proxy::register(plugin)?;
    appsrc::register(plugin)?;
    jitterbuffer::jitterbuffer::register(plugin)?;
    inputselector::register(plugin)?;

    Ok(())
}

gst_plugin_define!(
    threadshare,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "LGPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);

pub fn set_element_flags<T: glib::IsA<gst::Object> + glib::IsA<gst::Element>>(
    element: &T,
    flags: gst::ElementFlags,
) {
    unsafe {
        let ptr: *mut gst_ffi::GstObject = element.as_ptr() as *mut _;
        let _guard = MutexGuard::lock(&(*ptr).lock);
        (*ptr).flags |= flags.to_glib();
    }
}

struct MutexGuard<'a>(&'a glib_ffi::GMutex);

impl<'a> MutexGuard<'a> {
    pub fn lock(mutex: &'a glib_ffi::GMutex) -> Self {
        unsafe {
            glib_ffi::g_mutex_lock(mut_override(mutex));
        }
        MutexGuard(mutex)
    }
}

impl<'a> Drop for MutexGuard<'a> {
    fn drop(&mut self) {
        unsafe {
            glib_ffi::g_mutex_unlock(mut_override(self.0));
        }
    }
}

pub fn get_current_running_time(element: &gst::Element) -> gst::ClockTime {
    if let Some(clock) = element.get_clock() {
        if clock.get_time() > element.get_base_time() {
            clock.get_time() - element.get_base_time()
        } else {
            0.into()
        }
    } else {
        gst::CLOCK_TIME_NONE
    }
}
