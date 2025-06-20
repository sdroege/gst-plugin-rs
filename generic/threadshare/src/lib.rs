// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// Take a look at the license at the top of the repository in the LICENSE file.
#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

//! A collection of GStreamer plugins which leverage the `threadshare` [`runtime`].
//!
//! [`runtime`]: runtime/index.html
/**
 * plugin-threadshare:
 *
 * Since: plugins-rs-0.4.0
 */
#[macro_use]
pub mod runtime;

mod appsrc;
mod audiotestsrc;
pub mod dataqueue;
mod inputselector;
mod inter;
mod jitterbuffer;
mod proxy;
mod queue;
pub mod socket;
mod tcpclientsrc;
mod udpsink;
mod udpsrc;

pub mod net;

use gst::glib;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    appsrc::register(plugin)?;
    audiotestsrc::register(plugin)?;
    inputselector::register(plugin)?;
    inter::register(plugin)?;
    jitterbuffer::register(plugin)?;
    proxy::register(plugin)?;
    queue::register(plugin)?;
    tcpclientsrc::register(plugin)?;
    udpsink::register(plugin)?;
    udpsrc::register(plugin)?;

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
