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

mod obu;

glib::wrapper! {
    pub(crate) struct FMP4MuxPad(ObjectSubclass<imp::FMP4MuxPad>) @extends gst_base::AggregatorPad, gst::Pad, gst::Object;
}

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
    #[cfg(feature = "doc")]
    {
        FMP4Mux::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
        FMP4MuxPad::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
        HeaderUpdateMode::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }
    gst::Element::register(
        Some(plugin),
        "isofmp4mux",
        gst::Rank::PRIMARY,
        ISOFMP4Mux::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "cmafmux",
        gst::Rank::PRIMARY,
        CMAFMux::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "dashmp4mux",
        gst::Rank::PRIMARY,
        DASHMP4Mux::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "onviffmp4mux",
        gst::Rank::PRIMARY,
        ONVIFFMP4Mux::static_type(),
    )?;

    Ok(())
}

#[derive(Debug, Copy, Clone)]
pub(crate) enum ImageOrientation {
    Rotate0,
    Rotate90,
    Rotate180,
    Rotate270,
    // TODO:
    // FlipRotate0,
    // FlipRotate90,
    // FlipRotate180,
    // FlipRotate270,
}

type TransformMatrix = [[u8; 4]; 9];

const IDENTITY_MATRIX: TransformMatrix = [
    (1u32 << 16).to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    (1u32 << 16).to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    (1u32 << 30).to_be_bytes(),
];

const ROTATE_90_MATRIX: TransformMatrix = [
    0u32.to_be_bytes(),
    (1u32 << 16).to_be_bytes(),
    0u32.to_be_bytes(),
    (-1i32 << 16).to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    (1u32 << 30).to_be_bytes(),
];

const ROTATE_180_MATRIX: TransformMatrix = [
    (-1i32 << 16).to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    (-1i32 << 16).to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    (1u32 << 30).to_be_bytes(),
];

const ROTATE_270_MATRIX: TransformMatrix = [
    0u32.to_be_bytes(),
    (-1i32 << 16).to_be_bytes(),
    0u32.to_be_bytes(),
    (1u32 << 16).to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    0u32.to_be_bytes(),
    (1u32 << 30).to_be_bytes(),
];

impl ImageOrientation {
    pub(crate) fn transform_matrix(&self) -> &'static TransformMatrix {
        match self {
            ImageOrientation::Rotate0 => &IDENTITY_MATRIX,
            ImageOrientation::Rotate90 => &ROTATE_90_MATRIX,
            ImageOrientation::Rotate180 => &ROTATE_180_MATRIX,
            ImageOrientation::Rotate270 => &ROTATE_270_MATRIX,
        }
    }
}

#[derive(Debug)]
pub(crate) struct HeaderConfiguration {
    variant: Variant,
    update: bool,

    /// Pre-defined movie timescale if not 0.
    movie_timescale: u32,

    /// First caps must be the video/reference stream. Must be in the order the tracks are going to
    /// be used later for the fragments too.
    streams: Vec<HeaderStream>,

    write_mehd: bool,
    duration: Option<gst::ClockTime>,
    language_code: Option<[u8; 3]>,
    orientation: Option<ImageOrientation>,

    /// Start UTC time in ONVIF mode.
    /// Since Jan 1 1601 in 100ns units.
    start_utc_time: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct HeaderStream {
    /// Caps of this stream
    caps: gst::Caps,

    /// Set if this is an intra-only stream
    delta_frames: DeltaFrames,

    /// Pre-defined trak timescale if not 0.
    trak_timescale: u32,

    // More data to be included in the fragmented stream header
    extra_header_data: Option<Vec<u8>>,
}

#[derive(Debug)]
pub(crate) struct FragmentHeaderConfiguration<'a> {
    variant: Variant,

    /// Sequence number for this fragment.
    sequence_number: u32,

    /// If this is a full fragment or only a chunk.
    chunk: bool,

    streams: &'a [FragmentHeaderStream],
    buffers: &'a [Buffer],
}

#[derive(Debug)]
pub(crate) struct FragmentHeaderStream {
    /// Caps of this stream
    caps: gst::Caps,

    /// Set if this is an intra-only stream
    delta_frames: DeltaFrames,

    /// Pre-defined trak timescale if not 0.
    trak_timescale: u32,

    /// Start time of this fragment
    ///
    /// `None` if this stream has no buffers in this fragment.
    start_time: Option<gst::ClockTime>,
}

#[derive(Debug, Copy, Clone)]
pub(crate) enum DeltaFrames {
    /// Only single completely decodable frames
    IntraOnly,
    /// Frames may depend on past frames
    PredictiveOnly,
    /// Frames may depend on past or future frames
    Bidirectional,
}

impl DeltaFrames {
    /// Whether dts is required to order buffers differently from presentation order
    pub(crate) fn requires_dts(&self) -> bool {
        matches!(self, Self::Bidirectional)
    }
    /// Whether this coding structure does not allow delta flags on buffers
    pub(crate) fn intra_only(&self) -> bool {
        matches!(self, Self::IntraOnly)
    }
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
