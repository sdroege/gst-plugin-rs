// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

/**
 * SECTION:element-animatedimagersdec
 *
 * Decodes animated image formats using pure Rust to raw video
 *
 * ## Example launch line
 *
 * ```bash
 * gst-launch-1.0 filesrc location=/path/to/image ! typefind ! animatedimagersdec ! videoconvert ! autovideosink
 * ```
 *
 * Since: 0.16
 */
mod imp;

glib::wrapper! {
    pub struct Decoder(ObjectSubclass<imp::Decoder>) @extends gst::Element, gst::Object;
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "GstImageRsLoopStrategy")]
#[repr(C)]
pub enum LoopStrategy {
    #[enum_value(
        name = "Loop the animation irrespective of the image's settings",
        nick = "force-yes"
    )]
    Yes,
    #[enum_value(
        name = "Play the animation only once irrespective of the image's settings",
        nick = "force-no"
    )]
    No,
    #[default]
    #[enum_value(name = "Respect the image's settings", nick = "image")]
    Image,
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        LoopStrategy::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }
    gst::Element::register(
        Some(plugin),
        "animatedimagersdec",
        gst::Rank::SECONDARY + 2,
        Decoder::static_type(),
    )
}
