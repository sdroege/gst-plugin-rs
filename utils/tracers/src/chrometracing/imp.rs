// Copyright (C) 2021 Simonas Kazlauskas <tracing-gstreamer@kazlauskas.me>
// Copyright (C) 2026 Thibault Saunier <tsaunier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * tracer-chrometracing:
 *
 * This tracer outputs events in the Chrome JSON tracing format, which can be
 * visualized in [perfetto](https://ui.perfetto.dev/).
 *
 * Example:
 *
 * ```console
 * $ GST_TRACERS='chrometracing(log-level=4)' gst-launch-1.0 videotestsrc num-buffers=120 ! fakesink
 * ```
 *
 * A `trace-XXX.json` file will be created in the current directory.
 *
 * ## Parameters
 *
 * ### `log-level`
 *
 * GStreamer log level to integrate (same format as GST_DEBUG).
 *
 * ### `include-args`
 *
 * Whether to include span arguments in the trace output (default: true).
 */
use gst::glib::Properties;
use gst::{glib, prelude::*, subclass::prelude::*};
use gst_tracing::tracer::{TracingTracer, TracingTracerImpl};
use std::sync::Mutex;
use tracing::error;
use tracing_subscriber::prelude::*;

#[derive(Default)]
struct State {
    chrome_guard: Option<tracing_chrome::FlushGuard>,
}

#[derive(Properties, Default)]
#[properties(wrapper_type = super::ChromeTracer)]
pub struct ChromeTracer {
    #[property(name = "include-args", get, set, construct, type = bool, member = include_args, blurb = "Include span arguments in the trace output", default = true)]
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

struct Settings {
    include_args: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { include_args: true }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ChromeTracer {
    const NAME: &'static str = "GstChromeTracer";
    type Type = super::ChromeTracer;
    type ParentType = TracingTracer;
}

#[glib::derived_properties]
impl ObjectImpl for ChromeTracer {
    fn constructed(&self) {
        self.parent_constructed();

        let include_args = self.settings.lock().unwrap().include_args;

        let (chrome_layer, guard) = tracing_chrome::ChromeLayerBuilder::new()
            .include_args(include_args)
            .build();

        self.state.lock().unwrap().chrome_guard = Some(guard);
        if let Err(e) = tracing_subscriber::registry().with(chrome_layer).try_init() {
            error!("Failed to initialize tracing subscriber: {e:?}");
        }
    }
}

impl GstObjectImpl for ChromeTracer {}
impl TracerImpl for ChromeTracer {
    // Parse the GST_TRACERS params into properties.
    const USE_STRUCTURE_PARAMS: bool = true;
}
impl TracingTracerImpl for ChromeTracer {}
