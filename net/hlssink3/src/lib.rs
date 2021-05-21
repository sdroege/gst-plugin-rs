//
// Copyright (C) 2021 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use glib::prelude::*;

mod imp;
mod playlist;

glib::wrapper! {
    pub struct HlsSink3(ObjectSubclass<imp::HlsSink3>) @extends gst::Bin, gst::Element, gst::Object;
}

unsafe impl Send for HlsSink3 {}
unsafe impl Sync for HlsSink3 {}

pub fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "hlssink3",
        gst::Rank::None,
        HlsSink3::static_type(),
    )?;

    Ok(())
}

gst::plugin_define!(
    hlssink3,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
