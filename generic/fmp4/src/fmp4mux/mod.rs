// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod boxes;
mod imp;

glib::wrapper! {
    pub(crate) struct FMP4Mux(ObjectSubclass<imp::FMP4Mux>) @extends gst_base::Aggregator, gst::Element, gst::Object;
}

glib::wrapper! {
    pub(crate) struct ISOFMP4Mux(ObjectSubclass<imp::ISOFMP4Mux>) @extends FMP4Mux, gst_base::Aggregator, gst::Element, gst::Object;
}

glib::wrapper! {
    pub(crate) struct CMAFMux(ObjectSubclass<imp::CMAFMux>) @extends FMP4Mux, gst_base::Aggregator, gst::Element, gst::Object;
}

glib::wrapper! {
    pub(crate) struct DASHMP4Mux(ObjectSubclass<imp::DASHMP4Mux>) @extends FMP4Mux, gst_base::Aggregator, gst::Element, gst::Object;
}

glib::wrapper! {
    pub(crate) struct ONVIFFMP4Mux(ObjectSubclass<imp::ONVIFFMP4Mux>) @extends FMP4Mux, gst_base::Aggregator, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "isofmp4mux",
        gst::Rank::Primary,
        ISOFMP4Mux::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "cmafmux",
        gst::Rank::Primary,
        CMAFMux::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "dashmp4mux",
        gst::Rank::Primary,
        DASHMP4Mux::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "onviffmp4mux",
        gst::Rank::Primary,
        ONVIFFMP4Mux::static_type(),
    )?;

    Ok(())
}

#[derive(Debug)]
pub(crate) struct HeaderConfiguration<'a> {
    variant: Variant,
    #[allow(dead_code)]
    element: &'a FMP4Mux,
    update: bool,
    /// First caps must be the video/reference stream. Must be in the order the tracks are going to
    /// be used later for the fragments too.
    streams: &'a [(gst_base::AggregatorPad, gst::Caps)],
    write_mehd: bool,
    duration: Option<gst::ClockTime>,
}

#[derive(Debug)]
pub(crate) struct FragmentHeaderConfiguration<'a> {
    variant: Variant,
    sequence_number: u32,
    streams: &'a [(
        gst_base::AggregatorPad,
        gst::Caps,
        Option<FragmentTimingInfo>,
    )],
    buffers: &'a [Buffer],
}

#[derive(Debug)]
pub(crate) struct FragmentTimingInfo {
    /// Start time of this fragment
    start_time: gst::ClockTime,
    /// Set if this is an intra-only stream
    intra_only: bool,
}

#[derive(Debug)]
pub(crate) struct Buffer {
    /// Track index
    idx: usize,

    /// Actual buffer
    buffer: gst::Buffer,

    /// Timestamp
    timestamp: gst::ClockTime,

    /// Sample duration
    duration: gst::ClockTime,

    /// Composition time offset
    composition_time_offset: Option<i64>,
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    ISO,
    CMAF,
    DASH,
    ONVIF,
}

impl Variant {
    pub(crate) fn is_single_stream(self) -> bool {
        match self {
            Variant::ISO | Variant::ONVIF => false,
            Variant::CMAF | Variant::DASH => true,
        }
    }
}

#[derive(Debug)]
pub(crate) struct FragmentOffset {
    time: gst::ClockTime,
    offset: u64,
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[repr(i32)]
#[enum_type(name = "GstFMP4MuxHeaderUpdateMode")]
pub(crate) enum HeaderUpdateMode {
    None,
    Rewrite,
    Update,
}
