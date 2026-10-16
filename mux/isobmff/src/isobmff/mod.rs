// Copyright (C) 2021-2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::LazyLock;

use gst::glib;
use gst::prelude::*;
#[cfg(feature = "v1_28")]
use gst::tags;

mod ac3;
mod aux_info;
mod boxes;
mod brands;
mod eac3;
mod flac;
mod fmp4mux;
mod mp4mux;
#[cfg(feature = "v1_28")]
mod precision_timestamps;
mod transform_matrix;
mod uncompressed;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "isobmffmux",
        gst::DebugColorFlags::empty(),
        Some("ISO Base Media File Format Mux Element"),
    )
});

/// Supported Bayer format strings for video/x-bayer caps
pub(crate) const BAYER_FORMATS: &[&str] = &[
    "bggr", "gbrg", "grbg", "rggb", "bggr10le", "bggr10be", "gbrg10le", "gbrg10be", "grbg10le",
    "grbg10be", "rggb10le", "rggb10be", "bggr12le", "bggr12be", "gbrg12le", "gbrg12be", "grbg12le",
    "grbg12be", "rggb12le", "rggb12be", "bggr14le", "bggr14be", "gbrg14le", "gbrg14be", "grbg14le",
    "grbg14be", "rggb14le", "rggb14be", "bggr16le", "bggr16be", "gbrg16le", "gbrg16be", "grbg16le",
    "grbg16be", "rggb16le", "rggb16be",
];

glib::wrapper! {
    pub(crate) struct FMP4MuxPad(ObjectSubclass<crate::isobmff::fmp4mux::imp::FMP4MuxPad>) @extends gst_base::AggregatorPad, gst::Pad, gst::Object;
}

glib::wrapper! {
    pub(crate) struct MP4MuxPad(ObjectSubclass<crate::isobmff::mp4mux::imp::MP4MuxPad>) @extends gst_base::AggregatorPad, gst::Pad, gst::Object;
}

glib::wrapper! {
    pub(crate) struct FMP4Mux(ObjectSubclass<crate::isobmff::fmp4mux::imp::FMP4Mux>) @extends gst_base::Aggregator, gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
    pub(crate) struct MP4Mux(ObjectSubclass<crate::isobmff::mp4mux::imp::MP4Mux>) @extends gst_base::Aggregator, gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
    pub(crate) struct CMAFMux(ObjectSubclass<crate::isobmff::fmp4mux::imp::CMAFMux>) @extends FMP4Mux, gst_base::Aggregator, gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
    pub(crate) struct DASHMP4Mux(ObjectSubclass<crate::isobmff::fmp4mux::imp::DASHMP4Mux>) @extends FMP4Mux, gst_base::Aggregator, gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
    pub(crate) struct ISOFMP4Mux(ObjectSubclass<crate::isobmff::fmp4mux::imp::ISOFMP4Mux>) @extends FMP4Mux, gst_base::Aggregator, gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
    pub(crate) struct ISOMP4Mux(ObjectSubclass<crate::isobmff::mp4mux::imp::ISOMP4Mux>) @extends MP4Mux, gst_base::Aggregator, gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
    pub(crate) struct ONVIFFMP4Mux(ObjectSubclass<crate::isobmff::fmp4mux::imp::ONVIFFMP4Mux>) @extends FMP4Mux, gst_base::Aggregator, gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
    pub(crate) struct ONVIFMP4Mux(ObjectSubclass<crate::isobmff::mp4mux::imp::ONVIFMP4Mux>) @extends MP4Mux, gst_base::Aggregator, gst::Element, gst::Object, @implements gst::ChildProxy;
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
        MP4Mux::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
        MP4MuxPad::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
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

    #[cfg(feature = "v1_28")]
    {
        tags::register::<PrecisionClockTypeTag>();
        tags::register::<PrecisionClockTimeUncertaintyNanosecondsTag>();
    }
    Ok(())
}

#[cfg(feature = "v1_28")]
pub enum PrecisionClockTimeUncertaintyNanosecondsTag {}

#[cfg(feature = "v1_28")]
pub enum PrecisionClockTypeTag {}

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

/// Synchronisation capability of clock
///
/// This is used in the TAIClockInfoBox, see ISO/IEC 23001-17:2024 Amd 1.
#[repr(u8)]
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TaicClockType {
    /// Clock type is unknown
    Unknown = 0u8,
    /// Clock does not synchronise to an atomic clock time source
    CannotSync = 1u8,
    // Clock can synchronise to an atomic clock time source
    CanSync = 2u8,
    // Reserved - DO NOT USE
    Reserved = 3u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, glib::Enum, Default)]
#[enum_type(name = "GstFMP4MuxWriteEdtsMode")]
pub(crate) enum WriteEdtsMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg(feature = "v1_28")]
pub(crate) struct TaiClockInfo {
    // Set with the PrecisionClockTimeUncertaintyNanoseconds tag
    // Defaults to unknown
    time_uncertainty: u64,
    // Cannot currently be set, defaults to microsecond
    clock_resolution: u32,
    // Cannot currently be set, defaults to unknown
    clock_drift_rate: i32,
    // Set with the PrecisionClockType tag
    // Defaults to unknown
    clock_type: TaicClockType,
}

// Standard values for taic box (ISO/IEC 23001-17 Amd 1)
#[cfg(feature = "v1_28")]
pub(crate) const TAIC_TIME_UNCERTAINTY_UNKNOWN: u64 = 0xFFFF_FFFF_FFFF_FFFF;
#[cfg(feature = "v1_28")]
pub(crate) const TAIC_CLOCK_DRIFT_RATE_UNKNOWN: i32 = 0x7FFF_FFFF;

// Data for auxiliary information, as used for per-sample timestamps and for protection schemes
#[derive(Clone, Debug, Default, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub(crate) struct AuxiliaryInformation {
    pub(crate) aux_info_type: Option<[u8; 4]>,
    pub(crate) aux_info_type_parameter: u32,
}

// Data for auxiliary information, as used for per-sample timestamps and for protection schemes
#[derive(Clone, Debug, Default)]
pub(crate) struct AuxiliaryInformationData {
    pub(crate) chunk_offsets: VecDeque<u64>,
    pub(crate) entry_lengths: VecDeque<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct ElstInfo {
    pub(crate) start: Option<gst::Signed<gst::ClockTime>>,
    pub(crate) duration: Option<gst::ClockTime>,
}

impl ElstInfo {
    // Builds an edit list entry from the `AudioClippingMeta` of a buffer.
    //
    // The meta amounts are clipped from the start/end of the buffer, so
    // valid data of a buffer with PTS `pts` and end `buffer_end` spans
    // `[pts + clip_start, buffer_end - clip_end]`.
    //
    // `timescale` is the sample rate used to convert `default`-format clip
    // amounts (sample counts) to time. `buffer_end` is the PTS end of the
    // buffer, if known.
    pub(crate) fn from_audio_clipping(
        clip_meta: &gst_audio::AudioClippingMeta,
        timescale: u64,
        pts: gst::ClockTime,
        buffer_end: Option<gst::ClockTime>,
    ) -> Result<Self, anyhow::Error> {
        use anyhow::Context;

        let to_clocktime =
            |t: gst::GenericFormattedValue| -> Result<Option<gst::ClockTime>, anyhow::Error> {
                if let gst::GenericFormattedValue::Default(Some(v)) = t {
                    let v = u64::from(v);
                    let nseconds = v
                        .mul_div_round(gst::ClockTime::SECOND.nseconds(), timescale)
                        .context("Invalid sample count in the AudioClippingMeta")?;

                    Ok(Some(gst::ClockTime::from_nseconds(nseconds)).filter(|x| !x.is_zero()))
                } else if let gst::GenericFormattedValue::Time(Some(v)) = t {
                    Ok(Some(v).filter(|x| !x.is_zero()))
                } else {
                    Ok(None)
                }
            };

        let clip_start = to_clocktime(clip_meta.start())?;
        let clip_end = to_clocktime(clip_meta.end())?;

        if clip_start.is_none() && clip_end.is_none() {
            anyhow::bail!("Start and end time missing in AudioClippingMeta");
        }

        let start = pts + clip_start.unwrap_or(gst::ClockTime::ZERO);
        let duration = match buffer_end {
            Some(buffer_end) => Some(
                buffer_end
                    .saturating_sub(clip_end.unwrap_or(gst::ClockTime::ZERO))
                    .saturating_sub(start),
            ),
            None => {
                if clip_end.is_some() {
                    anyhow::bail!("No duration on buffer, we can't add edit list");
                }
                None
            }
        };

        Ok(ElstInfo {
            start: Some(start.into()),
            duration,
        })
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    CMAF,
    DASH,
    FragmentedISO,
    FragmentedONVIF,
    ISO,
    ONVIF,
}

impl Variant {
    pub(crate) fn is_single_stream(self) -> bool {
        match self {
            Variant::FragmentedISO | Variant::FragmentedONVIF => false,
            Variant::CMAF | Variant::DASH => true,
            Variant::ISO | Variant::ONVIF => todo!(),
        }
    }

    pub(crate) fn is_fragmented(self) -> bool {
        match self {
            Variant::FragmentedISO | Variant::FragmentedONVIF | Variant::CMAF | Variant::DASH => {
                true
            }
            Variant::ISO | Variant::ONVIF => false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PresentationConfiguration {
    pub(crate) variant: Variant,
    pub(crate) update: bool,

    /// Pre-defined movie timescale if not 0.
    pub(crate) movie_timescale: u32,

    /// First caps must be the video/reference stream. Must be in the order the tracks are going to
    /// be used later for the fragments too, for fragmented encoding.
    pub(crate) tracks: Vec<TrackConfiguration>,

    pub(crate) write_mehd: bool,
    pub(crate) duration: Option<gst::ClockTime>,

    /// Whether to write edts box
    pub(crate) write_edts: bool,
}

impl PresentationConfiguration {
    pub(crate) fn to_timescale(&self) -> u32 {
        if self.movie_timescale > 0 {
            self.movie_timescale
        } else {
            // Use the reference track timescale
            self.tracks[0].to_timescale()
        }
    }
}

#[derive(Debug)]
pub(crate) struct Sample {
    /// Sync point
    pub(crate) sync_point: bool,

    /// Sample duration
    pub(crate) duration: gst::ClockTime,

    /// Composition time offset
    ///
    /// This is `None` for streams that have no concept of DTS.
    pub(crate) composition_time_offset: Option<i64>,

    /// Size
    pub(crate) size: u32,

    /// Whether the sample should point to the next sample description
    pub(crate) sample_desc_idx: u32,
}

#[derive(Debug)]
pub(crate) struct Chunk {
    /// Chunk start offset
    pub(crate) offset: u64,

    /// Samples of this stream that are part of this chunk
    pub(crate) samples: Vec<Sample>,
}

#[derive(Debug)]
pub(crate) struct TrackConfiguration {
    /// Caps of this track
    pub(crate) caps: Vec<gst::Caps>,

    /// Set if this is an intra-only tracck
    pub(crate) delta_frames: DeltaFrames,

    /// Pre-defined trak timescale if not 0.
    pub(crate) trak_timescale: u32,

    // More data to be included in the track header
    pub(crate) extra_header_data: Option<Vec<u8>>,

    // Codec-specific boxes to be included in the sample entry
    pub(crate) codec_specific_boxes: Vec<u8>,

    // Tags meta for audio language and video orientation
    pub(crate) language_code: Option<[u8; 3]>,
    pub(crate) orientation: &'static transform_matrix::TransformMatrix,
    pub(crate) avg_bitrate: Option<u32>,
    pub(crate) max_bitrate: Option<u32>,

    /// Edit list clipping information
    pub(crate) elst_infos: Vec<ElstInfo>,

    /// Earliest PTS
    ///
    /// If this is >0 then an edit list entry is needed to shift
    /// Only used for non-fragmented track
    pub(crate) earliest_pts: gst::ClockTime,

    /// End PTS
    /// Only used for non-fragmented track
    pub(crate) end_pts: gst::ClockTime,

    /// All the chunks stored for this stream
    /// Only used for non-fragmented track
    pub(crate) chunks: Vec<Chunk>,

    /// Whether this stream should be encoded as an ISO/IEC 23008-12 image sequence
    pub(crate) image_sequence: bool,

    /// TAI Clock information (ISO/IEC 23001-17 Amd 1)
    #[cfg(feature = "v1_28")]
    pub(crate) tai_clock_info: Option<TaiClockInfo>,

    /// Sample auxiliary information (ISO/IEC 14496-12:2022 Section 8.7.8 and 8.7.9)
    pub(crate) auxiliary_info: BTreeMap<AuxiliaryInformation, AuxiliaryInformationData>,

    /// Information needed for creating `chnl` box
    chnl_layout_info: Option<ChnlLayoutInfo>,
}

pub(crate) fn caps_to_timescale(caps: &gst::CapsRef) -> u32 {
    let s = caps.structure(0).unwrap();

    match s.get::<gst::Fraction>("framerate") {
        Ok(fps) => {
            if fps.numer() == 0 {
                return 10_000;
            }

            if fps.denom() != 1
                && fps.denom() != 1001
                && let Some(fps) = (fps.denom() as u64)
                    .nseconds()
                    .mul_div_round(1_000_000_000, fps.numer() as u64)
                    .and_then(gst_video::guess_framerate)
            {
                return (fps.numer() as u32)
                    .mul_div_round(100, fps.denom() as u32)
                    .unwrap_or(10_000);
            }

            if fps.denom() == 1001 {
                fps.numer() as u32
            } else {
                (fps.numer() as u32)
                    .mul_div_round(100, fps.denom() as u32)
                    .unwrap_or(10_000)
            }
        }
        _ => match s.get::<i32>("rate") {
            Ok(rate) => rate as u32,
            _ => 10_000,
        },
    }
}

impl TrackConfiguration {
    pub(crate) fn to_timescale(&self) -> u32 {
        if self.trak_timescale > 0 {
            self.trak_timescale
        } else {
            caps_to_timescale(self.caps())
        }
    }

    pub(crate) fn caps(&self) -> &gst::Caps {
        self.caps.first().unwrap()
    }

    pub(crate) fn stream_entry_count(&self) -> usize {
        self.caps.len()
    }
}

#[derive(Debug)]
pub(crate) struct Buffer {
    /// Track index
    pub(crate) idx: usize,

    /// Actual buffer
    pub(crate) buffer: gst::Buffer,

    /// Timestamp (PTS or DTS)
    pub(crate) timestamp: gst::Signed<gst::ClockTime>,

    /// Sample duration
    pub(crate) duration: gst::ClockTime,

    /// Composition time offset
    pub(crate) composition_time_offset: Option<i64>,
}

#[derive(Debug)]
pub(crate) struct FragmentHeaderConfiguration<'a> {
    pub(crate) variant: Variant,

    /// Sequence number for this fragment.
    pub(crate) sequence_number: u32,

    /// If this is a full fragment or only a chunk.
    pub(crate) chunk: bool,

    pub(crate) streams: &'a [FragmentHeaderStream],
    pub(crate) buffers: &'a [Buffer],

    /// If this is for the last fragment.
    pub(crate) last_fragment: bool,
}

#[derive(Debug)]
pub(crate) struct FragmentHeaderStream {
    /// Caps of this stream
    pub(crate) caps: gst::Caps,

    /// Set if this is an intra-only stream
    pub(crate) delta_frames: DeltaFrames,

    /// Pre-defined trak timescale if not 0.
    pub(crate) trak_timescale: u32,

    /// Start time of this fragment
    ///
    /// `None` if this stream has no buffers in this fragment.
    pub(crate) start_time: Option<gst::ClockTime>,

    /// Start NTP time of this fragment
    ///
    /// This is in nanoseconds since epoch and is used for writing the prft box if present.
    ///
    /// Only the first track is ever used.
    pub(crate) start_ntp_time: Option<gst::ClockTime>,
}

impl FragmentHeaderStream {
    pub(crate) fn to_timescale(&self) -> u32 {
        if self.trak_timescale > 0 {
            self.trak_timescale
        } else {
            caps_to_timescale(&self.caps)
        }
    }
}

#[derive(Debug)]
pub(crate) struct FragmentOffset {
    pub(crate) time: gst::ClockTime,
    pub(crate) offset: u64,
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
    pub(crate) fn try_parse(
        event: &gst::event::CustomDownstream,
    ) -> Option<Result<Self, glib::BoolError>> {
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

#[derive(Debug, Clone)]
pub(crate) struct ChnlLayoutInfo {
    audio_info: gst_audio::AudioInfo,
    layout_idx: u8, /* Must be u8 for `chnl` box */
    reorder_map: Option<Vec<usize>>,
    omitted_channels_map: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst::format::DefaultFormatConstructor;

    fn clipping_elst_info(
        clip_start: gst::ClockTime,
        clip_end: gst::ClockTime,
        pts: gst::ClockTime,
        buffer_end: Option<gst::ClockTime>,
    ) -> Result<ElstInfo, anyhow::Error> {
        gst::init().unwrap();

        let mut buffer = gst::Buffer::with_size(1).unwrap();
        let cmeta =
            gst_audio::AudioClippingMeta::add(buffer.get_mut().unwrap(), clip_start, clip_end);

        ElstInfo::from_audio_clipping(&cmeta, 48_000, pts, buffer_end)
    }

    #[test]
    fn clipping_head_and_tail() {
        // Buffer [1s, 3s) with 250ms clipped from each side: the valid data is
        // [1.25s, 2.75s).
        let info = clipping_elst_info(
            gst::ClockTime::from_mseconds(250),
            gst::ClockTime::from_mseconds(250),
            gst::ClockTime::from_seconds(1),
            Some(gst::ClockTime::from_seconds(3)),
        )
        .unwrap();

        assert_eq!(
            info.start,
            Some(gst::Signed::Positive(gst::ClockTime::from_mseconds(1250)))
        );
        assert_eq!(info.duration, Some(gst::ClockTime::from_mseconds(1500)));
    }

    #[test]
    fn clipping_tail_only() {
        // With no clipping from the start, the valid data still starts at the
        // buffer PTS.
        let info = clipping_elst_info(
            gst::ClockTime::ZERO,
            gst::ClockTime::from_mseconds(250),
            gst::ClockTime::from_seconds(1),
            Some(gst::ClockTime::from_seconds(3)),
        )
        .unwrap();

        assert_eq!(
            info.start,
            Some(gst::Signed::Positive(gst::ClockTime::from_seconds(1)))
        );
        assert_eq!(info.duration, Some(gst::ClockTime::from_mseconds(1750)));
    }

    #[test]
    fn clipping_head_only() {
        let info = clipping_elst_info(
            gst::ClockTime::from_mseconds(250),
            gst::ClockTime::ZERO,
            gst::ClockTime::from_seconds(1),
            Some(gst::ClockTime::from_seconds(3)),
        )
        .unwrap();

        assert_eq!(
            info.start,
            Some(gst::Signed::Positive(gst::ClockTime::from_mseconds(1250)))
        );
        assert_eq!(info.duration, Some(gst::ClockTime::from_mseconds(1750)));
    }

    #[test]
    fn clipping_sample_units() {
        gst::init().unwrap();

        // 12000 samples at 48kHz is 250ms, as in `clipping_head_and_tail`.
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        let cmeta = gst_audio::AudioClippingMeta::add(
            buffer.get_mut().unwrap(),
            12_000u64.default_format(),
            12_000u64.default_format(),
        );

        let info = ElstInfo::from_audio_clipping(
            &cmeta,
            48_000,
            gst::ClockTime::from_seconds(1),
            Some(gst::ClockTime::from_seconds(3)),
        )
        .unwrap();

        assert_eq!(
            info.start,
            Some(gst::Signed::Positive(gst::ClockTime::from_mseconds(1250)))
        );
        assert_eq!(info.duration, Some(gst::ClockTime::from_mseconds(1500)));
    }

    #[test]
    fn clipping_unknown_buffer_end() {
        // Without the buffer end the edit duration stays unknown and is
        // filled in later.
        let info = clipping_elst_info(
            gst::ClockTime::from_mseconds(250),
            gst::ClockTime::ZERO,
            gst::ClockTime::from_seconds(1),
            None,
        )
        .unwrap();

        assert_eq!(
            info.start,
            Some(gst::Signed::Positive(gst::ClockTime::from_mseconds(1250)))
        );
        assert_eq!(info.duration, None);

        // Clipping from the end is required to compute the edit duration.
        assert!(
            clipping_elst_info(
                gst::ClockTime::ZERO,
                gst::ClockTime::from_mseconds(250),
                gst::ClockTime::from_seconds(1),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn clipping_exceeding_buffer() {
        // Clips larger than the buffer leave a zero-length edit.
        let info = clipping_elst_info(
            gst::ClockTime::from_seconds(2),
            gst::ClockTime::from_seconds(2),
            gst::ClockTime::ZERO,
            Some(gst::ClockTime::from_seconds(1)),
        )
        .unwrap();

        assert_eq!(
            info.start,
            Some(gst::Signed::Positive(gst::ClockTime::from_seconds(2)))
        );
        assert_eq!(info.duration, Some(gst::ClockTime::ZERO));
    }
}
