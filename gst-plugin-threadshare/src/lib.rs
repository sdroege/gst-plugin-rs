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

extern crate glib_sys as glib_ffi;
extern crate gstreamer_sys as gst_ffi;

extern crate glib;
extern crate gobject_subclass;
#[macro_use]
extern crate gst_plugin;
#[macro_use]
extern crate gstreamer as gst;

extern crate futures;
extern crate tokio;
extern crate tokio_current_thread;
extern crate tokio_executor;
extern crate tokio_reactor;
extern crate tokio_threadpool;
extern crate tokio_timer;

extern crate either;

extern crate rand;

#[macro_use]
extern crate lazy_static;

extern crate net2;

mod iocontext;

mod socket;
mod tcpclientsrc;
mod udpsrc;

mod appsrc;
mod dataqueue;
mod proxy;
mod queue;

fn plugin_init(plugin: &gst::Plugin) -> bool {
    udpsrc::register(plugin);
    tcpclientsrc::register(plugin);
    queue::register(plugin);
    proxy::register(plugin);
    appsrc::register(plugin);
    true
}

plugin_define!(
    b"threadshare\0",
    b"Threadshare Plugin\0",
    plugin_init,
    b"0.1.0\0",
    b"LGPL\0",
    b"threadshare\0",
    b"threadshare\0",
    b"https://github.com/sdroege/gst-plugin-threadshare\0",
    b"2018-03-01\0"
);

pub fn set_element_flags<T: glib::IsA<gst::Object> + glib::IsA<gst::Element>>(
    element: &T,
    flags: gst::ElementFlags,
) {
    unsafe {
        use glib::translate::ToGlib;
        use gst_ffi;

        let ptr: *mut gst_ffi::GstObject = element.to_glib_none().0;
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
