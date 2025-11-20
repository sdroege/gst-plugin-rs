// Copyright (C) 2022, Asymptotic Inc.
//      Author: Taruntej Kanakamalla <taruntej@asymptotic.io>
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

/**
 * plugin-webrtchttp:
 *
 * Since: plugins-rs-0.9.0
 *
 * Deprecated: plugins-rs-0.16.0 Use the `whipclientsink` and `whepclientsrc` elements from the
 * `rswebrtc` plugin instead
 */
use gst::glib;
mod utils;
mod whepsrc;
mod whipsink;

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstRsWebRTCICETransportPolicy")]
#[non_exhaustive]
pub enum IceTransportPolicy {
    #[enum_value(name = "All: get both STUN and TURN candidate pairs", nick = "all")]
    All = 0,
    #[enum_value(name = "Relay: get only TURN candidate pairs", nick = "relay")]
    Relay = 1,
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        use gst::prelude::*;

        IceTransportPolicy::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }

    #[cfg(feature = "v1_24")]
    plugin.add_status_warning(
        "This plugin is now deprecated. \
        Please use `whipclientsink` and `whepclientsrc` \
        from the rswebrtc plugin instead of `whipsink` and `whepsrc`.",
    );

    whipsink::register(plugin)?;
    whepsrc::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    webrtchttp,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
