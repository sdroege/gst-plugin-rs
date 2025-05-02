// Copyright (C) 2025 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-awstranslate
 *
 * `awstranslate` is an element that can be used to translate text from one
 * language to another.
 *
 * When working with live data, the element will accumulate input text until
 * the deadline is reached, comparing the current running time with the running
 * time of the input items and the upstream latency.
 *
 * At this point the input items will be drained up until the first item ending
 * with a punctuation symbol.
 *
 * The accumulator will also be drained upon receiving `rstranscribe/final-transcript`
 * and `rstranscribe/speaker-change` custom events.
 *
 * When the user wants to use a very low / no latency upstream, and is willing to
 * accept desynchronization in order to build up long-enough sentences for translation,
 * they can set the #GstAwsTranslate:accumulator-lateness property to shift the input
 * timestamps forward when accumulating.
 */
use gst::glib;
use gst::prelude::*;

mod imp;

use std::sync::LazyLock;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "awstranslate",
        gst::DebugColorFlags::empty(),
        Some("AWS translate element"),
    )
});

glib::wrapper! {
    pub struct Translate(ObjectSubclass<imp::Translate>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "awstranslate",
        gst::Rank::NONE,
        Translate::static_type(),
    )
}
