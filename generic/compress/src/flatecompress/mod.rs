// Copyright (C) 2026 Collabora Ltd
//   @author: Daniel Morin <daniel.morin@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-zlibcompress
 *
 * Compress data using the zlib algorithm (includes checksum).
 *
 * The srcpad caps will be `application/x-zlib-compressed` with an
 * `original-caps` field carrying the sinkpad caps.
 *
 * Unless the compressed stream will be multiplexed into a container, using the
 * GStreamer Data Protocol (GDP) is recommended so that caps are preserved
 * in-band and `zlibdecompress` can restore them without out-of-band signalling.
 *
 * Examples
 *
 * Using GDP (caps preserved in-band)
 * ```text
 * ... ! zlibcompress ! gdppay ! filesink location=/path/to/file
 * ```
 *
 * Without GDP
 * ```text
 * ... ! zlibcompress ! filesink location=/path/to/file
 * ```
 *
 * See Also
 * `zlibdecompress`, `deflatecompress`
 *
 *
 * SECTION:element-deflatecompress
 *
 * Compress data using the deflate algorithm (no checksum).
 *
 * The srcpad caps will be `application/x-deflate-compressed` with an
 * `original-caps` field carrying the sinkpad caps.
 *
 * Unless the compressed stream will be multiplexed into a container, using the
 * GStreamer Data Protocol (GDP) is recommended so that caps are preserved
 * in-band and `deflatedecompress` can restore them without out-of-band signalling.
 *
 * Examples
 *
 * Using GDP (caps preserved in-band)
 * ```text
 * ... ! deflatecompress ! gdppay ! filesink location=/path/to/file
 * ```
 *
 * Without GDP
 * ```text
 * ... ! deflatecompress ! filesink location=/path/to/file
 * ```
 *
 * See Also
 * `deflatedecompress`, `zlibcompress`
 */
use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct FlateCompress(ObjectSubclass<imp::FlateCompress>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

glib::wrapper! {
    pub struct ZlibCompress(ObjectSubclass<imp::ZlibCompress>)
        @extends FlateCompress, gst_base::BaseTransform, gst::Element, gst::Object;
}

glib::wrapper! {
    pub struct DeflateCompress(ObjectSubclass<imp::DeflateCompress>)
        @extends FlateCompress, gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    FlateCompress::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    gst::Element::register(
        Some(plugin),
        "zlibcompress",
        gst::Rank::NONE,
        ZlibCompress::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "deflatecompress",
        gst::Rank::NONE,
        DeflateCompress::static_type(),
    )?;
    Ok(())
}
