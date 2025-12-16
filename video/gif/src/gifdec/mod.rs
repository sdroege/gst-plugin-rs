// Copyright (C) 2025  Taruntej Kanakamalla <tarun@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

/**
 * SECTION:element-gifdec
 *
 * Decodes gif to raw video
 *
 * ## Example launch line
 *
 * ```bash
 * gst-launch-1.0 filesrc location=$GIF_FILE_PATH ! gifdec ! videoconvert ! autovideosink
 * ```
 *
 * Since: 0.15
 */
mod imp;

glib::wrapper! {
    pub struct GifDec(ObjectSubclass<imp::GifDec>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "gifdec",
        gst::Rank::PRIMARY,
        GifDec::static_type(),
    )
}
