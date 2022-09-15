//
// Copyright (C) 2022 Vivienne Watermeier <vwatermeier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(unused_doc_comments)]

/**
 * plugin-rtpav1:
 *
 * Since: plugins-rs-0.9.0
 */
use gst::glib;

mod common;
pub mod depay;
pub mod pay;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    depay::register(plugin)?;
    pay::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    rtpav1,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
