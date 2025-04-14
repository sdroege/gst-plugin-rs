// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

mod boxes;
mod imp;
mod obu;

use imp::CAT;

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

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TransformMatrix([[u8; 4]; 9]);

impl std::ops::Deref for TransformMatrix {
    type Target = [[u8; 4]; 9];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Default for &TransformMatrix {
    fn default() -> &'static TransformMatrix {
        &IDENTITY_MATRIX
    }
}

impl TransformMatrix {
    fn from_tag(obj: &impl ObjectSubclass, tag: &gst::event::Tag) -> &'static TransformMatrix {
        gst_video::VideoOrientationMethod::from_tag(tag.tag()).map_or(Default::default(), {
            |orientation| match orientation {
                gst_video::VideoOrientationMethod::Identity => &IDENTITY_MATRIX,
                gst_video::VideoOrientationMethod::_90r => &ROTATE_90R_MATRIX,
                gst_video::VideoOrientationMethod::_180 => &ROTATE_180_MATRIX,
                gst_video::VideoOrientationMethod::_90l => &ROTATE_90L_MATRIX,
                gst_video::VideoOrientationMethod::Horiz => &FLIP_HORZ_MATRIX,
                gst_video::VideoOrientationMethod::Vert => &FLIP_VERT_MATRIX,
                gst_video::VideoOrientationMethod::UrLl => &FLIP_ROTATE_90R_MATRIX,
                gst_video::VideoOrientationMethod::UlLr => &FLIP_ROTATE_90L_MATRIX,
                _ => {
                    gst::info!(
                        CAT,
                        imp = obj,
                        "Orientation {:?} not yet supported",
                        orientation
                    );
                    &IDENTITY_MATRIX
                }
            }
        })
    }
}

macro_rules! tm {
    ( $($v:expr),* ) => {
        TransformMatrix([
            $(
                (($v << 16) as i32).to_be_bytes(),
            )*
        ])
    }
}

// Point (p, q, 1) -> (p', q')
// Matrix (a, b, u,
//         c, d, v,
//         x, y, w)
// Where a, b, c, d, x, y are FP 16.16 and u, v, w are FP 2.30
// m = ap + cq + x
// n = bp + dq + y
// z = up + vq + w
// p' = m/z
// q' = n/z
#[rustfmt::skip]
const IDENTITY_MATRIX: TransformMatrix = tm!(1, 0, 0,
                                             0, 1, 0,
                                             0, 0, (1 << 14));
#[rustfmt::skip]
const FLIP_VERT_MATRIX: TransformMatrix = tm!(1,  0, 0,
                                              0, -1, 0,
                                              0,  0, (1 << 14));
#[rustfmt::skip]
const FLIP_HORZ_MATRIX: TransformMatrix = tm!(-1, 0, 0,
                                               0, 1, 0,
                                               0, 0, (1 << 14));
#[rustfmt::skip]
const ROTATE_90R_MATRIX: TransformMatrix = tm!( 0, 1, 0,
                                               -1, 0, 0,
                                                0, 0, (1 << 14));
#[rustfmt::skip]
const ROTATE_180_MATRIX: TransformMatrix = tm!(-1,  0, 0,
                                                0, -1, 0,
                                                0,  0, (1 << 14));
#[rustfmt::skip]
const ROTATE_90L_MATRIX: TransformMatrix = tm!(0, -1, 0,
                                               1,  0, 0,
                                               0,  0, (1 << 14));
#[rustfmt::skip]
const FLIP_ROTATE_90R_MATRIX: TransformMatrix = tm!( 0, -1, 0,
                                                    -1,  0, 0,
                                                     0,  0, (1 << 14));
#[rustfmt::skip]
const FLIP_ROTATE_90L_MATRIX: TransformMatrix = tm!(0, 1, 0,
                                                    1, 0, 0,
                                                    0, 0, (1 << 14));

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

    // Language code from tags
    language_code: Option<[u8; 3]>,

    /// Bitrate tags
    avg_bitrate: Option<u32>,
    max_bitrate: Option<u32>,

    /// Orientation from tags
    orientation: &'static TransformMatrix,

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
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    ISO,
    ONVIF,
}
