// Copyright (C) 2026 Netflix Inc.
//   Author: Xavier Claessens <xclaessens@netflix.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

/**
 * plugin-debugutils:
 *
 * Elements useful for debugging and testing purposes.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;

mod debugseimetainserter;
mod streamgrouper;
#[cfg(target_os = "linux")]
mod udmabufupload;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "v1_30")]
    plugin.set_static_features_flag();

    debugseimetainserter::register(plugin)?;
    streamgrouper::register(plugin)?;

    #[cfg(target_os = "linux")]
    udmabufupload::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    debugutils,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
