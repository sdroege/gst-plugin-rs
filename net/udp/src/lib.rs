// Copyright (C) 2024 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#[cfg(feature = "doc")]
use gst::prelude::*;

/**
 * plugin-rsudp:
 *
 * Since: plugins-rs-0.16.0
 */
mod baseudpsink;
mod multiudpsink;
mod net;
mod udpsink;
mod udpsrc;
mod uri;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "v1_30")]
    plugin.set_static_features_flag();

    #[cfg(feature = "doc")]
    baseudpsink::BaseUdpSink::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    udpsink::register(plugin)?;
    multiudpsink::register(plugin)?;
    udpsrc::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    rsudp,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
