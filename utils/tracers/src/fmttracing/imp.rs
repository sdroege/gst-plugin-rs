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
 *
 * ### `format`
 *
 * Output format: `full` (default), `compact`, `pretty`, or `json`.
 *
 * ### `with-time`
 *
 * Whether to include timestamps in the output (default: true).
 *
 * ### `with-target`
 *
 * Whether to include the target in the output (default: true).
 *
 * ### `with-ansi`
 *
 * Whether to include ANSI color codes in the output (default: true).
 */
use gst::glib::Properties;
use gst::{glib, prelude::*, subclass::prelude::*};
use gst_tracing::tracer::{TracingTracer, TracingTracerImpl};
use tracing::error;

use super::FmtFormat;

struct Settings {
    format: FmtFormat,
    with_time: bool,
    with_target: bool,
    with_ansi: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            format: FmtFormat::Full,
            with_time: true,
            with_target: true,
            with_ansi: true,
        }
    }
}

#[derive(Properties, Default)]
#[properties(wrapper_type = super::FmtTracer)]
pub struct FmtTracer {
    #[property(
        name = "format",
        get,
        set,
        type = FmtFormat,
        member = format,
        blurb = "Output format",
        builder(FmtFormat::Full)
    )]
    #[property(
        name = "with-time",
        get,
        set,
        type = bool,
        member = with_time,
        blurb = "Include timestamps",
        default = true
    )]
    #[property(
        name = "with-target",
        get,
        set,
        type = bool,
        member = with_target,
        blurb = "Include the target",
        default = true
    )]
    #[property(
        name = "with-ansi",
        get,
        set,
        type = bool,
        member = with_ansi,
        blurb = "Include ANSI color codes",
        default = true
    )]
    settings: std::sync::Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for FmtTracer {
    const NAME: &'static str = "GstFmtTracer";
    type Type = super::FmtTracer;
    type ParentType = TracingTracer;
}

#[glib::derived_properties]
impl ObjectImpl for FmtTracer {
    fn constructed(&self) {
        self.parent_constructed();

        let settings = self.settings.lock().unwrap();
        let format = settings.format;
        let with_time = settings.with_time;
        let with_target = settings.with_target;
        let with_ansi = settings.with_ansi;
        drop(settings);

        // Macro to reduce repetition - each format returns a different type
        // so we can't easily abstract over them without boxing
        macro_rules! init_subscriber {
            ($builder:expr) => {{
                let builder = $builder.with_target(with_target).with_ansi(with_ansi);
                if with_time {
                    builder.try_init()
                } else {
                    builder.without_time().try_init()
                }
            }};
        }

        let result = match format {
            FmtFormat::Full => init_subscriber!(tracing_subscriber::fmt()),
            FmtFormat::Compact => init_subscriber!(tracing_subscriber::fmt().compact()),
            FmtFormat::Pretty => init_subscriber!(tracing_subscriber::fmt().pretty()),
            FmtFormat::Json => init_subscriber!(tracing_subscriber::fmt().json()),
        };

        if let Err(e) = result {
            error!("Failed to initialize tracing subscriber: {e:?}");
        }
    }
}

impl GstObjectImpl for FmtTracer {}
impl TracerImpl for FmtTracer {}
impl TracingTracerImpl for FmtTracer {}
