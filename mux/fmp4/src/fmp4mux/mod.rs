// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::fmp4mux::imp::CAT;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

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
    if !gst::meta::CustomMeta::is_registered("FMP4KeyframeMeta") {
        gst::meta::CustomMeta::register("FMP4KeyframeMeta", &[]);
    }

    #[cfg(feature = "doc")]
    {
        FMP4Mux::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
        FMP4MuxPad::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
        HeaderUpdateMode::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
        WriteEdtsMode::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
        ChunkMode::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
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

    /// Start UTC time in ONVIF mode.
    /// Since Jan 1 1601 in 100ns units.
    start_utc_time: Option<u64>,

    /// Whether to write edts box
    write_edts: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ElstInfo {
    start: Option<gst::Signed<gst::ClockTime>>,
    duration: Option<gst::ClockTime>,
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

    // Codec-specific boxes to be included in the sample entry
    codec_specific_boxes: Vec<u8>,

    // Tags meta for audio language and video orientation
    language_code: Option<[u8; 3]>,
    orientation: &'static TransformMatrix,
    avg_bitrate: Option<u32>,
    max_bitrate: Option<u32>,

    /// Edit list clipping information
    elst_infos: Vec<ElstInfo>,
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

    /// If this is for the last fragment.
    last_fragment: bool,
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

    /// Start NTP time of this fragment
    ///
    /// This is in nanoseconds since epoch and is used for writing the prft box if present.
    ///
    /// Only the first track is ever used.
    start_ntp_time: Option<gst::ClockTime>,
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

    /// Timestamp (PTS or DTS)
    timestamp: gst::Signed<gst::ClockTime>,

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

/**
 * GstFMP4MuxHeaderUpdateMode:
 *
 * How and when updating of the header (`moov`, initialization segment) is allowed.
 */
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[repr(i32)]
#[enum_type(name = "GstFMP4MuxHeaderUpdateMode")]
pub(crate) enum HeaderUpdateMode {
    /**
     * GstFMP4MuxHeaderUpdateMode:none:
     *
     * Don't allow and do any header updates at all.
     *
     * Caps changes are not allowed in this mode.
     */
    None,
    /**
     * GstFMP4MuxHeaderUpdateMode:rewrite:
     *
     * Try rewriting the initial header with the overall duration at the very end.
     *
     * Caps changes are not allowed in this mode.
     */
    Rewrite,
    /**
     * GstFMP4MuxHeaderUpdateMode:update:
     *
     * Send an updated version of the initial header with the overall duration at
     * the very end.
     *
     * Caps changes are not allowed in this mode.
     */
    Update,
    /**
     * GstFMP4MuxHeaderUpdateMode:caps:
     *
     * Send an updated header whenever caps or tag changes are pending that affect the initial
     * header. The updated header does not have the duration set and will always be followed by a
     * new fragment.
     *
     * Since: plugins-rs-0.14.0
     */
    Caps,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, glib::Enum, Default)]
#[enum_type(name = "GstFMP4MuxWriteEdtsMode")]
pub(crate) enum WriteEdtsMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug)]
pub(crate) struct SplitNowEvent {
    pub chunk: bool,
}

impl From<&SplitNowEvent> for gst::Event {
    fn from(value: &SplitNowEvent) -> Self {
        gst::event::CustomDownstream::builder(
            gst::Structure::builder("FMP4MuxSplitNow")
                .field("chunk", value.chunk)
                .build(),
        )
        .build()
    }
}

impl From<SplitNowEvent> for gst::Event {
    fn from(value: SplitNowEvent) -> Self {
        gst::Event::from(&value)
    }
}

impl SplitNowEvent {
    fn try_parse(event: &gst::event::CustomDownstream) -> Option<Result<Self, glib::BoolError>> {
        let s = event.structure()?;
        if s.name() != "FMP4MuxSplitNow" {
            return None;
        }

        let chunk = match s
            .get_optional::<bool>("chunk")
            .map_err(|e| glib::bool_error!("Invalid SplitNow event with wrong chunk field: {e}"))
        {
            Ok(chunk) => chunk.unwrap_or(false),
            Err(err) => return Some(Err(err)),
        };

        Some(Ok(SplitNowEvent { chunk }))
    }
}

/**
 * GstFMP4MuxChunkMode:
 *
 * Whether chunking is done based on duration or key frames.
 */
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[repr(i32)]
#[enum_type(name = "GstFMP4MuxChunkMode")]
pub(crate) enum ChunkMode {
    /**
     * GstFMP4MuxChunkMode:none:
     *
     * Chunk mode not set.
     */
    None,
    /**
     * GstFMP4MuxChunkMode:duration:
     *
     * Chunk based on duration.
     */
    Duration,
    /**
     * GstFMP4MuxChunkMode:keyframe:
     *
     * Chunk on key frame boundaries.
     */
    Keyframe,
}
