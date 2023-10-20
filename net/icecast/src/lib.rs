// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * plugin-icecast:
 *
 * Since: plugins-rs-0.15.0
 */
use gst::glib;

mod icecastsink;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    icecastsink::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    icecast,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
