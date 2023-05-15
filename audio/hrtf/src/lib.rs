// Copyright (C) 2021-2024 Tomasz Andrzejak <andreiltd@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

/**
 * plugin-hrtf:
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;

#[cfg(feature = "doc")]
use gst::prelude::*;

mod hrtf;
mod sofa;
mod spatial;
mod thread;

pub use spatial::{SpatialObject, Vec3};

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstHrtfCoordinateSystem")]
pub enum CoordinateSystem {
    #[enum_value(
        name = "Cartesian: The positive x, y and z axes point forward, left and up, respectively",
        nick = "cartesian"
    )]
    Cartesian = 0,
    #[enum_value(
        name = "Left Handed: The positive x, y and z axes point right, up and forward, respectively",
        nick = "left-handed"
    )]
    LeftHanded = 1,
    #[enum_value(
        name = "Right Handed: The positive x and y axes point right and up, and the negative z axis points forward.",
        nick = "right-handed"
    )]
    RightHanded = 2,
}

pub fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    CoordinateSystem::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());

    hrtf::register(plugin)?;
    sofa::register(plugin)
}

gst::plugin_define!(
    hrtf,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
