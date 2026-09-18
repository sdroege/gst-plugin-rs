// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct DeviceProvider(ObjectSubclass<imp::DeviceProvider>)
        @extends gst::DeviceProvider, gst::Object;
}

glib::wrapper! {
    pub struct Device(ObjectSubclass<imp::Device>) @extends gst::Device, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::DeviceProvider::register(
        Some(plugin),
        "sapdeviceprovider",
        gst::Rank::PRIMARY,
        DeviceProvider::static_type(),
    )
}
