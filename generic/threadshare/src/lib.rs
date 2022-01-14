// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// Take a look at the license at the top of the repository in the LICENSE file.
#![allow(clippy::non_send_fields_in_send_ty)]

//! A collection of GStreamer plugins which leverage the `threadshare` [`runtime`].
//!
//! [`runtime`]: runtime/index.html

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
use gst::glib;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    udpsrc::register(plugin)?;
    udpsink::register(plugin)?;
    tcpclientsrc::register(plugin)?;
    queue::register(plugin)?;
    proxy::register(plugin)?;
    appsrc::register(plugin)?;
    jitterbuffer::register(plugin)?;
    inputselector::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
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
        let ptr: *mut gst::ffi::GstObject = element.as_ptr() as *mut _;
        let _guard = MutexGuard::lock(&(*ptr).lock);
        (*ptr).flags |= flags.into_glib();
    }
}

#[must_use = "if unused the Mutex will immediately unlock"]
struct MutexGuard<'a>(&'a glib::ffi::GMutex);

impl<'a> MutexGuard<'a> {
    pub fn lock(mutex: &'a glib::ffi::GMutex) -> Self {
        unsafe {
            glib::ffi::g_mutex_lock(mut_override(mutex));
        }
        MutexGuard(mutex)
    }
}

impl<'a> Drop for MutexGuard<'a> {
    fn drop(&mut self) {
        unsafe {
            glib::ffi::g_mutex_unlock(mut_override(self.0));
        }
    }
}
