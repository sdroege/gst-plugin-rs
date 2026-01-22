// Copyright (C) 2026 Thibault Saunier <tsaunier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! GStreamer tracer that outputs events in Perfetto native format.
//!
//! The generated trace files can be opened in [perfetto](https://ui.perfetto.dev/).

use gst::glib;
use gst::prelude::*;
use gst_tracing::tracer::TracingTracer;

mod imp;

glib::wrapper! {
    pub struct PerfettoTracer(ObjectSubclass<imp::PerfettoTracer>)
       @extends TracingTracer, gst::Tracer, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "perfetto", PerfettoTracer::static_type())
}
