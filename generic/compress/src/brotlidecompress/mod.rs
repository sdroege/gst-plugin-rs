// Copyright (C) 2026 Collabora Ltd
//   @author: Daniel Morin <daniel.morin@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-brotlidecompress
 *
 * Decompress data compressed by `brotlicompress` using the Brotli algorithm.
 *
 * The decompressor uses `GstAdapter` to reassemble complete Brotli streams from
 * arbitrary-sized input chunks (e.g. from `filesrc`). Brotli streams are
 * self-delimiting, so no `method` property is required.
 *
 * When the compressed stream carries `original-caps` (written by `brotlicompress`
 * with GDP), the srcpad caps are restored automatically. When absent, caps must be
 * provided downstream via a GDP depayloader or an explicit caps filter.
 *
 * Examples
 *
 * Direct pipeline (caps restored from original-caps):
 * ```text
 * ... ! brotlicompress ! brotlidecompress ! ...
 * ```
 *
 * From file (no embedded caps; provide them downstream):
 * ```text
 * filesrc location=file.br ! brotlidecompress ! rawvideoparse ...
 * ```
 *
 * Note Corruption handling
 *
 * Brotli has no end-to-end checksum (unlike zlib), so the outcome of data
 * corruption depends on where in the stream the corrupted bytes land:
 *
 * - Structural corruption: detected as a hard
 *   failure (`ResultFailure`), surfaced as a GStreamer flow error. The pipeline
 *   reports an error and no output is produced.
 *
 * - Payload corruption that breaks stream framing: treated as an incomplete
 *   stream (`NeedsMoreInput`). The decompressor waits for more data that never
 *   arrives, no output is produced and no error is signalled.
 *
 * - Payload corruption that still forms a valid stream: decoded silently
 *   with garbage output (`ResultSuccess` with wrong data). No error is
 *   signalled.
 *
 * For integrity guarantees, wrap the stream with GDP or use an external
 *  checksum.
 *
 * See Also
 * `brotlicompress`
 */
use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct BrotliDecompress(ObjectSubclass<imp::BrotliDecompress>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "brotlidecompress",
        gst::Rank::NONE,
        BrotliDecompress::static_type(),
    )
}
