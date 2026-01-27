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

/// Backend for Perfetto tracing output.
///
/// Determines where trace data is written.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "GstPerfettoTracerBackend")]
#[doc(alias = "GstPerfettoTracerBackend")]
#[repr(i32)]
pub enum PerfettoBackend {
    /// Write to a `.pftrace` file in the current directory.
    #[default]
    #[enum_value(name = "File: Write to a .pftrace file", nick = "file")]
    File = 0,
    /// Connect to the system `traced` daemon for system-wide tracing.
    #[enum_value(name = "System: Connect to traced daemon", nick = "system")]
    System = 1,
}

glib::wrapper! {
    pub struct PerfettoTracer(ObjectSubclass<imp::PerfettoTracer>)
       @extends TracingTracer, gst::Tracer, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Tracer::register(Some(plugin), "perfetto", PerfettoTracer::static_type())
}
