// Copyright (C) 2026 Collabora Ltd
//   @author: Daniel Morin <daniel.morin@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-brotlicompress
 *
 * Compress data using the Brotli algorithm.
 *
 * The `level` property controls the compression level (0=fastest, 11=slower
 * most compressed, default=6).
 * Higher level produces smaller output but is slower to compress.
 * Decompression speed is unaffected by the level used during compression.
 *
 * The srcpad caps are `application/x-brotli-compressed` with an `original-caps`
 * field carrying the sinkpad caps, enabling `brotlidecompress` to restore them
 * automatically.
 *
 * Examples
 *
 * Using GDP (caps preserved in-band):
 * ```text
 * ... ! brotlicompress ! gdppay ! filesink location=/path/to/file
 * ```
 *
 * Without GDP:
 * ```text
 * ... ! brotlicompress level=9 ! filesink location=/path/to/file
 * ```
 *
 * See Also
 * `brotlidecompress`
 */
use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct BrotliCompress(ObjectSubclass<imp::BrotliCompress>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "brotlicompress",
        gst::Rank::NONE,
        BrotliCompress::static_type(),
    )
}
