// GStreamer multi audio mixer plugin
//
// Copyright (C) 2022-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#![allow(unused_doc_comments)]

use gst::glib;
use gst::prelude::*;

mod audiomultimixerelement;
mod minus1mixer;
mod splitmeta;
mod splitter;

/**
 * plugin-audiomultimixer:
 *
 * Since: plugins-rs-0.16.0
 */

glib::wrapper! {
    pub struct MultiMixerElement(ObjectSubclass<audiomultimixerelement::MultiMixerElement>) @extends gst_audio::AudioAggregator, gst_base::Aggregator, gst::Element, gst::Object;
}

glib::wrapper! {
    pub struct Minus1Mixer(ObjectSubclass<minus1mixer::Minus1Mixer>) @extends gst::Bin, gst::Element, gst::Object;
}

glib::wrapper! {
    pub struct MultiMixerSplitter(ObjectSubclass<splitter::Splitter>) @extends gst::Element, gst::Object;
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    // Keep these internal for the time being
    /*
    gst::Element::register(
        Some(plugin),
        "multimixerelement",
        gst::Rank::NONE,
        MultiMixerElement::static_type(),
    )?;

    gst::Element::register(
        Some(plugin),
        "multimixersplitter",
        gst::Rank::NONE,
        MultiMixerSplitter::static_type(),
    )?;
    */

    gst::Element::register(
        Some(plugin),
        "minus1mixer",
        gst::Rank::NONE,
        Minus1Mixer::static_type(),
    )?;

    Ok(())
}

gst::plugin_define!(
    audiomultimixer,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);

#[allow(clippy::module_inception)]
#[cfg(test)]
mod tests;
