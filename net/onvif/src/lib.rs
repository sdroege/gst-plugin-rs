// Copyright (C) 2022 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(clippy::non_send_fields_in_send_ty)]

use gst::glib;

mod onvifaggregator;
mod onvifdepay;
mod onvifoverlay;
mod onvifpay;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    onvifpay::register(plugin)?;
    onvifdepay::register(plugin)?;
    onvifaggregator::register(plugin)?;
    onvifoverlay::register(plugin)?;

    gst::meta::CustomMeta::register("OnvifXMLFrameMeta", &[], |_, _, _, _| true);

    Ok(())
}

gst::plugin_define!(
    rsonvif,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
