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

extern crate glib;
#[macro_use]
extern crate gst_plugin;
#[macro_use]
extern crate gstreamer as gst;

extern crate futures;
extern crate tokio;
extern crate tokio_executor;
extern crate tokio_reactor;
extern crate tokio_threadpool;
extern crate tokio_timer;

extern crate either;

extern crate rand;

#[macro_use]
extern crate lazy_static;

mod iocontext;

mod udpsocket;
mod udpsrc;

mod appsrc;
mod dataqueue;
mod proxy;
mod queue;

fn plugin_init(plugin: &gst::Plugin) -> bool {
    udpsrc::register(plugin);
    queue::register(plugin);
    proxy::register(plugin);
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
