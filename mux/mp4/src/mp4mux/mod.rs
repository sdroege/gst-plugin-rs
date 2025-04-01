// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
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
    pub(crate) struct MP4MuxPad(ObjectSubclass<imp::MP4MuxPad>) @extends gst_base::AggregatorPad, gst::Pad, gst::Object;
}

glib::wrapper! {
    pub(crate) struct MP4Mux(ObjectSubclass<imp::MP4Mux>) @extends gst_base::Aggregator, gst::Element, gst::Object;
}

glib::wrapper! {
    pub(crate) struct ISOMP4Mux(ObjectSubclass<imp::ISOMP4Mux>) @extends MP4Mux, gst_base::Aggregator, gst::Element, gst::Object;
}

glib::wrapper! {
    pub(crate) struct ONVIFMP4Mux(ObjectSubclass<imp::ONVIFMP4Mux>) @extends MP4Mux, gst_base::Aggregator, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        MP4Mux::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
        MP4MuxPad::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }
    gst::Element::register(
        Some(plugin),
        "isomp4mux",
        gst::Rank::MARGINAL,
        ISOMP4Mux::static_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "onvifmp4mux",
        gst::Rank::MARGINAL,
        ONVIFMP4Mux::static_type(),
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
    /// Whether dts is required to order samples differently from presentation order
    pub(crate) fn requires_dts(&self) -> bool {
        matches!(self, Self::Bidirectional)
    }
    /// Whether this coding structure does not allow delta flags on samples
    pub(crate) fn intra_only(&self) -> bool {
        matches!(self, Self::IntraOnly)
    }
}

#[derive(Debug)]
pub(crate) struct Sample {
    /// Sync point
    sync_point: bool,

    /// Sample duration
    duration: gst::ClockTime,

    /// Composition time offset
    ///
    /// This is `None` for streams that have no concept of DTS.
    composition_time_offset: Option<i64>,

    /// Size
    size: u32,
}

#[derive(Debug)]
pub(crate) struct Chunk {
    /// Chunk start offset
    offset: u64,

    /// Samples of this stream that are part of this chunk
    samples: Vec<Sample>,
}

#[derive(Debug, Clone)]
pub(crate) struct ElstInfo {
    start: Option<gst::Signed<gst::ClockTime>>,
    duration: Option<gst::ClockTime>,
}

#[derive(Debug)]
pub(crate) struct Stream {
    /// Caps of this stream
    caps: gst::Caps,

    /// If this stream has delta frames, and if so if it can have B frames.
    delta_frames: DeltaFrames,

    /// Pre-defined trak timescale if not 0.
    timescale: u32,

    /// Earliest PTS
    ///
    /// If this is >0 then an edit list entry is needed to shift
    earliest_pts: gst::ClockTime,

    /// End PTS
    end_pts: gst::ClockTime,

    /// All the chunks stored for this stream
    chunks: Vec<Chunk>,

    // More data to be included in the fragmented stream header
    extra_header_data: Option<Vec<u8>>,

    /// Orientation from tags
    orientation: Option<ImageOrientation>,

    /// Edit list clipping information
    elst_infos: Vec<ElstInfo>,

    /// Whether this stream should be encoded as an ISO/IEC 23008-12 image sequence
    image_sequence: bool,
}

#[derive(Debug)]
pub(crate) struct Header {
    #[allow(dead_code)]
    variant: Variant,
    /// Pre-defined movie timescale if not 0.
    movie_timescale: u32,
    streams: Vec<Stream>,
    language_code: Option<[u8; 3]>,
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    ISO,
    ONVIF,
}
