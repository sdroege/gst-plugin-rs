// Copyright (C) 2025 Thibault Saunier <tsaunier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-debugseimetainserter
 *
 * Adds GstVideoSEIUserDataUnregisteredMeta to video buffers for testing SEI insertion.
 *
 * ## Example launch line
 * ```
 * gst-launch-1.0 videotestsrc ! x264enc ! h264parse ! \
 *   debugseimetainserter uuid=12345678-1234-1234-1234-123456789abc data="test payload" ! \
 *   h264seiinserter ! filesink location=output.h264
 * ```
 *
 * Since: plugins-rs-0.15.0
 */
use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct DebugSeiMetaInserter(ObjectSubclass<imp::DebugSeiMetaInserter>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "debugseimetainserter",
        gst::Rank::NONE,
        DebugSeiMetaInserter::static_type(),
    )
}
