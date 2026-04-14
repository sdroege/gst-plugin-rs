// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

/**
 * SECTION:element-imagersoverlay
 *
 * The imagersoverlay element overlays an image loaded from file onto a video stream.
 *
 * Changing the positioning, overlay width and height properties at runtime
 * is supported.
 *
 * Changing the image by setting a new file location at runtime is supported.
 *
 * Changing the image using a premade raw buffer or instance of
 * image::DynamicImage at runtime is not (yet) supported.
 *
 * ## Example launch line
 *
 * ```bash
 * gst-launch-1.0 -v videotestsrc ! imagersoverlay location=image.png ! autovideosink
 * ```
 *
 * Since: 0.16
 */
mod imp;

glib::wrapper! {
    pub struct Overlay(ObjectSubclass<imp::ImageRsOverlay>) @extends gst_video::VideoFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    imp::PositioningMode::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());

    gst::Element::register(
        Some(plugin),
        "imagersoverlay",
        gst::Rank::NONE,
        Overlay::static_type(),
    )
}
