// Copyright (C) 2021 Simonas Kazlauskas <tracing-gstreamer@kazlauskas.me>
// Copyright (C) 2026 Thibault Saunier <tsaunier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * tracer-fmttracing:
 *
 * This tracer uses the `tracing_subscriber::fmt` subscriber to format
 * tracing events. This provides human-readable output on stderr.
 *
 * Example:
 *
 * ```console
 * $ RUST_LOG=debug GST_TRACERS='fmttracing(log-level=4)' gst-launch-1.0 videotestsrc num-buffers=10 ! fakesink
 * ```
 *
 * ## Parameters
 *
 * ### `log-level`
 *
 * GStreamer log level to integrate (same format as GST_DEBUG).
 */
use gst::{glib, subclass::prelude::*};
use gstreamer_tracing::tracer::{TracingTracer, TracingTracerImpl};
use tracing::error;

#[derive(Default)]
pub struct FmtTracer {}

#[glib::object_subclass]
impl ObjectSubclass for FmtTracer {
    const NAME: &'static str = "GstFmtTracer";
    type Type = super::FmtTracer;
    type ParentType = TracingTracer;
    type Interfaces = ();
}

impl ObjectImpl for FmtTracer {
    fn constructed(&self) {
        if let Err(e) = tracing_subscriber::fmt::try_init() {
            error!("Failed to initialize tracing subscriber: {e:?}");
        }

        self.parent_constructed();
    }
}

impl GstObjectImpl for FmtTracer {}
impl TracerImpl for FmtTracer {}
impl TracingTracerImpl for FmtTracer {}
