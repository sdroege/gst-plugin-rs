// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
#[cfg(feature = "doc")]
use gst::prelude::*;

/**
 * plugin-sdp:
 *
 * Since: plugins-rs-0.16.0
 */
pub(crate) mod common;
pub(crate) mod listener;
pub(crate) mod message;

mod device_provider;
mod sapsink;
mod sapsrc;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "v1_30")]
    plugin.set_static_features_flag();

    #[cfg(feature = "doc")]
    {
        common::RtcpMode::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }

    sapsrc::register(plugin)?;
    sapsink::register(plugin)?;
    device_provider::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    sdp,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
