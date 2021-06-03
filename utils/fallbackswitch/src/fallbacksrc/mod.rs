// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
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

use gst::glib;
use gst::prelude::*;

mod custom_source;
mod imp;
mod video_fallback;

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::GEnum)]
#[repr(u32)]
#[genum(type_name = "GstFallbackSourceRetryReason")]
enum RetryReason {
    None,
    Error,
    Eos,
    StateChangeFailure,
    Timeout,
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::GEnum)]
#[repr(u32)]
#[genum(type_name = "GstFallbackSourceStatus")]
enum Status {
    Stopped,
    Buffering,
    Retrying,
    Running,
}

glib::wrapper! {
    pub struct FallbackSrc(ObjectSubclass<imp::FallbackSrc>) @extends gst::Bin, gst::Element, gst::Object;
}

// GStreamer elements need to be thread-safe. For the private implementation this is automatically
// enforced but for the public wrapper type we need to specify this manually.
unsafe impl Send for FallbackSrc {}
unsafe impl Sync for FallbackSrc {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "fallbacksrc",
        gst::Rank::None,
        FallbackSrc::static_type(),
    )
}
