// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

pub mod handdetectiontensordec;
pub mod handlandmarktensordec;

mod helper;

use gst::glib;

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    handdetectiontensordec::register(plugin)?;
    handlandmarktensordec::register(plugin)?;
    Ok(())
}
