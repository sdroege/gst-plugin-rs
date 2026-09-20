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

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "GstGifLoopStrategy")]
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
        "gifdec",
        gst::Rank::PRIMARY,
        GifDec::static_type(),
    )
}
