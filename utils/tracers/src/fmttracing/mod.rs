// Copyright (C) 2021 Simonas Kazlauskas <tracing-gstreamer@kazlauskas.me>
// Copyright (C) 2026 Thibault Saunier <tsaunier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! GStreamer tracer that uses the tracing-subscriber fmt formatter.
//!
//! This tracer provides human-readable output using `tracing_subscriber::fmt`.

use gst::glib;
use gst::prelude::*;
use gst_tracing::tracer::TracingTracer;

mod imp;

glib::wrapper! {
    pub struct FmtTracer(ObjectSubclass<imp::FmtTracer>)
       @extends TracingTracer, gst::Tracer, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "fmttracing", FmtTracer::static_type())
}
