// Copyright (C) 2021 Simonas Kazlauskas <tracing-gstreamer@kazlauskas.me>
// Copyright (C) 2026 Thibault Saunier <tsaunier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! GStreamer tracer that integrates with the Rust tracing ecosystem.
//!
//! This tracer provides spans for GStreamer pad operations, allowing
//! integration with any tracing subscriber.

use gst::glib;
use gst::prelude::*;

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(
        Some(plugin),
        "rusttracing",
        gst_tracing::tracer::TracingTracer::static_type(),
    )
}
