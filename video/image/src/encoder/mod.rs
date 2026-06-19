// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

/**
 * SECTION:element-imagersenc
 *
 * Encodes a single frame into still image formats
 *
 * ## Example launch line
 *
 * ```bash
 * gst-launch-1.0 videotestsrc pattern=smpte num-buffers=1 ! video/x-raw,width=320,height=240,format=RGBA ! videoconvert ! imagersenc ! image/png ! filesink location=foo.png
 * ```
 *
 * Since: 0.16
 */
mod imp;

glib::wrapper! {
    pub struct Encoder(ObjectSubclass<imp::Encoder>) @extends gst_video::VideoEncoder, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "imagersenc",
        // We want an encoder that's lower than format-specific decoders
        // (PRIMARY).
        // I went with SECONDARY + 1 to match its decoder counterpart.
        // The animated (en/de)coder should have SECONDARY + 2.
        gst::Rank::SECONDARY + 1,
        Encoder::static_type(),
    )
}
