// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

/**
 * SECTION:element-imagersdec
 *
 * Decodes still image formats using pure Rust to raw video
 *
 * ## Example launch line
 *
 * ```bash
 * gst-launch-1.0 filesrc location=$PATH ! typefind ! imagersdec ! imagefreeze ! video/x-raw,framerate=30/1 ! videoconvert ! autovideosink
 * ```
 *
 * Since: 0.16
 */
mod imp;

glib::wrapper! {
    pub struct Decoder(ObjectSubclass<imp::Decoder>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "imagersdec",
        // We want a decoder that's higher than gdkpixbufdec (SECONDARY)
        // but lower than format-specific decoders (PRIMARY).
        // The upcoming animated decoder will be +2 to take precedence over
        // this one.
        gst::Rank::SECONDARY + 1,
        Decoder::static_type(),
    )
}
