// Copyright (C) 2026 Collabora Ltd
//   @author: Daniel Morin <daniel.morin@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-zlibdecompress
 *
 * Decompress data compressed by `zlibcompress`.
 *
 * When the compressed stream carries `original-caps` (set by `zlibcompress`
 * with GDP), the srcpad caps are restored automatically.  When `original-caps`
 * is absent, caps must be supplied downstream via a GDP depayloader or an
 * explicit caps filter.
 *
 * Examples
 *
 * Direct pipeline
 * ```text
 * ... ! zlibcompress ! zlibdecompress ! ...
 * ```
 *
 * From file
 * ```text
 * filesrc location=file.zlib ! zlibdecompress ! rawvideoparse ...
 * ```
 *
 * See Also
 * `zlibcompress`, `deflatedecompress`
 *
 *
 *
 * SECTION:element-deflatedecompress
 *
 * Decompress data compressed by `deflatecompress`.
 *
 * When the compressed stream carries `original-caps` (set by `deflatecompress`
 * with GDP), the srcpad caps are restored automatically.  When `original-caps`
 * is absent, caps must be supplied downstream via a GDP depayloader or an
 * explicit caps filter.
 *
 * Examples
 *
 * Direct pipeline
 * ```text
 * ... ! deflatecompress ! deflatedecompress ! ...
 * ```
 *
 * From file
 * ```text
 * filesrc location=file.deflate ! deflatedecompress ! rawvideoparse ...
 * ```
 *
 * See Also
 * `deflatecompress`, `zlibdecompress`
 */
use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct FlateDecompress(ObjectSubclass<imp::FlateDecompress>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

glib::wrapper! {
    pub struct ZlibDecompress(ObjectSubclass<imp::ZlibDecompress>)
        @extends FlateDecompress, gst_base::BaseTransform, gst::Element, gst::Object;
}

glib::wrapper! {
    pub struct DeflateDecompress(ObjectSubclass<imp::DeflateDecompress>)
        @extends FlateDecompress, gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    FlateDecompress::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    gst::Element::register(
        Some(plugin),
        "zlibdecompress",
        gst::Rank::NONE,
        ZlibDecompress::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "deflatedecompress",
        gst::Rank::NONE,
        DeflateDecompress::static_type(),
    )?;
    Ok(())
}
