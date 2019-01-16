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

#![crate_type = "cdylib"]

extern crate libc;

extern crate gio_sys as gio_ffi;
extern crate glib_sys as glib_ffi;
extern crate gobject_sys as gobject_ffi;
extern crate gstreamer_sys as gst_ffi;

extern crate gio;
#[macro_use]
extern crate glib;
#[macro_use]
extern crate gstreamer as gst;

extern crate futures;
extern crate tokio;
extern crate tokio_current_thread;
extern crate tokio_executor;
extern crate tokio_reactor;
extern crate tokio_timer;

extern crate either;

extern crate rand;

#[macro_use]
extern crate lazy_static;

extern crate net2;

#[cfg(windows)]
extern crate winapi;

mod iocontext;

mod socket;
mod tcpclientsrc;
mod udpsrc;

mod appsrc;
mod dataqueue;
mod proxy;
mod queue;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    udpsrc::register(plugin)?;
    tcpclientsrc::register(plugin)?;
    queue::register(plugin)?;
    proxy::register(plugin)?;
    appsrc::register(plugin)?;

    Ok(())
}

gst_plugin_define!(
    "threadshare",
    "Threadshare Plugin",
    plugin_init,
    "0.1.0",
    "LGPL",
    "threadshare",
    "threadshare",
    "https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs",
    "2018-03-01"
);

pub fn set_element_flags<T: glib::IsA<gst::Object> + glib::IsA<gst::Element>>(
    element: &T,
    flags: gst::ElementFlags,
) {
    unsafe {
        use glib::translate::ToGlib;
        use gst_ffi;

        let ptr: *mut gst_ffi::GstObject = element.as_ptr() as *mut _;
        let _guard = MutexGuard::lock(&(*ptr).lock);
        (*ptr).flags |= flags.to_glib();
    }
}

struct MutexGuard<'a>(&'a glib_ffi::GMutex);

impl<'a> MutexGuard<'a> {
    pub fn lock(mutex: &'a glib_ffi::GMutex) -> Self {
        use glib::translate::mut_override;
        unsafe {
            glib_ffi::g_mutex_lock(mut_override(mutex));
        }
        MutexGuard(mutex)
    }
}

impl<'a> Drop for MutexGuard<'a> {
    fn drop(&mut self) {
        use glib::translate::mut_override;
        unsafe {
            glib_ffi::g_mutex_unlock(mut_override(self.0));
        }
    }
}
