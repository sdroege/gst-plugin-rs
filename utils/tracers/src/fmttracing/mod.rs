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

/// Output format for the fmt tracer.
///
/// Determines how trace events are formatted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "GstFmtTracerFormat")]
#[doc(alias = "GstFmtTracerFormat")]
#[repr(i32)]
pub enum FmtFormat {
    /// Full format with all details (default).
    #[default]
    #[enum_value(name = "Full: Complete event information", nick = "full")]
    Full = 0,
    /// Compact format on a single line.
    #[enum_value(name = "Compact: Single-line output", nick = "compact")]
    Compact = 1,
    /// Pretty format with visual indentation.
    #[enum_value(name = "Pretty: Visual indentation", nick = "pretty")]
    Pretty = 2,
    /// JSON format for machine parsing.
    #[enum_value(name = "JSON: Machine-readable output", nick = "json")]
    Json = 3,
}

glib::wrapper! {
    pub struct FmtTracer(ObjectSubclass<imp::FmtTracer>)
       @extends TracingTracer, gst::Tracer, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    FmtFormat::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());

    gst::Tracer::register(Some(plugin), "fmttracing", FmtTracer::static_type())
}
