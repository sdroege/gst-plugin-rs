// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

use anyhow::{anyhow, bail, Context, Error};
use gst_video::VideoFormat;
use std::str::FromStr;
use std::sync::LazyLock;
use std::{collections::BTreeMap, convert::TryFrom};

use super::{ImageOrientation, IDENTITY_MATRIX};

fn write_box<T, F: FnOnce(&mut Vec<u8>) -> Result<T, Error>>(
    vec: &mut Vec<u8>,
    fourcc: impl std::borrow::Borrow<[u8; 4]>,
    content_func: F,
) -> Result<T, Error> {
    // Write zero size ...
    let size_pos = vec.len();
    vec.extend([0u8; 4]);
    vec.extend(fourcc.borrow());

    let res = content_func(vec)?;

    // ... and update it here later.
    let size: u32 = vec
        .len()
        .checked_sub(size_pos)
        .expect("vector shrunk")
        .try_into()
        .context("too big box content")?;
    vec[size_pos..][..4].copy_from_slice(&size.to_be_bytes());

    Ok(res)
}

const FULL_BOX_VERSION_0: u8 = 0;
const FULL_BOX_VERSION_1: u8 = 1;

const FULL_BOX_FLAGS_NONE: u32 = 0;

static CAT_23001: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "mp4mux-23001-17",
        gst::DebugColorFlags::empty(),
        Some("MP4Mux Element - ISO/IEC 23001-17"),
    )
});

fn write_full_box<T, F: FnOnce(&mut Vec<u8>) -> Result<T, Error>>(
    vec: &mut Vec<u8>,
    fourcc: impl std::borrow::Borrow<[u8; 4]>,
    version: u8,
    flags: u32,
    content_func: F,
) -> Result<T, Error> {
    write_box(vec, fourcc, move |vec| {
        assert_eq!(flags >> 24, 0);
        vec.extend(((u32::from(version) << 24) | flags).to_be_bytes());
        content_func(vec)
    })
}

/// Creates `ftyp` box
pub(super) fn create_ftyp(
    major_brand: &[u8; 4],
    minor_version: u32,
    compatible_brands: Vec<&[u8; 4]>,
) -> Result<gst::Buffer, Error> {
    let mut v = vec![];
    write_box(&mut v, b"ftyp", |v| {
        // major brand
        v.extend(major_brand);
        v.extend(minor_version.to_be_bytes());
        // compatible brands
        v.extend(compatible_brands.into_iter().flatten());

        Ok(())
    })?;

    Ok(gst::Buffer::from_mut_slice(v))
}

/// Creates `mdat` box *header*.
pub(super) fn create_mdat_header(size: Option<u64>) -> Result<gst::Buffer, Error> {
    let mut v = vec![];

    if let Some(size) = size {
        if let Ok(size) = u32::try_from(size + 8) {
            v.extend(8u32.to_be_bytes());
            v.extend(b"free");
            v.extend(size.to_be_bytes());
            v.extend(b"mdat");
        } else {
            v.extend(1u32.to_be_bytes());
            v.extend(b"mdat");
            v.extend((size + 16).to_be_bytes());
        }
    } else {
        v.extend(8u32.to_be_bytes());
        v.extend(b"free");
        v.extend(0u32.to_be_bytes());
        v.extend(b"mdat");
    }

    Ok(gst::Buffer::from_mut_slice(v))
}

/// Offset between UNIX epoch and Jan 1 1601 epoch in seconds.
/// 1601 = UNIX + UNIX_1601_OFFSET.
const UNIX_1601_OFFSET: u64 = 11_644_473_600;

/// Creates `moov` box
pub(super) fn create_moov(header: super::Header) -> Result<gst::Buffer, Error> {
    let mut v = vec![];

    write_box(&mut v, b"moov", |v| write_moov(v, &header))?;

    if header.variant == super::Variant::ONVIF {
        write_full_box(
            &mut v,
            b"meta",
            FULL_BOX_VERSION_0,
            FULL_BOX_FLAGS_NONE,
            |v| {
                write_full_box(v, b"hdlr", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
                    // Handler type
                    v.extend(b"null");

                    // Reserved
                    v.extend([0u8; 3 * 4]);

                    // Name
                    v.extend(b"MetadataHandler");

                    Ok(())
                })?;

                write_box(v, b"cstb", |v| {
                    // entry count
                    v.extend(1u32.to_be_bytes());

                    // track id
                    v.extend(0u32.to_be_bytes());

                    // start UTC time in 100ns units since Jan 1 1601
                    // This is the UTC time of the earliest stream, which has to be converted to
                    // the correct epoch and scale.
                    let start_utc_time = header
                        .streams
                        .iter()
                        .map(|s| s.earliest_pts)
                        .min()
                        .unwrap()
                        .nseconds()
                        / 100;
                    let start_utc_time = start_utc_time + UNIX_1601_OFFSET * 10_000_000;
                    v.extend(start_utc_time.to_be_bytes());

                    Ok(())
                })
            },
        )?;
    }

    Ok(gst::Buffer::from_mut_slice(v))
}

struct TrackReference {
    reference_type: [u8; 4],
    track_ids: Vec<u32>,
}

fn write_moov(v: &mut Vec<u8>, header: &super::Header) -> Result<(), Error> {
    use gst::glib;

    let base = glib::DateTime::from_utc(1904, 1, 1, 0, 0, 0.0)?;
    let now = glib::DateTime::now_utc()?;
    let creation_time =
        u64::try_from(now.difference(&base).as_seconds()).expect("time before 1904");

    write_full_box(v, b"mvhd", FULL_BOX_VERSION_1, FULL_BOX_FLAGS_NONE, |v| {
        write_mvhd(v, header, creation_time)
    })?;
    for (idx, stream) in header.streams.iter().enumerate() {
        write_box(v, b"trak", |v| {
            let mut references = Vec::new();

            // Reference the video track for ONVIF metadata tracks
            if header.variant == super::Variant::ONVIF
                && stream.caps.structure(0).unwrap().name() == "application/x-onvif-metadata"
            {
                // Find the first video track
                for (idx, other_stream) in header.streams.iter().enumerate() {
                    let s = other_stream.caps.structure(0).unwrap();

                    if matches!(
                        s.name().as_str(),
                        "video/x-h264" | "video/x-h265" | "image/jpeg" | "video/x-raw"
                    ) {
                        references.push(TrackReference {
                            reference_type: *b"cdsc",
                            track_ids: vec![idx as u32 + 1],
                        });
                        break;
                    }
                }
            }

            write_trak(v, header, idx, stream, creation_time, &references)
        })?;
    }

    Ok(())
}

fn header_to_timescale(header: &super::Header) -> u32 {
    if header.movie_timescale > 0 {
        header.movie_timescale
    } else {
        // Use the reference track timescale
        header.streams[0].timescale
    }
}

fn write_mvhd(v: &mut Vec<u8>, header: &super::Header, creation_time: u64) -> Result<(), Error> {
    let timescale = header_to_timescale(header);

    // Creation time
    v.extend(creation_time.to_be_bytes());
    // Modification time
    v.extend(creation_time.to_be_bytes());
    // Timescale
    v.extend(timescale.to_be_bytes());
    // Duration
    let min_earliest_pts = header.streams.iter().map(|s| s.earliest_pts).min().unwrap();
    let max_end_pts = header
        .streams
        .iter()
        .map(|stream| stream.end_pts)
        .max()
        .unwrap();
    let duration = (max_end_pts - min_earliest_pts)
        .nseconds()
        .mul_div_round(timescale as u64, gst::ClockTime::SECOND.nseconds())
        .context("too big track duration")?;
    v.extend(duration.to_be_bytes());

    // Rate 1.0
    v.extend((1u32 << 16).to_be_bytes());
    // Volume 1.0
    v.extend((1u16 << 8).to_be_bytes());
    // Reserved
    v.extend([0u8; 2 + 2 * 4]);

    // Matrix
    v.extend(
        [
            (1u32 << 16).to_be_bytes(),
            0u32.to_be_bytes(),
            0u32.to_be_bytes(),
            0u32.to_be_bytes(),
            (1u32 << 16).to_be_bytes(),
            0u32.to_be_bytes(),
            0u32.to_be_bytes(),
            0u32.to_be_bytes(),
            (16384u32 << 16).to_be_bytes(),
        ]
        .into_iter()
        .flatten(),
    );

    // Pre defined
    v.extend([0u8; 6 * 4]);

    // Next track id
    v.extend((header.streams.len() as u32 + 1).to_be_bytes());

    Ok(())
}

const TKHD_FLAGS_TRACK_ENABLED: u32 = 0x1;
const TKHD_FLAGS_TRACK_IN_MOVIE: u32 = 0x2;
const TKHD_FLAGS_TRACK_IN_PREVIEW: u32 = 0x4;

fn write_trak(
    v: &mut Vec<u8>,
    header: &super::Header,
    idx: usize,
    stream: &super::Stream,
    creation_time: u64,
    references: &[TrackReference],
) -> Result<(), Error> {
    write_full_box(
        v,
        b"tkhd",
        FULL_BOX_VERSION_1,
        TKHD_FLAGS_TRACK_ENABLED | TKHD_FLAGS_TRACK_IN_MOVIE | TKHD_FLAGS_TRACK_IN_PREVIEW,
        |v| write_tkhd(v, header, idx, stream, creation_time),
    )?;

    write_box(v, b"mdia", |v| write_mdia(v, header, stream, creation_time))?;
    if !references.is_empty() {
        write_box(v, b"tref", |v| write_tref(v, header, references))?;
    }
    write_box(v, b"edts", |v| write_edts(v, stream))?;

    Ok(())
}

fn write_tkhd(
    v: &mut Vec<u8>,
    header: &super::Header,
    idx: usize,
    stream: &super::Stream,
    creation_time: u64,
) -> Result<(), Error> {
    // Creation time
    v.extend(creation_time.to_be_bytes());
    // Modification time
    v.extend(creation_time.to_be_bytes());
    // Track ID
    v.extend((idx as u32 + 1).to_be_bytes());
    // Reserved
    v.extend(0u32.to_be_bytes());
    // Duration

    // Track header duration is in movie header timescale
    let timescale = header_to_timescale(header);

    let min_earliest_pts = header.streams.iter().map(|s| s.earliest_pts).min().unwrap();
    // Duration is the end PTS of this stream up to the beginning of the earliest stream
    let duration = stream.end_pts - min_earliest_pts;
    let duration = duration
        .nseconds()
        .mul_div_round(timescale as u64, gst::ClockTime::SECOND.nseconds())
        .context("too big track duration")?;
    v.extend(duration.to_be_bytes());

    // Reserved
    v.extend([0u8; 2 * 4]);

    // Layer
    v.extend(0u16.to_be_bytes());
    // Alternate group
    v.extend(0u16.to_be_bytes());

    // Volume
    let s = stream.caps.structure(0).unwrap();
    match s.name().as_str() {
        "audio/mpeg" | "audio/x-opus" | "audio/x-flac" | "audio/x-alaw" | "audio/x-mulaw"
        | "audio/x-adpcm" => v.extend((1u16 << 8).to_be_bytes()),
        _ => v.extend(0u16.to_be_bytes()),
    }

    // Reserved
    v.extend([0u8; 2]);

    // Matrix
    let matrix = match s.name().as_str() {
        x if x.starts_with("video/") || x.starts_with("image/") => stream
            .orientation
            .unwrap_or(ImageOrientation::Rotate0)
            .transform_matrix(),
        _ => &IDENTITY_MATRIX,
    };
    v.extend(matrix.iter().flatten());

    // Width/height
    match s.name().as_str() {
        "video/x-h264" | "video/x-h265" | "video/x-vp8" | "video/x-vp9" | "video/x-av1"
        | "image/jpeg" | "video/x-raw" => {
            let width = s.get::<i32>("width").context("video caps without width")? as u32;
            let height = s
                .get::<i32>("height")
                .context("video caps without height")? as u32;
            let par = s
                .get::<gst::Fraction>("pixel-aspect-ratio")
                .unwrap_or_else(|_| gst::Fraction::new(1, 1));

            let width = std::cmp::min(
                width
                    .mul_div_round(par.numer() as u32, par.denom() as u32)
                    .unwrap_or(u16::MAX as u32),
                u16::MAX as u32,
            );
            let height = std::cmp::min(height, u16::MAX as u32);

            v.extend((width << 16).to_be_bytes());
            v.extend((height << 16).to_be_bytes());
        }
        _ => v.extend([0u8; 2 * 4]),
    }

    Ok(())
}

fn write_mdia(
    v: &mut Vec<u8>,
    header: &super::Header,
    stream: &super::Stream,
    creation_time: u64,
) -> Result<(), Error> {
    write_full_box(v, b"mdhd", FULL_BOX_VERSION_1, FULL_BOX_FLAGS_NONE, |v| {
        write_mdhd(v, header, stream, creation_time)
    })?;
    write_full_box(v, b"hdlr", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_hdlr(v, header, stream)
    })?;

    // TODO: write elng if needed

    write_box(v, b"minf", |v| write_minf(v, header, stream))?;

    Ok(())
}

fn language_code(lang: impl std::borrow::Borrow<[u8; 3]>) -> u16 {
    let lang = lang.borrow();

    assert!(lang.iter().all(u8::is_ascii_lowercase));

    (((lang[0] as u16 - 0x60) & 0x1F) << 10)
        + (((lang[1] as u16 - 0x60) & 0x1F) << 5)
        + ((lang[2] as u16 - 0x60) & 0x1F)
}

fn write_mdhd(
    v: &mut Vec<u8>,
    header: &super::Header,
    stream: &super::Stream,
    creation_time: u64,
) -> Result<(), Error> {
    let timescale = stream.timescale;

    // Creation time
    v.extend(creation_time.to_be_bytes());
    // Modification time
    v.extend(creation_time.to_be_bytes());
    // Timescale
    v.extend(timescale.to_be_bytes());
    // Duration
    let duration = stream
        .chunks
        .iter()
        .flat_map(|c| c.samples.iter().map(|b| b.duration.nseconds()))
        .sum::<u64>()
        .mul_div_round(timescale as u64, gst::ClockTime::SECOND.nseconds())
        .context("too big track duration")?;
    v.extend(duration.to_be_bytes());

    // Language as ISO-639-2/T
    if let Some(lang) = header.language_code {
        v.extend(language_code(lang).to_be_bytes());
    } else {
        v.extend(language_code(b"und").to_be_bytes());
    }

    // Pre-defined
    v.extend([0u8; 2]);

    Ok(())
}

fn write_hdlr(
    v: &mut Vec<u8>,
    _header: &super::Header,
    stream: &super::Stream,
) -> Result<(), Error> {
    // Pre-defined
    v.extend([0u8; 4]);

    let s = stream.caps.structure(0).unwrap();
    let (handler_type, name) = match s.name().as_str() {
        "video/x-h264" | "video/x-h265" | "video/x-vp8" | "video/x-vp9" | "video/x-av1"
        | "image/jpeg" | "video/x-raw" => {
            if stream.image_sequence {
                // See ISO/IEC 23008-12:2022 Section 7.2.2
                (b"pict", b"PictureHandler\0".as_slice())
            } else {
                (b"vide", b"VideoHandler\0".as_slice())
            }
        }
        "audio/mpeg" | "audio/x-opus" | "audio/x-flac" | "audio/x-alaw" | "audio/x-mulaw"
        | "audio/x-adpcm" => (b"soun", b"SoundHandler\0".as_slice()),
        "application/x-onvif-metadata" => (b"meta", b"MetadataHandler\0".as_slice()),
        _ => unreachable!(),
    };

    // Handler type
    v.extend(handler_type);

    // Reserved
    v.extend([0u8; 3 * 4]);

    // Name
    v.extend(name);

    Ok(())
}

fn write_minf(
    v: &mut Vec<u8>,
    header: &super::Header,
    stream: &super::Stream,
) -> Result<(), Error> {
    let s = stream.caps.structure(0).unwrap();

    match s.name().as_str() {
        "video/x-h264" | "video/x-h265" | "video/x-vp8" | "video/x-vp9" | "video/x-av1"
        | "image/jpeg" | "video/x-raw" => {
            // Flags are always 1 for unspecified reasons
            write_full_box(v, b"vmhd", FULL_BOX_VERSION_0, 1, |v| write_vmhd(v, header))?
        }
        "audio/mpeg" | "audio/x-opus" | "audio/x-flac" | "audio/x-alaw" | "audio/x-mulaw"
        | "audio/x-adpcm" => {
            write_full_box(v, b"smhd", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
                write_smhd(v, header)
            })?
        }
        "application/x-onvif-metadata" => {
            write_full_box(v, b"nmhd", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |_v| {
                Ok(())
            })?
        }
        _ => unreachable!(),
    }

    write_box(v, b"dinf", |v| write_dinf(v, header))?;

    write_box(v, b"stbl", |v| write_stbl(v, header, stream))?;

    Ok(())
}

fn write_vmhd(v: &mut Vec<u8>, _header: &super::Header) -> Result<(), Error> {
    // Graphics mode
    v.extend([0u8; 2]);

    // opcolor
    v.extend([0u8; 2 * 3]);

    Ok(())
}

fn write_smhd(v: &mut Vec<u8>, _header: &super::Header) -> Result<(), Error> {
    // Balance
    v.extend([0u8; 2]);

    // Reserved
    v.extend([0u8; 2]);

    Ok(())
}

fn write_dinf(v: &mut Vec<u8>, header: &super::Header) -> Result<(), Error> {
    write_full_box(v, b"dref", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_dref(v, header)
    })?;

    Ok(())
}

const DREF_FLAGS_MEDIA_IN_SAME_FILE: u32 = 0x1;

fn write_dref(v: &mut Vec<u8>, _header: &super::Header) -> Result<(), Error> {
    // Entry count
    v.extend(1u32.to_be_bytes());

    write_full_box(
        v,
        b"url ",
        FULL_BOX_VERSION_0,
        DREF_FLAGS_MEDIA_IN_SAME_FILE,
        |_v| Ok(()),
    )?;

    Ok(())
}

fn write_stbl(
    v: &mut Vec<u8>,
    header: &super::Header,
    stream: &super::Stream,
) -> Result<(), Error> {
    write_full_box(v, b"stsd", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_stsd(v, header, stream)
    })?;
    write_full_box(v, b"stts", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_stts(v, header, stream)
    })?;

    // If there are any composition time offsets we need to write the ctts box. If any are negative
    // we need to write version 1 of the box, otherwise version 0 is sufficient.
    let mut need_ctts = None;
    if stream.delta_frames.requires_dts() {
        for composition_time_offset in stream.chunks.iter().flat_map(|c| {
            c.samples.iter().map(|b| {
                b.composition_time_offset
                    .expect("not all samples have a composition time offset")
            })
        }) {
            if composition_time_offset < 0 {
                need_ctts = Some(1);
                break;
            } else if composition_time_offset != 0 {
                need_ctts = Some(0);
            }
        }
    }
    if let Some(need_ctts) = need_ctts {
        let version = if need_ctts == 0 {
            FULL_BOX_VERSION_0
        } else {
            FULL_BOX_VERSION_1
        };

        write_full_box(v, b"ctts", version, FULL_BOX_FLAGS_NONE, |v| {
            write_ctts(v, header, stream, version)
        })?;

        write_full_box(v, b"cslg", FULL_BOX_VERSION_1, FULL_BOX_FLAGS_NONE, |v| {
            write_cslg(v, header, stream)
        })?;
    }

    // If any sample is not a sync point, write the stss box
    if !stream.delta_frames.intra_only()
        && stream
            .chunks
            .iter()
            .flat_map(|c| c.samples.iter().map(|b| b.sync_point))
            .any(|sync_point| !sync_point)
    {
        write_full_box(v, b"stss", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
            write_stss(v, header, stream)
        })?;
    }

    write_full_box(v, b"stsz", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_stsz(v, header, stream)
    })?;

    write_full_box(v, b"stsc", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_stsc(v, header, stream)
    })?;

    if stream.chunks.last().unwrap().offset > u32::MAX as u64 {
        write_full_box(v, b"co64", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
            write_stco(v, header, stream, true)
        })?;
    } else {
        write_full_box(v, b"stco", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
            write_stco(v, header, stream, false)
        })?;
    }

    Ok(())
}

fn write_stsd(
    v: &mut Vec<u8>,
    header: &super::Header,
    stream: &super::Stream,
) -> Result<(), Error> {
    // Entry count
    v.extend(1u32.to_be_bytes());

    let s = stream.caps.structure(0).unwrap();
    match s.name().as_str() {
        "video/x-h264" | "video/x-h265" | "video/x-vp8" | "video/x-vp9" | "video/x-av1"
        | "image/jpeg" | "video/x-raw" => write_visual_sample_entry(v, header, stream)?,
        "audio/mpeg" | "audio/x-opus" | "audio/x-flac" | "audio/x-alaw" | "audio/x-mulaw"
        | "audio/x-adpcm" => write_audio_sample_entry(v, header, stream)?,
        "application/x-onvif-metadata" => write_xml_meta_data_sample_entry(v, header, stream)?,
        _ => unreachable!(),
    }

    Ok(())
}

fn write_sample_entry_box<T, F: FnOnce(&mut Vec<u8>) -> Result<T, Error>>(
    v: &mut Vec<u8>,
    fourcc: impl std::borrow::Borrow<[u8; 4]>,
    content_func: F,
) -> Result<T, Error> {
    write_box(v, fourcc, move |v| {
        // Reserved
        v.extend([0u8; 6]);

        // Data reference index
        v.extend(1u16.to_be_bytes());

        content_func(v)
    })
}

fn write_visual_sample_entry(
    v: &mut Vec<u8>,
    _header: &super::Header,
    stream: &super::Stream,
) -> Result<(), Error> {
    let s = stream.caps.structure(0).unwrap();
    let fourcc = match s.name().as_str() {
        "video/x-h264" => {
            let stream_format = s.get::<&str>("stream-format").context("no stream-format")?;
            match stream_format {
                "avc" => b"avc1",
                "avc3" => b"avc3",
                _ => unreachable!(),
            }
        }
        "video/x-h265" => {
            let stream_format = s.get::<&str>("stream-format").context("no stream-format")?;
            match stream_format {
                "hvc1" => b"hvc1",
                "hev1" => b"hev1",
                _ => unreachable!(),
            }
        }
        "image/jpeg" => b"jpeg",
        "video/x-vp8" => b"vp08",
        "video/x-vp9" => b"vp09",
        "video/x-av1" => b"av01",
        "video/x-raw" => b"uncv",
        _ => unreachable!(),
    };

    write_sample_entry_box(v, fourcc, move |v| {
        // pre-defined
        v.extend([0u8; 2]);
        // Reserved
        v.extend([0u8; 2]);
        // pre-defined
        v.extend([0u8; 3 * 4]);

        // Width
        let width =
            u16::try_from(s.get::<i32>("width").context("no width")?).context("too big width")?;
        v.extend(width.to_be_bytes());

        // Height
        let height = u16::try_from(s.get::<i32>("height").context("no height")?)
            .context("too big height")?;
        v.extend(height.to_be_bytes());

        // Horizontal resolution
        v.extend(0x00480000u32.to_be_bytes());

        // Vertical resolution
        v.extend(0x00480000u32.to_be_bytes());

        // Reserved
        v.extend([0u8; 4]);

        // Frame count
        v.extend(1u16.to_be_bytes());

        // Compressor name
        v.extend([0u8; 32]);

        // Depth
        v.extend(0x0018u16.to_be_bytes());

        // Pre-defined
        v.extend((-1i16).to_be_bytes());

        // Codec specific boxes
        match s.name().as_str() {
            "video/x-h264" => {
                let codec_data = s
                    .get::<&gst::BufferRef>("codec_data")
                    .context("no codec_data")?;
                let map = codec_data
                    .map_readable()
                    .context("codec_data not mappable")?;
                write_box(v, b"avcC", move |v| {
                    v.extend_from_slice(&map);
                    Ok(())
                })?;
            }
            "video/x-h265" => {
                let codec_data = s
                    .get::<&gst::BufferRef>("codec_data")
                    .context("no codec_data")?;
                let map = codec_data
                    .map_readable()
                    .context("codec_data not mappable")?;
                write_box(v, b"hvcC", move |v| {
                    v.extend_from_slice(&map);
                    Ok(())
                })?;
            }
            "video/x-vp9" => {
                let profile: u8 = match s.get::<&str>("profile").expect("no vp9 profile") {
                    "0" => Some(0),
                    "1" => Some(1),
                    "2" => Some(2),
                    "3" => Some(3),
                    _ => None,
                }
                .context("unsupported vp9 profile")?;
                let colorimetry = gst_video::VideoColorimetry::from_str(
                    s.get::<&str>("colorimetry").expect("no colorimetry"),
                )
                .context("failed to parse colorimetry")?;
                let video_full_range =
                    colorimetry.range() == gst_video::VideoColorRange::Range0_255;
                let chroma_format: u8 =
                    match s.get::<&str>("chroma-format").expect("no chroma-format") {
                        "4:2:0" =>
                        // chroma-site is optional
                        {
                            match s
                                .get::<&str>("chroma-site")
                                .ok()
                                .and_then(|cs| gst_video::VideoChromaSite::from_str(cs).ok())
                            {
                                Some(gst_video::VideoChromaSite::V_COSITED) => Some(0),
                                // COSITED
                                _ => Some(1),
                            }
                        }
                        "4:2:2" => Some(2),
                        "4:4:4" => Some(3),
                        _ => None,
                    }
                    .context("unsupported chroma-format")?;
                let bit_depth: u8 = {
                    let bit_depth_luma = s.get::<u32>("bit-depth-luma").expect("no bit-depth-luma");
                    let bit_depth_chroma = s
                        .get::<u32>("bit-depth-chroma")
                        .expect("no bit-depth-chroma");
                    if bit_depth_luma != bit_depth_chroma {
                        return Err(anyhow!("bit-depth-luma and bit-depth-chroma have different values which is an unsupported configuration"));
                    }
                    bit_depth_luma as u8
                };
                write_full_box(v, b"vpcC", 1, 0, move |v| {
                    v.push(profile);
                    // XXX: hardcoded level 1
                    v.push(10);
                    let mut byte: u8 = 0;
                    byte |= (bit_depth & 0xF) << 4;
                    byte |= (chroma_format & 0x7) << 1;
                    byte |= video_full_range as u8;
                    v.push(byte);
                    v.push(colorimetry.primaries().to_iso() as u8);
                    v.push(colorimetry.transfer().to_iso() as u8);
                    v.push(colorimetry.matrix().to_iso() as u8);
                    // 16-bit length field for codec initialization, unused
                    v.push(0);
                    v.push(0);
                    Ok(())
                })?;
            }
            "video/x-av1" => {
                write_box(v, b"av1C", move |v| {
                    if let Ok(codec_data) = s.get::<&gst::BufferRef>("codec_data") {
                        let map = codec_data
                            .map_readable()
                            .context("codec_data not mappable")?;

                        v.extend_from_slice(&map);
                    } else {
                        let presentation_delay_minus_one =
                            if let Ok(presentation_delay) = s.get::<i32>("presentation-delay") {
                                Some(
                                    (1u8 << 5)
                                        | std::cmp::max(
                                            0xF,
                                            (presentation_delay.saturating_sub(1) & 0xF) as u8,
                                        ),
                                )
                            } else {
                                None
                            };

                        let profile = match s.get::<&str>("profile").unwrap() {
                            "main" => 0,
                            "high" => 1,
                            "professional" => 2,
                            _ => unreachable!(),
                        };

                        // TODO: Use `gst_codec_utils_av1_get_seq_level_idx` when exposed in bindings
                        let level = av1_seq_level_idx(s.get::<&str>("level").ok());
                        let tier = av1_tier(s.get::<&str>("tier").ok());
                        let (high_bitdepth, twelve_bit) =
                            match s.get::<u32>("bit-depth-luma").unwrap() {
                                8 => (false, false),
                                10 => (true, false),
                                12 => (true, true),
                                _ => unreachable!(),
                            };
                        let (monochrome, chroma_sub_x, chroma_sub_y) =
                            match s.get::<&str>("chroma-format").unwrap() {
                                "4:0:0" => (true, true, true),
                                "4:2:0" => (false, true, true),
                                "4:2:2" => (false, true, false),
                                "4:4:4" => (false, false, false),
                                _ => unreachable!(),
                            };

                        let chrome_sample_position = match s.get::<&str>("chroma-site") {
                            Ok("v-cosited") => 1,
                            Ok("v-cosited+h-cosited") => 2,
                            _ => 0,
                        };

                        let codec_data = [
                            0x80 | 0x01,            // marker | version
                            (profile << 5) | level, // profile | level
                            (tier << 7)
                                | ((high_bitdepth as u8) << 6)
                                | ((twelve_bit as u8) << 5)
                                | ((monochrome as u8) << 4)
                                | ((chroma_sub_x as u8) << 3)
                                | ((chroma_sub_y as u8) << 2)
                                | chrome_sample_position, // tier | high bitdepth | twelve bit | monochrome | chroma sub x |
                            // chroma sub y | chroma sample position
                            if let Some(presentation_delay_minus_one) = presentation_delay_minus_one
                            {
                                0x10 | presentation_delay_minus_one // reserved | presentation delay present | presentation delay
                            } else {
                                0
                            },
                        ];

                        v.extend_from_slice(&codec_data);
                    }

                    if let Some(extra_data) = &stream.extra_header_data {
                        // unsigned int(8) configOBUs[];
                        v.extend_from_slice(extra_data.as_slice());
                    }
                    Ok(())
                })?;
            }
            "video/x-vp8" | "image/jpeg" => {
                // Nothing to do here
            }
            "video/x-raw" => {
                let video_info = gst_video::VideoInfo::from_caps(&stream.caps).unwrap();
                write_uncompressed_sample_entries(v, video_info)?
            }
            _ => unreachable!(),
        }

        if let Ok(par) = s.get::<gst::Fraction>("pixel-aspect-ratio") {
            write_box(v, b"pasp", move |v| {
                v.extend((par.numer() as u32).to_be_bytes());
                v.extend((par.denom() as u32).to_be_bytes());
                Ok(())
            })?;
        }

        if let Some(colorimetry) = s
            .get::<&str>("colorimetry")
            .ok()
            .and_then(|c| c.parse::<gst_video::VideoColorimetry>().ok())
        {
            write_box(v, b"colr", move |v| {
                v.extend(b"nclx");
                let (primaries, transfer, matrix) = {
                    (
                        (colorimetry.primaries().to_iso() as u16),
                        (colorimetry.transfer().to_iso() as u16),
                        (colorimetry.matrix().to_iso() as u16),
                    )
                };

                let full_range = match colorimetry.range() {
                    gst_video::VideoColorRange::Range0_255 => 0x80u8,
                    gst_video::VideoColorRange::Range16_235 => 0x00u8,
                    _ => 0x00,
                };

                v.extend(primaries.to_be_bytes());
                v.extend(transfer.to_be_bytes());
                v.extend(matrix.to_be_bytes());
                v.push(full_range);

                Ok(())
            })?;
        }

        if let Ok(cll) = gst_video::VideoContentLightLevel::from_caps(&stream.caps) {
            write_box(v, b"clli", move |v| {
                v.extend((cll.max_content_light_level()).to_be_bytes());
                v.extend((cll.max_frame_average_light_level()).to_be_bytes());
                Ok(())
            })?;
        }

        if let Ok(mastering) = gst_video::VideoMasteringDisplayInfo::from_caps(&stream.caps) {
            write_box(v, b"mdcv", move |v| {
                for primary in mastering.display_primaries() {
                    v.extend(primary.x.to_be_bytes());
                    v.extend(primary.y.to_be_bytes());
                }
                v.extend(mastering.white_point().x.to_be_bytes());
                v.extend(mastering.white_point().y.to_be_bytes());
                v.extend(mastering.max_display_mastering_luminance().to_be_bytes());
                v.extend(mastering.max_display_mastering_luminance().to_be_bytes());
                Ok(())
            })?;
        }

        // Write fiel box for codecs that require it
        if ["image/jpeg"].contains(&s.name().as_str()) {
            let interlace_mode = s
                .get::<&str>("interlace-mode")
                .ok()
                .map(gst_video::VideoInterlaceMode::from_string)
                .unwrap_or(gst_video::VideoInterlaceMode::Progressive);
            let field_order = s
                .get::<&str>("field-order")
                .ok()
                .map(gst_video::VideoFieldOrder::from_string)
                .unwrap_or(gst_video::VideoFieldOrder::Unknown);

            write_box(v, b"fiel", move |v| {
                let (interlace, field_order) = match interlace_mode {
                    gst_video::VideoInterlaceMode::Progressive => (1, 0),
                    gst_video::VideoInterlaceMode::Interleaved
                        if field_order == gst_video::VideoFieldOrder::TopFieldFirst =>
                    {
                        (2, 9)
                    }
                    gst_video::VideoInterlaceMode::Interleaved => (2, 14),
                    _ => (0, 0),
                };

                v.push(interlace);
                v.push(field_order);
                Ok(())
            })?;
        }

        if stream.image_sequence {
            match s.name().as_str() {
                // intra formats
                "video/x-vp9" | "video/x-vp8" | "image/jpeg" => {
                    let all_ref_pics_intra = 1u32; // 0 = don't know, 1 = reference pictures are only intra
                    let intra_pred_used = 1u32; // 0 = no, 1 = yes, or maybe
                    let max_ref_per_pic = 0u32; // none number
                    let packed_bits = (all_ref_pics_intra << 31)
                        | (intra_pred_used << 30)
                        | (max_ref_per_pic << 26);
                    write_full_box(v, b"ccst", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
                        v.extend(packed_bits.to_be_bytes());
                        Ok(())
                    })?;
                }
                // uncompressed
                "video/x-raw" => {
                    let all_ref_pics_intra = 1u32; // 0 = don't know, 1 = reference pictures are only intra
                    let intra_pred_used = 0u32; // 0 = no, 1 = yes, or maybe
                    let max_ref_per_pic = 0u32; // none
                    let packed_bits = (all_ref_pics_intra << 31)
                        | (intra_pred_used << 30)
                        | (max_ref_per_pic << 26);
                    write_full_box(v, b"ccst", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
                        v.extend(packed_bits.to_be_bytes());
                        Ok(())
                    })?;
                }
                _ => {
                    let all_ref_pics_intra = 0u32; // 0 = don't know, 1 = reference pictures are only intra
                    let intra_pred_used = 1u32; // 0 = no, 1 = yes, or maybe
                    let max_ref_per_pic = 15u32; // any number
                    let packed_bits = (all_ref_pics_intra << 31)
                        | (intra_pred_used << 30)
                        | (max_ref_per_pic << 26);
                    write_full_box(v, b"ccst", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
                        v.extend(packed_bits.to_be_bytes());
                        Ok(())
                    })?;
                }
            }
        }

        // TODO: write btrt bitrate box based on tags

        Ok(())
    })?;

    Ok(())
}

fn write_uncompressed_sample_entries(
    v: &mut Vec<u8>,
    video_info: gst_video::VideoInfo,
) -> Result<(), Error> {
    let profile = *get_profile_for_uncc_format(&video_info);
    if matches!(
        video_info.format(),
        VideoFormat::Rgba | VideoFormat::Abgr | VideoFormat::Rgb
    ) && (get_row_align_size_for_uncc_format(&video_info) == 0)
    {
        write_full_box(v, b"uncC", 1, 0, move |v| {
            v.extend(profile);
            Ok(())
        })?;
    } else {
        let component_types = get_components_for_uncc_format(&video_info);
        let num_components = component_types.len() as u32;
        let mut uncc_component_bytes: Vec<u8> = Vec::new();
        for i in 0..num_components {
            uncc_component_bytes.extend((i as u16).to_be_bytes());
            let component = component_types[i as usize];
            uncc_component_bytes
                .extend(get_bit_depth_for_uncc_format(&video_info, component).to_be_bytes());
            uncc_component_bytes.extend((0_u8).to_be_bytes()); // component_format
            uncc_component_bytes.extend((0_u8).to_be_bytes()); // component_align_size
        }
        write_box(v, b"cmpd", move |v| {
            v.extend(num_components.to_be_bytes());
            for c in component_types {
                v.extend(c.to_be_bytes());
            }
            Ok(())
        })?;
        write_full_box(v, b"uncC", 0, 0, move |v| {
            v.extend(profile);
            v.extend(num_components.to_be_bytes());
            v.extend(uncc_component_bytes);
            let sampling_type = get_sampling_type_for_uncc_format(&video_info);
            gst::debug!(CAT_23001, "sampling_type: {sampling_type:?}");
            v.extend(sampling_type.to_be_bytes());
            let interleave_type = get_interleave_type_for_uncc_format(&video_info);
            gst::debug!(CAT_23001, "interleave_type: {interleave_type:?}");
            v.extend(interleave_type.to_be_bytes());
            v.extend(get_block_size_for_uncc_format(&video_info).to_be_bytes());
            v.extend(get_flag_bits_for_uncc_format(&video_info).to_be_bytes());
            v.extend(get_pixel_size_for_uncc_format(&video_info).to_be_bytes());
            let row_align_size = get_row_align_size_for_uncc_format(&video_info);
            gst::debug!(CAT_23001, "row_align_size: {row_align_size:?}");
            v.extend(row_align_size.to_be_bytes());
            v.extend((0_u32).to_be_bytes()); // tile align size
            v.extend((0_u32).to_be_bytes()); // num tile columns minus 1
            v.extend((0_u32).to_be_bytes()); // num tile rows minus 1
            Ok(())
        })?;
    }
    Ok(())
}

// See ISO/IEC 23001-17:2024 Table 1
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComponentType {
    Monochrome,
    Luma,
    Cb,
    Cr,
    Red,
    Green,
    Blue,
    Alpha,
    // There are more here, but we don't support them yet
}

impl ComponentType {
    fn to_be_bytes(self) -> [u8; 2] {
        match self {
            Self::Monochrome => 0u16.to_be_bytes(),
            Self::Luma => 1u16.to_be_bytes(),
            Self::Cb => 2u16.to_be_bytes(),
            Self::Cr => 3u16.to_be_bytes(),
            Self::Red => 4u16.to_be_bytes(),
            Self::Green => 5u16.to_be_bytes(),
            Self::Blue => 6u16.to_be_bytes(),
            Self::Alpha => 7u16.to_be_bytes(),
        }
    }
}

fn component_index_to_component_type(
    format_info: gst_video::VideoFormatInfo,
    index: u32,
) -> ComponentType {
    // See https://gstreamer.freedesktop.org/documentation/video/video-format.html?gi-language=c#GstVideoFormatFlags
    if format_info.is_rgb() {
        gst::debug!(CAT_23001, "format for {}: rgb", format_info.name());
        return match index {
            0 => ComponentType::Red,
            1 => ComponentType::Green,
            2 => ComponentType::Blue,
            3 => ComponentType::Alpha,
            _ => unreachable!(),
        };
    }
    if format_info.is_yuv() {
        gst::debug!(CAT_23001, "format for {}: yuv", format_info.name());
        return match index {
            0 => ComponentType::Luma,
            1 => ComponentType::Cb,
            2 => ComponentType::Cr,
            3 => ComponentType::Alpha,
            _ => unreachable!(),
        };
    }
    if format_info.is_gray() {
        gst::debug!(CAT_23001, "format for {}: gray", format_info.name());
        return match index {
            0 => ComponentType::Monochrome,
            _ => unreachable!(),
        };
    }
    unreachable!("format for {}: unknown", format_info.name())
}

fn component_type_to_component_index(
    format_info: gst_video::VideoFormatInfo,
    component_type: ComponentType,
) -> u8 {
    // See https://gstreamer.freedesktop.org/documentation/video/video-format.html?gi-language=c#GstVideoFormatFlags
    if format_info.is_rgb() {
        gst::debug!(CAT_23001, "format for {}: rgb", format_info.name());
        return match component_type {
            ComponentType::Red => 0,
            ComponentType::Green => 1,
            ComponentType::Blue => 2,
            ComponentType::Alpha => 3,
            _ => unreachable!(),
        };
    }
    if format_info.is_yuv() {
        gst::debug!(CAT_23001, "format for {}: yuv", format_info.name());
        return match component_type {
            ComponentType::Luma => 0,
            ComponentType::Cb => 1,
            ComponentType::Cr => 2,
            ComponentType::Alpha => 3,
            _ => unreachable!(),
        };
    }
    if format_info.is_gray() {
        gst::debug!(CAT_23001, "format for {}: gray", format_info.name());
        return match component_type {
            ComponentType::Monochrome => 0,
            _ => unreachable!(),
        };
    }
    unreachable!("format for {}: unknown", format_info.name())
}

fn get_components_for_uncc_format(video_info: &gst_video::VideoInfo) -> Vec<ComponentType> {
    let format_info = video_info.format_info();
    let mut component_offsets: BTreeMap<u32, ComponentType> = BTreeMap::new();
    let interleave_type = get_interleave_type_for_uncc_format(video_info);
    if interleave_type == 5 {
        // TODO: it would be nice to pull this out of the info, but too much for now
        return match video_info.format() {
            VideoFormat::Yuy2 => [
                ComponentType::Luma,
                ComponentType::Cb,
                ComponentType::Luma,
                ComponentType::Cr,
            ]
            .to_vec(),
            VideoFormat::Yvyu => [
                ComponentType::Luma,
                ComponentType::Cr,
                ComponentType::Luma,
                ComponentType::Cb,
            ]
            .to_vec(),
            VideoFormat::Uyvy => [
                ComponentType::Cb,
                ComponentType::Luma,
                ComponentType::Cr,
                ComponentType::Luma,
            ]
            .to_vec(),
            VideoFormat::Vyuy => [
                ComponentType::Cr,
                ComponentType::Luma,
                ComponentType::Cb,
                ComponentType::Luma,
            ]
            .to_vec(),
            _ => unreachable!(),
        };
    }
    if interleave_type == 0 {
        for comp_index in 0..video_info.n_components() {
            let comp_offset = video_info.comp_offset(comp_index as u8);
            gst::debug!(
                CAT_23001,
                "planar comp_offsets for {}, index {comp_index:?}, offset: {comp_offset:?}",
                format_info.name()
            );
            let component_type = component_index_to_component_type(format_info, comp_index);
            gst::debug!(
                CAT_23001,
                "planar component_type for {}, index {comp_index:?}: {component_type:?}",
                format_info.name()
            );
            component_offsets.insert(comp_offset as u32, component_type);
        }
        let components = component_offsets.into_values().collect();
        gst::debug!(
            CAT_23001,
            "planar components for video format {}: {components:?}",
            format_info.name()
        );
        return components;
    }
    if interleave_type == 2 {
        // mixed interleave, like NV12 or NV21
        // Assume Y channel (which comp_poffset does not include) is first
        component_offsets.insert(0, ComponentType::Luma);
    }
    let components = match video_info.format() {
        VideoFormat::R210 => [
            ComponentType::Red,
            ComponentType::Green,
            ComponentType::Blue,
        ]
        .to_vec(),
        _ => {
            for comp_index in 0..video_info.n_components() {
                let offset = video_info.comp_poffset(comp_index as u8);
                let component_type = component_index_to_component_type(format_info, comp_index);
                gst::debug!(
                    CAT_23001,
                    "component_type for {}, index {comp_index:?}: {component_type:?}",
                    format_info.name()
                );
                // plus 1 ensures that there is room for the Y value if pushed above
                component_offsets.insert(offset + 1, component_type);
            }
            component_offsets.into_values().collect()
        }
    };
    gst::debug!(
        CAT_23001,
        "components for video format {}: {components:?}",
        format_info.name()
    );
    components
}

fn get_profile_for_uncc_format(video_info: &gst_video::VideoInfo) -> &[u8; 4] {
    // See ISO/IEC 23001-17:2024 Table 5
    // Appears that no VideoFormat value matches "yuv1", "v408", "v410" or "yv22"
    match video_info.format() {
        VideoFormat::Uyvy => b"2vuy",
        VideoFormat::Yuy2 => b"yuv2",
        VideoFormat::Yvyu => b"yvyu",
        VideoFormat::Vyuy => b"vyuy",
        VideoFormat::V308 => b"v308",
        VideoFormat::Y210 => b"y210",
        VideoFormat::V210 => b"v210",
        VideoFormat::Rgb => b"rgb3",
        VideoFormat::I420 => b"i420",
        VideoFormat::Nv12 => b"nv12",
        VideoFormat::Nv21 => b"nv21",
        VideoFormat::Rgba => b"rgba",
        VideoFormat::Abgr => b"abgr",
        VideoFormat::Y42b => b"yu22",
        VideoFormat::Yv12 => b"yv20",
        _ => &[0u8, 0u8, 0u8, 0u8],
    }
}

fn get_bit_depth_for_uncc_format(
    video_info: &gst_video::VideoInfo,
    component_type: ComponentType,
) -> u8 {
    let component_index =
        component_type_to_component_index(video_info.format_info(), component_type);
    let component_bit_depth_minus_one = video_info.comp_depth(component_index) - 1;
    gst::debug!(CAT_23001, "component_bit_depth_minus_one for video format {} index {component_index:?}: {component_bit_depth_minus_one:?}", video_info.format_info().name());
    component_bit_depth_minus_one.try_into().unwrap()
}

fn get_sampling_type_for_uncc_format(video_info: &gst_video::VideoInfo) -> u8 {
    let format_info = video_info.format_info();
    let mut horiz_subsampling = 0;
    let mut vert_subsampling = 0;
    for i in 0..video_info.n_components() {
        if format_info.w_sub()[i as usize] != 0 {
            horiz_subsampling = format_info.w_sub()[i as usize];
        }
        if format_info.h_sub()[i as usize] != 0 {
            vert_subsampling = format_info.h_sub()[i as usize];
        }
    }
    if horiz_subsampling == 0 {
        // No subsampling - 4:4:4 or similar
        return 0;
    }
    if horiz_subsampling == 1 {
        if vert_subsampling == 0 {
            // 4:2:2 or similar
            return 1;
        } else if vert_subsampling == 1 {
            // 4:2:0 or similar
            if video_info.height() % 2 != 0 {
                unreachable!("4:2:0 images must have an even number of rows in 23001-17, should have failed caps negotiation");
            }
            return 2;
        } else {
            unreachable!("Unsupported vertical subsampling");
        }
    }
    if horiz_subsampling == 2 {
        // 4:1:1
        return 3;
    }
    unreachable!("unsupported horizontal subsampling");
}

pub(crate) fn get_interleave_type_for_uncc_format(video_info: &gst_video::VideoInfo) -> u8 {
    let n_components = video_info.n_components();
    let n_planes = video_info.n_planes();
    match video_info.format() {
        VideoFormat::Nv12
        | VideoFormat::Nv21
        | VideoFormat::Nv16
        | VideoFormat::Nv61
        | VideoFormat::Nv24
        | VideoFormat::Nv1264z32
        | VideoFormat::P01010be
        | VideoFormat::P01010le
        | VideoFormat::Nv1210le32
        | VideoFormat::Nv1610le32
        | VideoFormat::Nv1210le40
        | VideoFormat::P016Be
        | VideoFormat::P016Le
        | VideoFormat::P012Be
        | VideoFormat::P012Le
        | VideoFormat::Nv124l4
        | VideoFormat::Nv1232l32
        | VideoFormat::Av12 => 2,
        VideoFormat::Yuy2 | VideoFormat::Yvyu | VideoFormat::Uyvy | VideoFormat::Vyuy => 5,
        _ => {
            if (n_components == 1) || (n_components == n_planes) {
                // component interleave
                0
            } else if n_planes == 1 {
                // pixel interleave
                1
            } else {
                unreachable!()
            }
        }
    }
}

fn get_block_size_for_uncc_format(video_info: &gst_video::VideoInfo) -> u8 {
    match video_info.format() {
        VideoFormat::Iyu2
        | VideoFormat::Rgb
        | VideoFormat::Bgr
        | VideoFormat::Rgba
        | VideoFormat::Argb
        | VideoFormat::Bgra
        | VideoFormat::Abgr
        | VideoFormat::Rgbx
        | VideoFormat::Bgrx
        | VideoFormat::Nv12
        | VideoFormat::Nv21
        | VideoFormat::Y444
        | VideoFormat::I420
        | VideoFormat::Yv12
        | VideoFormat::Yuy2
        | VideoFormat::Yvyu
        | VideoFormat::Uyvy
        | VideoFormat::Vyuy
        | VideoFormat::Ayuv
        | VideoFormat::Y41b
        | VideoFormat::Y42b
        | VideoFormat::V308
        | VideoFormat::Gray8
        | VideoFormat::Gray16Be
        | VideoFormat::Nv16
        | VideoFormat::Nv61
        | VideoFormat::Gbr
        | VideoFormat::Bgrp
        | VideoFormat::Rgbp => 0,
        VideoFormat::R210 => 4,
        _ => unreachable!(),
    }
}

fn get_flag_bits_for_uncc_format(video_info: &gst_video::VideoInfo) -> u8 {
    match video_info.format() {
        VideoFormat::Iyu2
        | VideoFormat::Rgb
        | VideoFormat::Bgr
        | VideoFormat::Rgba
        | VideoFormat::Argb
        | VideoFormat::Bgra
        | VideoFormat::Abgr
        | VideoFormat::Rgbx
        | VideoFormat::Bgrx
        | VideoFormat::Nv12
        | VideoFormat::Nv21
        | VideoFormat::Y444
        | VideoFormat::I420
        | VideoFormat::Yv12
        | VideoFormat::Yuy2
        | VideoFormat::Yvyu
        | VideoFormat::Uyvy
        | VideoFormat::Vyuy
        | VideoFormat::Ayuv
        | VideoFormat::Y41b
        | VideoFormat::Y42b
        | VideoFormat::V308
        | VideoFormat::Gray8
        | VideoFormat::Gray16Be
        | VideoFormat::R210
        | VideoFormat::Nv16
        | VideoFormat::Nv61
        | VideoFormat::Gbr
        | VideoFormat::Bgrp
        | VideoFormat::Rgbp => 0,
        _ => unreachable!(),
    }
}

fn get_pixel_size_for_uncc_format(video_info: &gst_video::VideoInfo) -> u32 {
    let interleave = get_interleave_type_for_uncc_format(video_info);
    if (interleave != 1) && (interleave != 5) {
        // See ISO/IEC 23001-17:2024 Section 5.2.1.7
        return 0;
    }
    if interleave == 1 {
        // Pixel interleave
        // Assume all components use the same pixel stride
        return video_info.comp_pstride(0u8) as u32;
    }
    if interleave == 5 {
        // Multi-Y
        let pixel_size = match video_info.format() {
            VideoFormat::Yuy2 | VideoFormat::Yvyu | VideoFormat::Uyvy | VideoFormat::Vyuy => 4,
            _ => unreachable!(),
        };
        gst::debug!(
            CAT_23001,
            "Multi-Y stride for video format {}: {pixel_size:?}",
            video_info.format_info().name()
        );
        return pixel_size;
    }
    unreachable!()
}

fn get_row_align_size_for_uncc_format(video_info: &gst_video::VideoInfo) -> u32 {
    if ((get_interleave_type_for_uncc_format(video_info) == 0)
        || (get_interleave_type_for_uncc_format(video_info) == 5))
        && (get_sampling_type_for_uncc_format(video_info) != 0)
    {
        // ISO/IEC 23001-17 5.2.1.5 requires alignment to be be done with halved row alignment for 4:2:2, 4:2:0, 4:1:1
        // GStreamer uses the same row stride for subsampling (not halved). So we don't support that in general.
        if video_info.width() % 4 == 0 {
            // However we can handle it if everything is a multiple of 4, which requires no alignment.
            return 0;
        } else {
            unreachable!("23001-17 Sub-sampled images must have image width that is a multiple of 4, should have failed caps negotiation");
        }
    }
    let first_plane_stride = video_info.stride()[0] as u32;
    if (first_plane_stride == video_info.width() * video_info.n_components())
        && (video_info.comp_depth(0) == 8)
    {
        // Then there is no padding at the end of the row
        return 0;
    }
    first_plane_stride
}

fn av1_seq_level_idx(level: Option<&str>) -> u8 {
    match level {
        Some("2.0") => 0,
        Some("2.1") => 1,
        Some("2.2") => 2,
        Some("2.3") => 3,
        Some("3.0") => 4,
        Some("3.1") => 5,
        Some("3.2") => 6,
        Some("3.3") => 7,
        Some("4.0") => 8,
        Some("4.1") => 9,
        Some("4.2") => 10,
        Some("4.3") => 11,
        Some("5.0") => 12,
        Some("5.1") => 13,
        Some("5.2") => 14,
        Some("5.3") => 15,
        Some("6.0") => 16,
        Some("6.1") => 17,
        Some("6.2") => 18,
        Some("6.3") => 19,
        Some("7.0") => 20,
        Some("7.1") => 21,
        Some("7.2") => 22,
        Some("7.3") => 23,
        _ => 1,
    }
}

fn av1_tier(tier: Option<&str>) -> u8 {
    match tier {
        Some("main") => 0,
        Some("high") => 1,
        _ => 0,
    }
}

fn write_audio_sample_entry(
    v: &mut Vec<u8>,
    _header: &super::Header,
    stream: &super::Stream,
) -> Result<(), Error> {
    let s = stream.caps.structure(0).unwrap();
    let fourcc = match s.name().as_str() {
        "audio/mpeg" => b"mp4a",
        "audio/x-opus" => b"Opus",
        "audio/x-flac" => b"fLaC",
        "audio/x-alaw" => b"alaw",
        "audio/x-mulaw" => b"ulaw",
        "audio/x-adpcm" => {
            let layout = s.get::<&str>("layout").context("no ADPCM layout field")?;

            match layout {
                "g726" => b"ms\x00\x45",
                _ => unreachable!(),
            }
        }
        _ => unreachable!(),
    };

    let sample_size = match s.name().as_str() {
        "audio/x-adpcm" => {
            let bitrate = s.get::<i32>("bitrate").context("no ADPCM bitrate field")?;
            (bitrate / 8000) as u16
        }
        "audio/x-flac" => with_flac_metadata(&stream.caps, |streaminfo, _| {
            1 + ((u16::from_be_bytes([streaminfo[16], streaminfo[17]]) >> 4) & 0b11111)
        })
        .context("FLAC metadata error")?,
        _ => 16u16,
    };

    write_sample_entry_box(v, fourcc, move |v| {
        // Reserved
        v.extend([0u8; 2 * 4]);

        // Channel count
        let channels = u16::try_from(s.get::<i32>("channels").context("no channels")?)
            .context("too many channels")?;
        v.extend(channels.to_be_bytes());

        // Sample size
        v.extend(sample_size.to_be_bytes());

        // Pre-defined
        v.extend([0u8; 2]);

        // Reserved
        v.extend([0u8; 2]);

        // Sample rate
        let rate = u16::try_from(s.get::<i32>("rate").context("no rate")?).unwrap_or(0);
        v.extend((u32::from(rate) << 16).to_be_bytes());

        // Codec specific boxes
        match s.name().as_str() {
            "audio/mpeg" => {
                let codec_data = s
                    .get::<&gst::BufferRef>("codec_data")
                    .context("no codec_data")?;
                let map = codec_data
                    .map_readable()
                    .context("codec_data not mappable")?;
                if map.len() < 2 {
                    bail!("too small codec_data");
                }
                write_esds_aac(v, &map)?;
            }
            "audio/x-opus" => {
                write_dops(v, &stream.caps)?;
            }
            "audio/x-flac" => {
                write_dfla(v, &stream.caps)?;
            }
            "audio/x-alaw" | "audio/x-mulaw" | "audio/x-adpcm" => {
                // Nothing to do here
            }
            _ => unreachable!(),
        }

        // If rate did not fit into 16 bits write a full `srat` box
        if rate == 0 {
            let rate = s.get::<i32>("rate").context("no rate")?;
            // FIXME: This is defined as full box?
            write_full_box(
                v,
                b"srat",
                FULL_BOX_VERSION_0,
                FULL_BOX_FLAGS_NONE,
                move |v| {
                    v.extend((rate as u32).to_be_bytes());
                    Ok(())
                },
            )?;
        }

        // TODO: write btrt bitrate box based on tags

        // TODO: chnl box for channel ordering? probably not needed for AAC

        Ok(())
    })?;

    Ok(())
}

fn write_esds_aac(v: &mut Vec<u8>, codec_data: &[u8]) -> Result<(), Error> {
    let calculate_len = |mut len| {
        if len > 260144641 {
            bail!("too big descriptor length");
        }

        if len == 0 {
            return Ok(([0; 4], 1));
        }

        let mut idx = 0;
        let mut lens = [0u8; 4];
        while len > 0 {
            lens[idx] = ((if len > 0x7f { 0x80 } else { 0x00 }) | (len & 0x7f)) as u8;
            idx += 1;
            len >>= 7;
        }

        Ok((lens, idx))
    };

    write_full_box(
        v,
        b"esds",
        FULL_BOX_VERSION_0,
        FULL_BOX_FLAGS_NONE,
        move |v| {
            // Calculate all lengths bottom up

            // Decoder specific info
            let decoder_specific_info_len = calculate_len(codec_data.len())?;

            // Decoder config
            let decoder_config_len =
                calculate_len(13 + 1 + decoder_specific_info_len.1 + codec_data.len())?;

            // SL config
            let sl_config_len = calculate_len(1)?;

            // ES descriptor
            let es_descriptor_len = calculate_len(
                3 + 1
                    + decoder_config_len.1
                    + 13
                    + 1
                    + decoder_specific_info_len.1
                    + codec_data.len()
                    + 1
                    + sl_config_len.1
                    + 1,
            )?;

            // ES descriptor tag
            v.push(0x03);

            // Length
            v.extend_from_slice(&es_descriptor_len.0[..(es_descriptor_len.1)]);

            // Track ID
            v.extend(1u16.to_be_bytes());
            // Flags
            v.push(0u8);

            // Decoder config descriptor
            v.push(0x04);

            // Length
            v.extend_from_slice(&decoder_config_len.0[..(decoder_config_len.1)]);

            // Object type ESDS_OBJECT_TYPE_MPEG4_P3
            v.push(0x40);
            // Stream type ESDS_STREAM_TYPE_AUDIO
            v.push((0x05 << 2) | 0x01);

            // Buffer size db?
            v.extend([0u8; 3]);

            // Max bitrate
            v.extend(0u32.to_be_bytes());

            // Avg bitrate
            v.extend(0u32.to_be_bytes());

            // Decoder specific info
            v.push(0x05);

            // Length
            v.extend_from_slice(&decoder_specific_info_len.0[..(decoder_specific_info_len.1)]);
            v.extend_from_slice(codec_data);

            // SL config descriptor
            v.push(0x06);

            // Length: 1 (tag) + 1 (length) + 1 (predefined)
            v.extend_from_slice(&sl_config_len.0[..(sl_config_len.1)]);

            // Predefined
            v.push(0x02);
            Ok(())
        },
    )
}

fn write_dops(v: &mut Vec<u8>, caps: &gst::Caps) -> Result<(), Error> {
    let rate;
    let channels;
    let channel_mapping_family;
    let stream_count;
    let coupled_count;
    let pre_skip;
    let output_gain;
    let mut channel_mapping = [0; 256];

    // TODO: Use audio clipping meta to calculate pre_skip

    if let Some(header) = caps
        .structure(0)
        .unwrap()
        .get::<gst::ArrayRef>("streamheader")
        .ok()
        .and_then(|a| a.first().and_then(|v| v.get::<gst::Buffer>().ok()))
    {
        (
            rate,
            channels,
            channel_mapping_family,
            stream_count,
            coupled_count,
            pre_skip,
            output_gain,
        ) = gst_pbutils::codec_utils_opus_parse_header(&header, Some(&mut channel_mapping))
            .unwrap();
    } else {
        (
            rate,
            channels,
            channel_mapping_family,
            stream_count,
            coupled_count,
        ) = gst_pbutils::codec_utils_opus_parse_caps(caps, Some(&mut channel_mapping)).unwrap();
        output_gain = 0;
        pre_skip = 0;
    }

    write_box(v, b"dOps", move |v| {
        // Version number
        v.push(0);
        v.push(channels);
        v.extend(pre_skip.to_be_bytes());
        v.extend(rate.to_be_bytes());
        v.extend(output_gain.to_be_bytes());
        v.push(channel_mapping_family);
        if channel_mapping_family > 0 {
            v.push(stream_count);
            v.push(coupled_count);
            v.extend(&channel_mapping[..channels as usize]);
        }

        Ok(())
    })
}

fn with_flac_metadata<R>(
    caps: &gst::Caps,
    cb: impl FnOnce(&[u8], &[gst::glib::SendValue]) -> R,
) -> Result<R, Error> {
    let caps = caps.structure(0).unwrap();
    let header = caps.get::<gst::ArrayRef>("streamheader").unwrap();
    let (streaminfo, remainder) = header.as_ref().split_first().unwrap();
    let streaminfo = streaminfo.get::<&gst::BufferRef>().unwrap();
    let streaminfo = streaminfo.map_readable().unwrap();
    // 13 bytes for the Ogg/FLAC prefix and 38 for the streaminfo itself.
    match <&[_; 13 + 38]>::try_from(streaminfo.as_slice()) {
        Ok(i) if i.starts_with(b"\x7FFLAC\x01\x00") => Ok(cb(&i[13..], remainder)),
        Ok(_) | Err(_) => bail!("Unknown streamheader format"),
    }
}

fn write_dfla(v: &mut Vec<u8>, caps: &gst::Caps) -> Result<(), Error> {
    write_full_box(v, b"dfLa", 0, 0, move |v| {
        with_flac_metadata(caps, |streaminfo, remainder| {
            v.extend(streaminfo);
            for metadata in remainder {
                let metadata = metadata.get::<&gst::BufferRef>().unwrap();
                let metadata = metadata.map_readable().unwrap();
                v.extend(&metadata[..]);
            }
        })
    })
}

fn write_xml_meta_data_sample_entry(
    v: &mut Vec<u8>,
    _header: &super::Header,
    stream: &super::Stream,
) -> Result<(), Error> {
    let s = stream.caps.structure(0).unwrap();
    let namespace = match s.name().as_str() {
        "application/x-onvif-metadata" => b"http://www.onvif.org/ver10/schema",
        _ => unreachable!(),
    };

    write_sample_entry_box(v, b"metx", move |v| {
        // content_encoding, empty string
        v.push(0);

        // namespace
        v.extend_from_slice(namespace);
        v.push(0);

        // schema_location, empty string list
        v.push(0);

        Ok(())
    })?;

    Ok(())
}

fn write_stts(
    v: &mut Vec<u8>,
    _header: &super::Header,
    stream: &super::Stream,
) -> Result<(), Error> {
    let timescale = stream.timescale;

    let entry_count_position = v.len();
    // Entry count, rewritten in the end
    v.extend(0u32.to_be_bytes());

    let mut last_duration: Option<u32> = None;
    let mut sample_count = 0u32;
    let mut num_entries = 0u32;
    for duration in stream
        .chunks
        .iter()
        .flat_map(|c| c.samples.iter().map(|b| b.duration))
    {
        let duration = u32::try_from(
            duration
                .nseconds()
                .mul_div_round(timescale as u64, gst::ClockTime::SECOND.nseconds())
                .context("too big sample duration")?,
        )
        .context("too big sample duration")?;

        if last_duration != Some(duration) {
            if let Some(last_duration) = last_duration {
                v.extend(sample_count.to_be_bytes());
                v.extend(last_duration.to_be_bytes());
                num_entries += 1;
            }

            last_duration = Some(duration);
            sample_count = 1;
        } else {
            sample_count += 1;
        }
    }

    if let Some(last_duration) = last_duration {
        v.extend(sample_count.to_be_bytes());
        v.extend(last_duration.to_be_bytes());
        num_entries += 1;
    }

    // Rewrite entry count
    v[entry_count_position..][..4].copy_from_slice(&num_entries.to_be_bytes());

    Ok(())
}

fn write_ctts(
    v: &mut Vec<u8>,
    _header: &super::Header,
    stream: &super::Stream,
    version: u8,
) -> Result<(), Error> {
    let timescale = stream.timescale;

    let entry_count_position = v.len();
    // Entry count, rewritten in the end
    v.extend(0u32.to_be_bytes());

    let mut last_composition_time_offset = None;
    let mut sample_count = 0u32;
    let mut num_entries = 0u32;
    for composition_time_offset in stream
        .chunks
        .iter()
        .flat_map(|c| c.samples.iter().map(|b| b.composition_time_offset))
    {
        let composition_time_offset = composition_time_offset
            .expect("not all samples have a composition time offset")
            .mul_div_round(timescale as i64, gst::ClockTime::SECOND.nseconds() as i64)
            .context("too big sample composition time offset")?;

        if last_composition_time_offset != Some(composition_time_offset) {
            if let Some(last_composition_time_offset) = last_composition_time_offset {
                v.extend(sample_count.to_be_bytes());
                if version == FULL_BOX_VERSION_0 {
                    let last_composition_time_offset = u32::try_from(last_composition_time_offset)
                        .context("too big sample composition time offset")?;

                    v.extend(last_composition_time_offset.to_be_bytes());
                } else {
                    let last_composition_time_offset = i32::try_from(last_composition_time_offset)
                        .context("too big sample composition time offset")?;
                    v.extend(last_composition_time_offset.to_be_bytes());
                }
                num_entries += 1;
            }

            last_composition_time_offset = Some(composition_time_offset);
            sample_count = 1;
        } else {
            sample_count += 1;
        }
    }

    if let Some(last_composition_time_offset) = last_composition_time_offset {
        v.extend(sample_count.to_be_bytes());
        if version == FULL_BOX_VERSION_0 {
            let last_composition_time_offset = u32::try_from(last_composition_time_offset)
                .context("too big sample composition time offset")?;

            v.extend(last_composition_time_offset.to_be_bytes());
        } else {
            let last_composition_time_offset = i32::try_from(last_composition_time_offset)
                .context("too big sample composition time offset")?;
            v.extend(last_composition_time_offset.to_be_bytes());
        }
        num_entries += 1;
    }

    // Rewrite entry count
    v[entry_count_position..][..4].copy_from_slice(&num_entries.to_be_bytes());

    Ok(())
}

fn write_cslg(
    v: &mut Vec<u8>,
    _header: &super::Header,
    stream: &super::Stream,
) -> Result<(), Error> {
    let timescale = stream.timescale;

    let (min_ctts, max_ctts) = stream
        .chunks
        .iter()
        .flat_map(|c| {
            c.samples.iter().map(|b| {
                b.composition_time_offset
                    .expect("not all samples have a composition time offset")
            })
        })
        .fold((None, None), |(min, max), ctts| {
            (
                if min.map_or(true, |min| ctts < min) {
                    Some(ctts)
                } else {
                    min
                },
                if max.map_or(true, |max| ctts > max) {
                    Some(ctts)
                } else {
                    max
                },
            )
        });
    let min_ctts = min_ctts
        .unwrap()
        .mul_div_round(timescale as i64, gst::ClockTime::SECOND.nseconds() as i64)
        .context("too big composition time offset")?;
    let max_ctts = max_ctts
        .unwrap()
        .mul_div_round(timescale as i64, gst::ClockTime::SECOND.nseconds() as i64)
        .context("too big composition time offset")?;

    // Composition to DTS shift
    v.extend((-min_ctts).to_be_bytes());

    // least decode to display delta
    v.extend(min_ctts.to_be_bytes());

    // greatest decode to display delta
    v.extend(max_ctts.to_be_bytes());

    // composition start time
    let composition_start_time = stream
        .earliest_pts
        .nseconds()
        .mul_div_round(timescale as u64, gst::ClockTime::SECOND.nseconds())
        .context("too earliest PTS")?;
    v.extend(composition_start_time.to_be_bytes());

    // composition end time
    let composition_end_time = stream
        .end_pts
        .nseconds()
        .mul_div_round(timescale as u64, gst::ClockTime::SECOND.nseconds())
        .context("too end PTS")?;
    v.extend(composition_end_time.to_be_bytes());

    Ok(())
}

fn write_stss(
    v: &mut Vec<u8>,
    _header: &super::Header,
    stream: &super::Stream,
) -> Result<(), Error> {
    let entry_count_position = v.len();
    // Entry count, rewritten in the end
    v.extend(0u32.to_be_bytes());

    let mut num_entries = 0u32;
    for (idx, _sync_point) in stream
        .chunks
        .iter()
        .flat_map(|c| c.samples.iter().map(|b| b.sync_point))
        .enumerate()
        .filter(|(_idx, sync_point)| *sync_point)
    {
        v.extend((idx as u32 + 1).to_be_bytes());
        num_entries += 1;
    }

    // Rewrite entry count
    v[entry_count_position..][..4].copy_from_slice(&num_entries.to_be_bytes());

    Ok(())
}

fn write_stsz(
    v: &mut Vec<u8>,
    _header: &super::Header,
    stream: &super::Stream,
) -> Result<(), Error> {
    let first_sample_size = stream.chunks[0].samples[0].size;

    if stream
        .chunks
        .iter()
        .flat_map(|c| c.samples.iter().map(|b| b.size))
        .all(|size| size == first_sample_size)
    {
        // Sample size
        v.extend(first_sample_size.to_be_bytes());

        // Sample count
        let sample_count = stream
            .chunks
            .iter()
            .map(|c| c.samples.len() as u32)
            .sum::<u32>();
        v.extend(sample_count.to_be_bytes());
    } else {
        // Sample size
        v.extend(0u32.to_be_bytes());

        // Sample count, will be rewritten later
        let sample_count_position = v.len();
        let mut sample_count = 0u32;
        v.extend(0u32.to_be_bytes());

        for size in stream
            .chunks
            .iter()
            .flat_map(|c| c.samples.iter().map(|b| b.size))
        {
            v.extend(size.to_be_bytes());
            sample_count += 1;
        }

        v[sample_count_position..][..4].copy_from_slice(&sample_count.to_be_bytes());
    }

    Ok(())
}

fn write_stsc(
    v: &mut Vec<u8>,
    _header: &super::Header,
    stream: &super::Stream,
) -> Result<(), Error> {
    let entry_count_position = v.len();
    // Entry count, rewritten in the end
    v.extend(0u32.to_be_bytes());

    let mut num_entries = 0u32;
    let mut first_chunk = 1u32;
    let mut samples_per_chunk: Option<u32> = None;
    for (idx, chunk) in stream.chunks.iter().enumerate() {
        if samples_per_chunk != Some(chunk.samples.len() as u32) {
            if let Some(samples_per_chunk) = samples_per_chunk {
                v.extend(first_chunk.to_be_bytes());
                v.extend(samples_per_chunk.to_be_bytes());
                // sample description index
                v.extend(1u32.to_be_bytes());
                num_entries += 1;
            }
            samples_per_chunk = Some(chunk.samples.len() as u32);
            first_chunk = idx as u32 + 1;
        }
    }

    if let Some(samples_per_chunk) = samples_per_chunk {
        v.extend(first_chunk.to_be_bytes());
        v.extend(samples_per_chunk.to_be_bytes());
        // sample description index
        v.extend(1u32.to_be_bytes());
        num_entries += 1;
    }

    // Rewrite entry count
    v[entry_count_position..][..4].copy_from_slice(&num_entries.to_be_bytes());

    Ok(())
}

fn write_stco(
    v: &mut Vec<u8>,
    _header: &super::Header,
    stream: &super::Stream,
    co64: bool,
) -> Result<(), Error> {
    // Entry count
    v.extend((stream.chunks.len() as u32).to_be_bytes());

    for chunk in &stream.chunks {
        if co64 {
            v.extend(chunk.offset.to_be_bytes());
        } else {
            v.extend(u32::try_from(chunk.offset).unwrap().to_be_bytes());
        }
    }

    Ok(())
}

fn write_tref(
    v: &mut Vec<u8>,
    _header: &super::Header,
    references: &[TrackReference],
) -> Result<(), Error> {
    for reference in references {
        write_box(v, reference.reference_type, |v| {
            for track_id in &reference.track_ids {
                v.extend(track_id.to_be_bytes());
            }

            Ok(())
        })?;
    }

    Ok(())
}

fn write_edts(v: &mut Vec<u8>, stream: &super::Stream) -> Result<(), Error> {
    write_full_box(v, b"elst", FULL_BOX_VERSION_1, 0, |v| write_elst(v, stream))?;

    Ok(())
}

fn write_elst(v: &mut Vec<u8>, stream: &super::Stream) -> Result<(), Error> {
    // Entry count
    v.extend((stream.elst_infos.len() as u32).to_be_bytes());

    for elst_info in &stream.elst_infos {
        v.extend(
            elst_info
                .duration
                .expect("Should have been set by `get_elst_infos`")
                .to_be_bytes(),
        );

        // Media time
        v.extend(elst_info.start.to_be_bytes());

        // Media rate
        v.extend(1u16.to_be_bytes());
        v.extend(0u16.to_be_bytes());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            gst::init().unwrap();
        });
    }

    mod interleave {
        use super::*;
        use crate::mp4mux::boxes::get_interleave_type_for_uncc_format;
        use gst_video::VideoFormat;

        const PLANAR: u8 = 0; // aka component
        const PACKED: u8 = 1; // aka pixel
        const SEMI_PLANAR: u8 = 2; // aka mixed
        const MULTI_Y: u8 = 5;

        fn check_interleave(format: VideoFormat, expected_uncompressed_interleave: u8) {
            let video_info = gst_video::VideoInfo::builder(format, 320, 240).build();
            assert!(video_info.is_ok());
            let interleave = get_interleave_type_for_uncc_format(&video_info.unwrap());
            assert_eq!(interleave, expected_uncompressed_interleave);
        }

        #[test]
        fn interleave_i420() {
            init();
            check_interleave(VideoFormat::I420, PLANAR);
        }

        #[test]
        fn interleave_yv12() {
            init();
            check_interleave(VideoFormat::Yv12, PLANAR);
        }

        #[test]
        fn interleave_yuy2() {
            init();
            check_interleave(VideoFormat::Yuy2, MULTI_Y);
        }

        #[test]
        fn interleave_uyvy() {
            init();
            check_interleave(VideoFormat::Uyvy, MULTI_Y);
        }

        #[test]
        fn interleave_ayuv() {
            init();
            check_interleave(VideoFormat::Ayuv, PACKED);
        }

        #[test]
        fn interleave_rgbx() {
            init();
            check_interleave(VideoFormat::Rgbx, PACKED);
        }

        #[test]
        fn interleave_bgrx() {
            init();
            check_interleave(VideoFormat::Bgrx, PACKED);
        }

        #[test]
        fn interleave_xrgb() {
            init();
            check_interleave(VideoFormat::Xrgb, PACKED);
        }

        #[test]
        fn interleave_xbgr() {
            init();
            check_interleave(VideoFormat::Xbgr, PACKED);
        }

        #[test]
        fn interleave_argb() {
            init();
            check_interleave(VideoFormat::Argb, PACKED);
        }

        #[test]
        fn interleave_abgr() {
            init();
            check_interleave(VideoFormat::Abgr, PACKED);
        }

        #[test]
        fn interleave_rgb() {
            init();
            check_interleave(VideoFormat::Rgb, PACKED);
        }

        #[test]
        fn interleave_bgr() {
            init();
            check_interleave(VideoFormat::Bgr, PACKED);
        }

        #[test]
        fn interleave_y41b() {
            init();
            check_interleave(VideoFormat::Y41b, PLANAR);
        }

        #[test]
        fn interleave_y42b() {
            init();
            check_interleave(VideoFormat::Y42b, PLANAR);
        }

        #[test]
        fn interleave_yvyu() {
            init();
            check_interleave(VideoFormat::Yvyu, MULTI_Y);
        }

        #[test]
        fn interleave_y444() {
            init();
            check_interleave(VideoFormat::Y444, PLANAR);
        }

        #[test]
        fn interleave_v210() {
            init();
            check_interleave(VideoFormat::V210, PACKED);
        }

        #[test]
        fn interleave_v216() {
            init();
            check_interleave(VideoFormat::V216, PACKED);
        }

        #[test]
        fn interleave_nv12() {
            init();
            check_interleave(VideoFormat::Nv12, SEMI_PLANAR);
        }

        #[test]
        fn interleave_nv21() {
            init();
            check_interleave(VideoFormat::Nv21, SEMI_PLANAR);
        }

        #[test]
        fn interleave_gray8() {
            init();
            check_interleave(VideoFormat::Gray8, PLANAR);
        }

        #[test]
        fn interleave_gray16be() {
            init();
            check_interleave(VideoFormat::Gray16Be, PLANAR);
        }

        #[test]
        fn interleave_gray16le() {
            init();
            check_interleave(VideoFormat::Gray16Le, PLANAR);
        }

        #[test]
        fn interleave_v308() {
            init();
            check_interleave(VideoFormat::V308, PACKED);
        }

        #[test]
        fn interleave_rgb16() {
            init();
            check_interleave(VideoFormat::Rgb16, PACKED);
        }

        #[test]
        fn interleave_bgr16() {
            init();
            check_interleave(VideoFormat::Bgr16, PACKED);
        }

        #[test]
        fn interleave_rgb15() {
            init();
            check_interleave(VideoFormat::Rgb15, PACKED);
        }

        #[test]
        fn interleave_bgr15() {
            init();
            check_interleave(VideoFormat::Bgr15, PACKED);
        }

        #[test]
        fn interleave_uyvp() {
            init();
            check_interleave(VideoFormat::Uyvp, PACKED);
        }

        #[test]
        fn interleave_a420() {
            init();
            check_interleave(VideoFormat::A420, PLANAR);
        }

        // TODO: RGB8P (35)

        #[test]
        fn interleave_yuv9() {
            init();
            check_interleave(VideoFormat::Yuv9, PLANAR);
        }

        #[test]
        fn interleave_yvu9() {
            init();
            check_interleave(VideoFormat::Yvu9, PLANAR);
        }

        #[test]
        fn interleave_iyu1() {
            init();
            check_interleave(VideoFormat::Iyu1, PACKED);
        }

        #[test]
        fn interleave_argb64() {
            init();
            check_interleave(VideoFormat::Argb64, PACKED);
        }

        #[test]
        fn interleave_ayuv64() {
            init();
            check_interleave(VideoFormat::Ayuv64, PACKED);
        }

        #[test]
        fn interleave_r210() {
            init();
            check_interleave(VideoFormat::R210, PACKED);
        }

        #[test]
        fn interleave_i420_10be() {
            init();
            check_interleave(VideoFormat::I42010be, PLANAR);
        }

        #[test]
        fn interleave_i420_10le() {
            init();
            check_interleave(VideoFormat::I42010le, PLANAR);
        }

        #[test]
        fn interleave_i422_10be() {
            init();
            check_interleave(VideoFormat::I42210be, PLANAR);
        }

        #[test]
        fn interleave_i422_10le() {
            init();
            check_interleave(VideoFormat::I42210le, PLANAR);
        }

        #[test]
        fn interleave_y444_10be() {
            init();
            check_interleave(VideoFormat::Y44410be, PLANAR);
        }

        #[test]
        fn interleave_y444_10le() {
            init();
            check_interleave(VideoFormat::Y44410le, PLANAR);
        }

        #[test]
        fn interleave_gbr() {
            init();
            check_interleave(VideoFormat::Gbr, PLANAR);
        }

        #[test]
        fn interleave_gbr_10be() {
            init();
            check_interleave(VideoFormat::Gbr10be, PLANAR);
        }

        #[test]
        fn interleave_gbr_10le() {
            init();
            check_interleave(VideoFormat::Gbr10le, PLANAR);
        }

        #[test]
        fn interleave_nv16() {
            init();
            check_interleave(VideoFormat::Nv16, SEMI_PLANAR);
        }

        #[test]
        fn interleave_nv24() {
            init();
            check_interleave(VideoFormat::Nv24, SEMI_PLANAR);
        }

        #[test]
        fn interleave_nv12_64z32() {
            init();
            check_interleave(VideoFormat::Nv1264z32, SEMI_PLANAR);
        }

        #[test]
        fn interleave_a420_10be() {
            init();
            check_interleave(VideoFormat::A42010be, PLANAR);
        }

        #[test]
        fn interleave_a420_10le() {
            init();
            check_interleave(VideoFormat::A42010le, PLANAR);
        }

        #[test]
        fn interleave_a422_10be() {
            init();
            check_interleave(VideoFormat::A42210be, PLANAR);
        }

        #[test]
        fn interleave_a422_10le() {
            init();
            check_interleave(VideoFormat::A42210le, PLANAR);
        }

        #[test]
        fn interleave_a444_10be() {
            init();
            check_interleave(VideoFormat::A44410be, PLANAR);
        }

        #[test]
        fn interleave_a444_10le() {
            init();
            check_interleave(VideoFormat::A44410le, PLANAR);
        }

        #[test]
        fn interleave_nv61() {
            init();
            check_interleave(VideoFormat::Nv61, SEMI_PLANAR);
        }

        #[test]
        fn interleave_p010_10be() {
            init();
            check_interleave(VideoFormat::P01010be, SEMI_PLANAR);
        }

        #[test]
        fn interleave_p010_10le() {
            init();
            check_interleave(VideoFormat::P01010le, SEMI_PLANAR);
        }

        #[test]
        fn interleave_iyu2() {
            init();
            check_interleave(VideoFormat::Iyu2, PACKED);
        }

        #[test]
        fn interleave_vyuy() {
            init();
            check_interleave(VideoFormat::Vyuy, MULTI_Y);
        }

        #[test]
        fn interleave_gbra() {
            init();
            check_interleave(VideoFormat::Gbra, PLANAR);
        }

        #[test]
        fn interleave_gbra_10be() {
            init();
            check_interleave(VideoFormat::Gbra10be, PLANAR);
        }

        #[test]
        fn interleave_gbra_10le() {
            init();
            check_interleave(VideoFormat::Gbra10le, PLANAR);
        }

        #[test]
        fn interleave_gbr_12be() {
            init();
            check_interleave(VideoFormat::Gbr12be, PLANAR);
        }

        #[test]
        fn interleave_gbr_12le() {
            init();
            check_interleave(VideoFormat::Gbr12le, PLANAR);
        }

        #[test]
        fn interleave_gbra_12be() {
            init();
            check_interleave(VideoFormat::Gbra12be, PLANAR);
        }

        #[test]
        fn interleave_gbra_12le() {
            init();
            check_interleave(VideoFormat::Gbra12le, PLANAR);
        }

        #[test]
        fn interleave_i420_12be() {
            init();
            check_interleave(VideoFormat::I42012be, PLANAR);
        }

        #[test]
        fn interleave_i420_12le() {
            init();
            check_interleave(VideoFormat::I42012le, PLANAR);
        }

        #[test]
        fn interleave_i422_12be() {
            init();
            check_interleave(VideoFormat::I42212be, PLANAR);
        }

        #[test]
        fn interleave_i422_12le() {
            init();
            check_interleave(VideoFormat::I42212le, PLANAR);
        }

        #[test]
        fn interleave_y444_12be() {
            init();
            check_interleave(VideoFormat::Y44412be, PLANAR);
        }

        #[test]
        fn interleave_y444_12le() {
            init();
            check_interleave(VideoFormat::Y44412le, PLANAR);
        }

        // TODO: GRAY10_LE32

        #[test]
        fn interleave_nv12_10le32() {
            init();
            check_interleave(VideoFormat::Nv1210le32, SEMI_PLANAR);
        }

        #[test]
        fn interleave_nv16_10le32() {
            init();
            check_interleave(VideoFormat::Nv1610le32, SEMI_PLANAR);
        }

        #[test]
        fn interleave_nv12_10le40() {
            init();
            check_interleave(VideoFormat::Nv1210le40, SEMI_PLANAR);
        }

        #[test]
        fn interleave_y210() {
            init();
            check_interleave(VideoFormat::Y210, PACKED);
        }

        #[test]
        fn interleave_y410() {
            init();
            check_interleave(VideoFormat::Y410, PACKED);
        }

        #[test]
        fn interleave_vuya() {
            init();
            check_interleave(VideoFormat::Vuya, PACKED);
        }

        #[test]
        fn interleave_bgra10a2_le() {
            init();
            check_interleave(VideoFormat::Bgr10a2Le, PACKED);
        }

        #[test]
        fn interleave_rgb10a2_le() {
            init();
            check_interleave(VideoFormat::Rgb10a2Le, PACKED);
        }

        #[test]
        fn interleave_y444_16be() {
            init();
            check_interleave(VideoFormat::Y44416be, PLANAR);
        }

        #[test]
        fn interleave_y444_16le() {
            init();
            check_interleave(VideoFormat::Y44416le, PLANAR);
        }

        #[test]
        fn interleave_p016be() {
            init();
            check_interleave(VideoFormat::P016Be, SEMI_PLANAR);
        }

        #[test]
        fn interleave_p016le() {
            init();
            check_interleave(VideoFormat::P016Le, SEMI_PLANAR);
        }

        #[test]
        fn interleave_p012be() {
            init();
            check_interleave(VideoFormat::P012Be, SEMI_PLANAR);
        }

        #[test]
        fn interleave_p012le() {
            init();
            check_interleave(VideoFormat::P012Le, SEMI_PLANAR);
        }

        #[test]
        fn interleave_y212be() {
            init();
            check_interleave(VideoFormat::Y212Be, PACKED);
        }

        #[test]
        fn interleave_y212le() {
            init();
            check_interleave(VideoFormat::Y212Le, PACKED);
        }

        #[test]
        fn interleave_y412be() {
            init();
            check_interleave(VideoFormat::Y412Be, PACKED);
        }

        #[test]
        fn interleave_y412le() {
            init();
            check_interleave(VideoFormat::Y412Le, PACKED);
        }

        #[test]
        fn interleave_nv12_4l4() {
            init();
            check_interleave(VideoFormat::Nv124l4, SEMI_PLANAR);
        }

        #[test]
        fn interleave_nv12_32l32() {
            init();
            check_interleave(VideoFormat::Nv1232l32, SEMI_PLANAR);
        }

        #[test]
        fn interleave_rgbp() {
            init();
            check_interleave(VideoFormat::Rgbp, PLANAR);
        }

        #[test]
        fn interleave_bgrp() {
            init();
            check_interleave(VideoFormat::Bgrp, PLANAR);
        }

        #[test]
        fn interleave_av12() {
            init();
            check_interleave(VideoFormat::Av12, SEMI_PLANAR);
        }

        #[test]
        fn interleave_argb64_le() {
            init();
            check_interleave(VideoFormat::Argb64Le, PACKED);
        }

        #[test]
        fn interleave_argb64_be() {
            init();
            check_interleave(VideoFormat::Argb64Be, PACKED);
        }

        #[test]
        fn interleave_rgba64_le() {
            init();
            check_interleave(VideoFormat::Rgba64Le, PACKED);
        }

        #[test]
        fn interleave_rgba64_be() {
            init();
            check_interleave(VideoFormat::Rgba64Be, PACKED);
        }

        #[test]
        fn interleave_bgra64_le() {
            init();
            check_interleave(VideoFormat::Bgra64Le, PACKED);
        }

        #[test]
        fn interleave_bgra64_be() {
            init();
            check_interleave(VideoFormat::Bgra64Be, PACKED);
        }

        #[test]
        fn interleave_abgr64_le() {
            init();
            check_interleave(VideoFormat::Abgr64Le, PACKED);
        }

        #[test]
        fn interleave_abgr64_be() {
            init();
            check_interleave(VideoFormat::Abgr64Be, PACKED);
        }

        // TODO: NV12_16L32S (110) and after - requires gst-video 1.22
    }

    mod subsampling {
        use super::*;
        use crate::mp4mux::boxes::get_sampling_type_for_uncc_format;
        use gst_video::VideoFormat;

        const NONE: u8 = 0; // aka 4:4:4
        const Y422: u8 = 1; // 4:2:2
        const Y420: u8 = 2; // 4:2:0
        const Y411: u8 = 3; // 4:1:1

        fn check_subsampling(format: VideoFormat, expected_subsampling: u8) {
            let video_info = gst_video::VideoInfo::builder(format, 320, 240).build();
            assert!(video_info.is_ok());
            let sampling_type = get_sampling_type_for_uncc_format(&video_info.unwrap());
            assert_eq!(sampling_type, expected_subsampling);
        }

        #[test]
        fn subsampling_i420() {
            init();
            check_subsampling(VideoFormat::I420, Y420);
        }

        #[test]
        fn subsampling_yv12() {
            init();
            check_subsampling(VideoFormat::Yv12, Y420);
        }

        #[test]
        fn subsampling_yuy2() {
            init();
            check_subsampling(VideoFormat::Yuy2, Y422);
        }

        #[test]
        fn subsampling_uyvy() {
            init();
            check_subsampling(VideoFormat::Uyvy, Y422);
        }

        #[test]
        fn subsampling_ayuv() {
            init();
            check_subsampling(VideoFormat::Ayuv, NONE);
        }

        #[test]
        fn subsampling_rgbx() {
            init();
            check_subsampling(VideoFormat::Rgbx, NONE);
        }

        #[test]
        fn subsampling_bgrx() {
            init();
            check_subsampling(VideoFormat::Bgrx, NONE);
        }

        #[test]
        fn subsampling_xrgb() {
            init();
            check_subsampling(VideoFormat::Xrgb, NONE);
        }

        #[test]
        fn subsampling_xbgr() {
            init();
            check_subsampling(VideoFormat::Xbgr, NONE);
        }

        #[test]
        fn subsampling_argb() {
            init();
            check_subsampling(VideoFormat::Argb, NONE);
        }

        #[test]
        fn subsampling_abgr() {
            init();
            check_subsampling(VideoFormat::Abgr, NONE);
        }

        #[test]
        fn subsampling_rgb() {
            init();
            check_subsampling(VideoFormat::Rgb, NONE);
        }

        #[test]
        fn subsampling_bgr() {
            init();
            check_subsampling(VideoFormat::Bgr, NONE);
        }

        #[test]
        fn subsampling_y41b() {
            init();
            check_subsampling(VideoFormat::Y41b, Y411);
        }

        #[test]
        fn subsampling_y42b() {
            init();
            check_subsampling(VideoFormat::Y42b, Y422);
        }

        #[test]
        fn subsampling_yvyu() {
            init();
            check_subsampling(VideoFormat::Yvyu, Y422);
        }

        #[test]
        fn subsampling_y444() {
            init();
            check_subsampling(VideoFormat::Y444, NONE);
        }

        #[test]
        fn subsampling_v210() {
            init();
            check_subsampling(VideoFormat::V210, Y422);
        }

        #[test]
        fn subsampling_v216() {
            init();
            check_subsampling(VideoFormat::V216, Y422);
        }

        #[test]
        fn subsampling_nv12() {
            init();
            check_subsampling(VideoFormat::Nv12, Y420);
        }

        #[test]
        fn subsampling_nv21() {
            init();
            check_subsampling(VideoFormat::Nv21, Y420);
        }

        #[test]
        fn subsampling_gray8() {
            init();
            check_subsampling(VideoFormat::Gray8, NONE);
        }

        #[test]
        fn subsampling_gray16be() {
            init();
            check_subsampling(VideoFormat::Gray16Be, NONE);
        }

        #[test]
        fn subsampling_gray16le() {
            init();
            check_subsampling(VideoFormat::Gray16Le, NONE);
        }

        #[test]
        fn subsampling_v308() {
            init();
            check_subsampling(VideoFormat::V308, NONE);
        }

        #[test]
        fn subsampling_rgb16() {
            init();
            check_subsampling(VideoFormat::Rgb16, NONE);
        }

        #[test]
        fn subsampling_bgr16() {
            init();
            check_subsampling(VideoFormat::Bgr16, NONE);
        }

        #[test]
        fn subsampling_rgb15() {
            init();
            check_subsampling(VideoFormat::Rgb15, NONE);
        }

        #[test]
        fn subsampling_bgr15() {
            init();
            check_subsampling(VideoFormat::Bgr15, NONE);
        }

        #[test]
        fn subsampling_uyvp() {
            init();
            check_subsampling(VideoFormat::Uyvp, Y422);
        }

        #[test]
        fn subsampling_a420() {
            init();
            check_subsampling(VideoFormat::A420, Y420);
        }

        // TODO: RGB8P (35)

        #[test]
        fn subsampling_yuv9() {
            init();
            check_subsampling(VideoFormat::Yuv9, Y411);
        }

        #[test]
        fn subsampling_yvu9() {
            init();
            check_subsampling(VideoFormat::Yvu9, Y411);
        }

        #[test]
        fn subsampling_iyu1() {
            init();
            check_subsampling(VideoFormat::Iyu1, Y411);
        }

        #[test]
        fn subsampling_argb64() {
            init();
            check_subsampling(VideoFormat::Argb64, NONE);
        }

        #[test]
        fn subsampling_ayuv64() {
            init();
            check_subsampling(VideoFormat::Ayuv64, NONE);
        }

        #[test]
        fn subsampling_r210() {
            init();
            check_subsampling(VideoFormat::R210, NONE);
        }

        #[test]
        fn subsampling_i420_10be() {
            init();
            check_subsampling(VideoFormat::I42010be, Y420);
        }

        #[test]
        fn subsampling_i420_10le() {
            init();
            check_subsampling(VideoFormat::I42010le, Y420);
        }

        #[test]
        fn subsampling_i422_10be() {
            init();
            check_subsampling(VideoFormat::I42210be, Y422);
        }

        #[test]
        fn subsampling_i422_10le() {
            init();
            check_subsampling(VideoFormat::I42210le, Y422);
        }

        #[test]
        fn subsampling_y444_10be() {
            init();
            check_subsampling(VideoFormat::Y44410be, NONE);
        }

        #[test]
        fn subsampling_y444_10le() {
            init();
            check_subsampling(VideoFormat::Y44410le, NONE);
        }

        #[test]
        fn subsampling_gbr() {
            init();
            check_subsampling(VideoFormat::Gbr, NONE);
        }

        #[test]
        fn subsampling_gbr_10be() {
            init();
            check_subsampling(VideoFormat::Gbr10be, NONE);
        }

        #[test]
        fn subsampling_gbr_10le() {
            init();
            check_subsampling(VideoFormat::Gbr10le, NONE);
        }

        #[test]
        fn subsampling_nv16() {
            init();
            check_subsampling(VideoFormat::Nv16, Y422);
        }

        #[test]
        fn subsampling_nv24() {
            init();
            check_subsampling(VideoFormat::Nv24, NONE);
        }

        #[test]
        fn subsampling_nv12_64z32() {
            init();
            check_subsampling(VideoFormat::Nv1264z32, Y420);
        }

        #[test]
        fn subsampling_a420_10be() {
            init();
            check_subsampling(VideoFormat::A42010be, Y420);
        }

        #[test]
        fn subsampling_a420_10le() {
            init();
            check_subsampling(VideoFormat::A42010le, Y420);
        }

        #[test]
        fn subsampling_a422_10be() {
            init();
            check_subsampling(VideoFormat::A42210be, Y422);
        }

        #[test]
        fn subsampling_a422_10le() {
            init();
            check_subsampling(VideoFormat::A42210le, Y422);
        }

        #[test]
        fn subsampling_a444_10be() {
            init();
            check_subsampling(VideoFormat::A44410be, NONE);
        }

        #[test]
        fn subsampling_a444_10le() {
            init();
            check_subsampling(VideoFormat::A44410le, NONE);
        }

        #[test]
        fn subsampling_nv61() {
            init();
            check_subsampling(VideoFormat::Nv61, Y422);
        }

        #[test]
        fn subsampling_p010_10be() {
            init();
            check_subsampling(VideoFormat::P01010be, Y420);
        }

        #[test]
        fn subsampling_p010_10le() {
            init();
            check_subsampling(VideoFormat::P01010le, Y420);
        }

        #[test]
        fn subsampling_iyu2() {
            init();
            check_subsampling(VideoFormat::Iyu2, NONE);
        }

        #[test]
        fn subsampling_vyuy() {
            init();
            check_subsampling(VideoFormat::Vyuy, Y422);
        }

        #[test]
        fn subsampling_gbra() {
            init();
            check_subsampling(VideoFormat::Gbra, NONE);
        }

        #[test]
        fn subsampling_gbra_10be() {
            init();
            check_subsampling(VideoFormat::Gbra10be, NONE);
        }

        #[test]
        fn subsampling_gbra_10le() {
            init();
            check_subsampling(VideoFormat::Gbra10le, NONE);
        }

        #[test]
        fn subsampling_gbr_12be() {
            init();
            check_subsampling(VideoFormat::Gbr12be, NONE);
        }

        #[test]
        fn subsampling_gbr_12le() {
            init();
            check_subsampling(VideoFormat::Gbr12le, NONE);
        }

        #[test]
        fn subsampling_gbra_12be() {
            init();
            check_subsampling(VideoFormat::Gbra12be, NONE);
        }

        #[test]
        fn subsampling_gbra_12le() {
            init();
            check_subsampling(VideoFormat::Gbra12le, NONE);
        }

        #[test]
        fn subsampling_i420_12be() {
            init();
            check_subsampling(VideoFormat::I42012be, Y420);
        }

        #[test]
        fn subsampling_i420_12le() {
            init();
            check_subsampling(VideoFormat::I42012le, Y420);
        }

        #[test]
        fn subsampling_i422_12be() {
            init();
            check_subsampling(VideoFormat::I42212be, Y422);
        }

        #[test]
        fn subsampling_i422_12le() {
            init();
            check_subsampling(VideoFormat::I42212le, Y422);
        }

        #[test]
        fn subsampling_y444_12be() {
            init();
            check_subsampling(VideoFormat::Y44412be, NONE);
        }

        #[test]
        fn subsampling_y444_12le() {
            init();
            check_subsampling(VideoFormat::Y44412le, NONE);
        }

        // TODO: GRAY10_LE32

        #[test]
        fn subsampling_nv12_10le32() {
            init();
            check_subsampling(VideoFormat::Nv1210le32, Y420);
        }

        #[test]
        fn subsampling_nv16_10le32() {
            init();
            check_subsampling(VideoFormat::Nv1610le32, Y422);
        }

        #[test]
        fn subsampling_nv12_10le40() {
            init();
            check_subsampling(VideoFormat::Nv1210le40, Y420);
        }

        #[test]
        fn subsampling_y210() {
            init();
            check_subsampling(VideoFormat::Y210, Y422);
        }

        #[test]
        fn subsampling_y410() {
            init();
            check_subsampling(VideoFormat::Y410, NONE);
        }

        #[test]
        fn subsampling_vuya() {
            init();
            check_subsampling(VideoFormat::Vuya, NONE);
        }

        #[test]
        fn subsampling_bgra10a2_le() {
            init();
            check_subsampling(VideoFormat::Bgr10a2Le, NONE);
        }

        #[test]
        fn subsampling_rgb10a2_le() {
            init();
            check_subsampling(VideoFormat::Rgb10a2Le, NONE);
        }

        #[test]
        fn subsampling_y444_16be() {
            init();
            check_subsampling(VideoFormat::Y44416be, NONE);
        }

        #[test]
        fn subsampling_y444_16le() {
            init();
            check_subsampling(VideoFormat::Y44416le, NONE);
        }

        #[test]
        fn subsampling_p016be() {
            init();
            check_subsampling(VideoFormat::P016Be, Y420);
        }

        #[test]
        fn subsampling_p016le() {
            init();
            check_subsampling(VideoFormat::P016Le, Y420);
        }

        #[test]
        fn subsampling_p012be() {
            init();
            check_subsampling(VideoFormat::P012Be, Y420);
        }

        #[test]
        fn subsampling_p012le() {
            init();
            check_subsampling(VideoFormat::P012Le, Y420);
        }

        #[test]
        fn subsampling_y212be() {
            init();
            check_subsampling(VideoFormat::Y212Be, Y422);
        }

        #[test]
        fn subsampling_y212le() {
            init();
            check_subsampling(VideoFormat::Y212Le, Y422);
        }

        #[test]
        fn subsampling_y412be() {
            init();
            check_subsampling(VideoFormat::Y412Be, NONE);
        }

        #[test]
        fn subsampling_y412le() {
            init();
            check_subsampling(VideoFormat::Y412Le, NONE);
        }

        #[test]
        fn subsampling_nv12_4l4() {
            init();
            check_subsampling(VideoFormat::Nv124l4, Y420);
        }

        #[test]
        fn subsampling_nv12_32l32() {
            init();
            check_subsampling(VideoFormat::Nv1232l32, Y420);
        }

        #[test]
        fn subsampling_rgbp() {
            init();
            check_subsampling(VideoFormat::Rgbp, NONE);
        }

        #[test]
        fn subsampling_bgrp() {
            init();
            check_subsampling(VideoFormat::Bgrp, NONE);
        }

        #[test]
        fn subsampling_av12() {
            init();
            check_subsampling(VideoFormat::Av12, Y420);
        }

        #[test]
        fn subsampling_argb64_le() {
            init();
            check_subsampling(VideoFormat::Argb64Le, NONE);
        }

        #[test]
        fn subsampling_argb64_be() {
            init();
            check_subsampling(VideoFormat::Argb64Be, NONE);
        }

        #[test]
        fn subsampling_rgba64_le() {
            init();
            check_subsampling(VideoFormat::Rgba64Le, NONE);
        }

        #[test]
        fn subsampling_rgba64_be() {
            init();
            check_subsampling(VideoFormat::Rgba64Be, NONE);
        }

        #[test]
        fn subsampling_bgra64_le() {
            init();
            check_subsampling(VideoFormat::Bgra64Le, NONE);
        }

        #[test]
        fn subsampling_bgra64_be() {
            init();
            check_subsampling(VideoFormat::Bgra64Be, NONE);
        }

        #[test]
        fn subsampling_abgr64_le() {
            init();
            check_subsampling(VideoFormat::Abgr64Le, NONE);
        }

        #[test]
        fn subsampling_abgr64_be() {
            init();
            check_subsampling(VideoFormat::Abgr64Be, NONE);
        }

        // TODO: NV12_16L32S (110) and after - requires gst-video 1.22
    }

    mod pixelsize {
        use super::*;
        use crate::mp4mux::boxes::get_pixel_size_for_uncc_format;
        use gst_video::VideoFormat;

        fn check_pixel_size(format: VideoFormat, expected_size: u32) {
            let video_info = gst_video::VideoInfo::builder(format, 320, 240).build();
            assert!(video_info.is_ok());
            let pixel_size = get_pixel_size_for_uncc_format(&video_info.unwrap());
            assert_eq!(pixel_size, expected_size);
        }

        #[test]
        fn pixel_size_i420() {
            init();
            check_pixel_size(VideoFormat::I420, 0);
        }

        #[test]
        fn pixel_size_yv12() {
            init();
            check_pixel_size(VideoFormat::Yv12, 0);
        }

        #[test]
        fn pixel_size_yuy2() {
            init();
            check_pixel_size(VideoFormat::Yuy2, 4);
        }

        #[test]
        fn pixel_size_uyvy() {
            init();
            check_pixel_size(VideoFormat::Uyvy, 4);
        }

        #[test]
        fn pixel_size_ayuv() {
            init();
            check_pixel_size(VideoFormat::Ayuv, 4);
        }

        #[test]
        fn pixel_size_rgbx() {
            init();
            check_pixel_size(VideoFormat::Rgbx, 4);
        }

        #[test]
        fn pixel_size_bgrx() {
            init();
            check_pixel_size(VideoFormat::Bgrx, 4);
        }

        #[test]
        fn pixel_size_xrgb() {
            init();
            check_pixel_size(VideoFormat::Xrgb, 4);
        }

        #[test]
        fn pixel_size_xbgr() {
            init();
            check_pixel_size(VideoFormat::Xbgr, 4);
        }

        #[test]
        fn pixel_size_argb() {
            init();
            check_pixel_size(VideoFormat::Argb, 4);
        }

        #[test]
        fn pixel_size_abgr() {
            init();
            check_pixel_size(VideoFormat::Abgr, 4);
        }

        #[test]
        fn pixel_size_rgb() {
            init();
            check_pixel_size(VideoFormat::Rgb, 3);
        }

        #[test]
        fn pixel_size_bgr() {
            init();
            check_pixel_size(VideoFormat::Bgr, 3);
        }

        #[test]
        fn pixel_size_y41b() {
            init();
            check_pixel_size(VideoFormat::Y41b, 0);
        }

        #[test]
        fn pixel_size_y42b() {
            init();
            check_pixel_size(VideoFormat::Y42b, 0);
        }

        #[test]
        fn pixel_size_yvyu() {
            init();
            check_pixel_size(VideoFormat::Yvyu, 4);
        }

        #[test]
        fn pixel_size_y444() {
            init();
            check_pixel_size(VideoFormat::Y444, 0);
        }

        #[test]
        fn pixel_size_v210() {
            init();
            check_pixel_size(VideoFormat::V210, 0);
        }

        #[test]
        fn pixel_size_v216() {
            init();
            check_pixel_size(VideoFormat::V216, 4);
        }

        #[test]
        fn pixel_size_nv12() {
            init();
            check_pixel_size(VideoFormat::Nv12, 0);
        }

        #[test]
        fn pixel_size_nv21() {
            init();
            check_pixel_size(VideoFormat::Nv21, 0);
        }

        #[test]
        fn pixel_size_gray8() {
            init();
            check_pixel_size(VideoFormat::Gray8, 0);
        }

        #[test]
        fn pixel_size_gray16be() {
            init();
            check_pixel_size(VideoFormat::Gray16Be, 0);
        }

        #[test]
        fn pixel_size_gray16le() {
            init();
            check_pixel_size(VideoFormat::Gray16Le, 0);
        }

        #[test]
        fn pixel_size_v308() {
            init();
            check_pixel_size(VideoFormat::V308, 3);
        }

        #[test]
        fn pixel_size_rgb16() {
            init();
            check_pixel_size(VideoFormat::Rgb16, 2);
        }

        #[test]
        fn pixel_size_bgr16() {
            init();
            check_pixel_size(VideoFormat::Bgr16, 2);
        }

        #[test]
        fn pixel_size_rgb15() {
            init();
            check_pixel_size(VideoFormat::Rgb15, 2);
        }

        #[test]
        fn pixel_size_bgr15() {
            init();
            check_pixel_size(VideoFormat::Bgr15, 2);
        }

        #[test]
        fn pixel_size_uyvp() {
            init();
            // TODO: this might need more work
            check_pixel_size(VideoFormat::Uyvp, 0);
        }

        #[test]
        fn pixel_size_a420() {
            init();
            check_pixel_size(VideoFormat::A420, 0);
        }

        // TODO: RGB8P (35)

        #[test]
        fn pixel_size_yuv9() {
            init();
            check_pixel_size(VideoFormat::Yuv9, 0);
        }

        #[test]
        fn pixel_size_yvu9() {
            init();
            check_pixel_size(VideoFormat::Yvu9, 0);
        }

        #[test]
        fn pixel_size_iyu1() {
            init();
            check_pixel_size(VideoFormat::Iyu1, 0);
        }

        #[test]
        fn pixel_size_argb64() {
            init();
            check_pixel_size(VideoFormat::Argb64, 8);
        }

        #[test]
        fn pixel_size_ayuv64() {
            init();
            check_pixel_size(VideoFormat::Ayuv64, 8);
        }

        #[test]
        fn pixel_size_r210() {
            init();
            check_pixel_size(VideoFormat::R210, 4);
        }

        #[test]
        fn pixel_size_i420_10be() {
            init();
            check_pixel_size(VideoFormat::I42010be, 0);
        }

        #[test]
        fn pixel_size_i420_10le() {
            init();
            check_pixel_size(VideoFormat::I42010le, 0);
        }

        #[test]
        fn pixel_size_i422_10be() {
            init();
            check_pixel_size(VideoFormat::I42210be, 0);
        }

        #[test]
        fn pixel_size_i422_10le() {
            init();
            check_pixel_size(VideoFormat::I42210le, 0);
        }

        #[test]
        fn pixel_size_y444_10be() {
            init();
            check_pixel_size(VideoFormat::Y44410be, 0);
        }

        #[test]
        fn pixel_size_y444_10le() {
            init();
            check_pixel_size(VideoFormat::Y44410le, 0);
        }

        #[test]
        fn pixel_size_gbr() {
            init();
            check_pixel_size(VideoFormat::Gbr, 0);
        }

        #[test]
        fn pixel_size_gbr_10be() {
            init();
            check_pixel_size(VideoFormat::Gbr10be, 0);
        }

        #[test]
        fn pixel_size_gbr_10le() {
            init();
            check_pixel_size(VideoFormat::Gbr10le, 0);
        }

        #[test]
        fn pixel_size_nv16() {
            init();
            check_pixel_size(VideoFormat::Nv16, 0);
        }

        #[test]
        fn pixel_size_nv24() {
            init();
            check_pixel_size(VideoFormat::Nv24, 0);
        }

        #[test]
        fn pixel_size_nv12_64z32() {
            init();
            check_pixel_size(VideoFormat::Nv1264z32, 0);
        }

        #[test]
        fn pixel_size_a420_10be() {
            init();
            check_pixel_size(VideoFormat::A42010be, 0);
        }

        #[test]
        fn pixel_size_a420_10le() {
            init();
            check_pixel_size(VideoFormat::A42010le, 0);
        }

        #[test]
        fn pixel_size_a422_10be() {
            init();
            check_pixel_size(VideoFormat::A42210be, 0);
        }

        #[test]
        fn pixel_size_a422_10le() {
            init();
            check_pixel_size(VideoFormat::A42210le, 0);
        }

        #[test]
        fn pixel_size_a444_10be() {
            init();
            check_pixel_size(VideoFormat::A44410be, 0);
        }

        #[test]
        fn pixel_size_a444_10le() {
            init();
            check_pixel_size(VideoFormat::A44410le, 0);
        }

        #[test]
        fn pixel_size_nv61() {
            init();
            check_pixel_size(VideoFormat::Nv61, 0);
        }

        #[test]
        fn pixel_size_p010_10be() {
            init();
            check_pixel_size(VideoFormat::P01010be, 0);
        }

        #[test]
        fn pixel_size_p010_10le() {
            init();
            check_pixel_size(VideoFormat::P01010le, 0);
        }

        #[test]
        fn pixel_size_iyu2() {
            init();
            check_pixel_size(VideoFormat::Iyu2, 3);
        }

        #[test]
        fn pixel_size_vyuy() {
            init();
            check_pixel_size(VideoFormat::Vyuy, 4);
        }

        #[test]
        fn pixel_size_gbra() {
            init();
            check_pixel_size(VideoFormat::Gbra, 0);
        }

        #[test]
        fn pixel_size_gbra_10be() {
            init();
            check_pixel_size(VideoFormat::Gbra10be, 0);
        }

        #[test]
        fn pixel_size_gbra_10le() {
            init();
            check_pixel_size(VideoFormat::Gbra10le, 0);
        }

        #[test]
        fn pixel_size_gbr_12be() {
            init();
            check_pixel_size(VideoFormat::Gbr12be, 0);
        }

        #[test]
        fn pixel_size_gbr_12le() {
            init();
            check_pixel_size(VideoFormat::Gbr12le, 0);
        }

        #[test]
        fn pixel_size_gbra_12be() {
            init();
            check_pixel_size(VideoFormat::Gbra12be, 0);
        }

        #[test]
        fn pixel_size_gbra_12le() {
            init();
            check_pixel_size(VideoFormat::Gbra12le, 0);
        }

        #[test]
        fn pixel_size_i420_12be() {
            init();
            check_pixel_size(VideoFormat::I42012be, 0);
        }

        #[test]
        fn pixel_size_i420_12le() {
            init();
            check_pixel_size(VideoFormat::I42012le, 0);
        }

        #[test]
        fn pixel_size_i422_12be() {
            init();
            check_pixel_size(VideoFormat::I42212be, 0);
        }

        #[test]
        fn pixel_size_i422_12le() {
            init();
            check_pixel_size(VideoFormat::I42212le, 0);
        }

        #[test]
        fn pixel_size_y444_12be() {
            init();
            check_pixel_size(VideoFormat::Y44412be, 0);
        }

        #[test]
        fn pixel_size_y444_12le() {
            init();
            check_pixel_size(VideoFormat::Y44412le, 0);
        }

        // TODO: GRAY10_LE32

        #[test]
        fn pixel_size_nv12_10le32() {
            init();
            check_pixel_size(VideoFormat::Nv1210le32, 0);
        }

        #[test]
        fn pixel_size_nv16_10le32() {
            init();
            check_pixel_size(VideoFormat::Nv1610le32, 0);
        }

        #[test]
        fn pixel_size_nv12_10le40() {
            init();
            check_pixel_size(VideoFormat::Nv1210le40, 0);
        }

        #[test]
        fn pixel_size_y210() {
            init();
            check_pixel_size(VideoFormat::Y210, 4);
        }

        #[test]
        fn pixel_size_y410() {
            init();
            check_pixel_size(VideoFormat::Y410, 4);
        }

        #[test]
        fn pixel_size_vuya() {
            init();
            check_pixel_size(VideoFormat::Vuya, 4);
        }

        #[test]
        fn pixel_size_bgra10a2_le() {
            init();
            check_pixel_size(VideoFormat::Bgr10a2Le, 4);
        }

        #[test]
        fn pixel_size_rgb10a2_le() {
            init();
            check_pixel_size(VideoFormat::Rgb10a2Le, 4);
        }

        #[test]
        fn pixel_size_y444_16be() {
            init();
            check_pixel_size(VideoFormat::Y44416be, 0);
        }

        #[test]
        fn pixel_size_y444_16le() {
            init();
            check_pixel_size(VideoFormat::Y44416le, 0);
        }

        #[test]
        fn pixel_size_p016be() {
            init();
            check_pixel_size(VideoFormat::P016Be, 0);
        }

        #[test]
        fn pixel_size_p016le() {
            init();
            check_pixel_size(VideoFormat::P016Le, 0);
        }

        #[test]
        fn pixel_size_p012be() {
            init();
            check_pixel_size(VideoFormat::P012Be, 0);
        }

        #[test]
        fn pixel_size_p012le() {
            init();
            check_pixel_size(VideoFormat::P012Le, 0);
        }

        #[test]
        fn pixel_size_y212be() {
            init();
            check_pixel_size(VideoFormat::Y212Be, 4);
        }

        #[test]
        fn pixel_size_y212le() {
            init();
            check_pixel_size(VideoFormat::Y212Le, 4);
        }

        #[test]
        fn pixel_size_y412be() {
            init();
            // TODO: check how this is actually packed
            check_pixel_size(VideoFormat::Y412Be, 8);
        }

        #[test]
        fn pixel_size_y412le() {
            init();
            // TODO: check how this is actually packed
            check_pixel_size(VideoFormat::Y412Le, 8);
        }

        #[test]
        fn pixel_size_nv12_4l4() {
            init();
            check_pixel_size(VideoFormat::Nv124l4, 0);
        }

        #[test]
        fn pixel_size_nv12_32l32() {
            init();
            check_pixel_size(VideoFormat::Nv1232l32, 0);
        }

        #[test]
        fn pixel_size_rgbp() {
            init();
            check_pixel_size(VideoFormat::Rgbp, 0);
        }

        #[test]
        fn pixel_size_bgrp() {
            init();
            check_pixel_size(VideoFormat::Bgrp, 0);
        }

        #[test]
        fn pixel_size_av12() {
            init();
            check_pixel_size(VideoFormat::Av12, 0);
        }

        #[test]
        fn pixel_size_argb64_le() {
            init();
            check_pixel_size(VideoFormat::Argb64Le, 8);
        }

        #[test]
        fn pixel_size_argb64_be() {
            init();
            check_pixel_size(VideoFormat::Argb64Be, 8);
        }

        #[test]
        fn pixel_size_rgba64_le() {
            init();
            check_pixel_size(VideoFormat::Rgba64Le, 8);
        }

        #[test]
        fn pixel_size_rgba64_be() {
            init();
            check_pixel_size(VideoFormat::Rgba64Be, 8);
        }

        #[test]
        fn pixel_size_bgra64_le() {
            init();
            check_pixel_size(VideoFormat::Bgra64Le, 8);
        }

        #[test]
        fn pixel_size_bgra64_be() {
            init();
            check_pixel_size(VideoFormat::Bgra64Be, 8);
        }

        #[test]
        fn pixel_size_abgr64_le() {
            init();
            check_pixel_size(VideoFormat::Abgr64Le, 8);
        }

        #[test]
        fn pixel_size_abgr64_be() {
            init();
            check_pixel_size(VideoFormat::Abgr64Be, 8);
        }

        // TODO: NV12_16L32S (110) and after - requires gst-video 1.22
    }

    mod profile_4cc {
        use super::*;
        use crate::mp4mux::boxes::get_profile_for_uncc_format;
        use gst_video::VideoFormat;

        fn check_profile(video_format: VideoFormat, fourcc: &[u8]) {
            assert_eq!(fourcc.len(), 4);
            let video_info = gst_video::VideoInfo::builder(video_format, 320, 240).build();
            assert!(video_info.is_ok());
            let format = video_info.unwrap();
            let pixel_size = get_profile_for_uncc_format(&format);
            assert_eq!(pixel_size, fourcc);
        }

        #[test]
        fn profile_4cc_i420() {
            init();
            check_profile(VideoFormat::I420, "i420".as_bytes());
        }

        #[test]
        fn profile_4cc_yv12() {
            init();
            // YUV 420 8 bits planar YCrCb
            check_profile(VideoFormat::Yv12, "yv20".as_bytes());
        }

        #[test]
        fn profile_4cc_yuy2() {
            init();
            // 8 bits YUV 422 packed Y0 Cb Y1 Cr
            check_profile(VideoFormat::Yuy2, "yuv2".as_bytes());
        }

        #[test]
        fn profile_4cc_uyvy() {
            init();
            // 8 bits YUV 422 packed Cb Y0 Cr Y1
            check_profile(VideoFormat::Uyvy, "2vuy".as_bytes());
        }

        #[test]
        fn profile_4cc_ayuv() {
            init();
            check_profile(VideoFormat::Ayuv, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_rgbx() {
            init();
            check_profile(VideoFormat::Rgbx, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_bgrx() {
            init();
            check_profile(VideoFormat::Bgrx, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_xrgb() {
            init();
            check_profile(VideoFormat::Xrgb, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_xbgr() {
            init();
            check_profile(VideoFormat::Xbgr, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_argb() {
            init();
            check_profile(VideoFormat::Argb, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_abgr() {
            init();
            check_profile(VideoFormat::Abgr, "abgr".as_bytes());
        }

        #[test]
        fn profile_4cc_rgb() {
            init();
            check_profile(VideoFormat::Rgb, "rgb3".as_bytes());
        }

        #[test]
        fn profile_4cc_bgr() {
            init();
            check_profile(VideoFormat::Bgr, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y41b() {
            init();
            check_profile(VideoFormat::Y41b, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y42b() {
            init();
            // YUV 422 8 bits planar YCbCr
            check_profile(VideoFormat::Y42b, "yu22".as_bytes());
        }

        #[test]
        fn profile_4cc_yvyu() {
            init();
            check_profile(VideoFormat::Yvyu, "yvyu".as_bytes());
        }

        #[test]
        fn profile_4cc_y444() {
            init();
            check_profile(VideoFormat::Y444, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_v210() {
            init();
            check_profile(VideoFormat::V210, "v210".as_bytes());
        }

        #[test]
        fn profile_4cc_v216() {
            init();
            check_profile(VideoFormat::V216, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_nv12() {
            init();
            check_profile(VideoFormat::Nv12, "nv12".as_bytes());
        }

        #[test]
        fn profile_4cc_nv21() {
            init();
            check_profile(VideoFormat::Nv21, "nv21".as_bytes());
        }

        #[test]
        fn profile_4cc_gray8() {
            init();
            check_profile(VideoFormat::Gray8, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_gray16be() {
            init();
            check_profile(VideoFormat::Gray16Be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_gray16le() {
            init();
            check_profile(VideoFormat::Gray16Le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_v308() {
            init();
            check_profile(VideoFormat::V308, "v308".as_bytes());
        }

        #[test]
        fn profile_4cc_rgb16() {
            init();
            check_profile(VideoFormat::Rgb16, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_bgr16() {
            init();
            check_profile(VideoFormat::Bgr16, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_rgb15() {
            init();
            check_profile(VideoFormat::Rgb15, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_bgr15() {
            init();
            check_profile(VideoFormat::Bgr15, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_uyvp() {
            init();
            check_profile(VideoFormat::Uyvp, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_a420() {
            init();
            check_profile(VideoFormat::A420, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_rgb8p() {
            init();
            check_profile(VideoFormat::Rgb8p, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_yuv9() {
            init();
            check_profile(VideoFormat::Yuv9, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_yvu9() {
            init();
            check_profile(VideoFormat::Yvu9, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_iyu1() {
            init();
            check_profile(VideoFormat::Iyu1, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_argb64() {
            init();
            check_profile(VideoFormat::Argb64, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_ayuv64() {
            init();
            check_profile(VideoFormat::Ayuv64, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_r210() {
            init();
            check_profile(VideoFormat::R210, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_i420_10be() {
            init();
            check_profile(VideoFormat::I42010be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_i420_10le() {
            init();
            check_profile(VideoFormat::I42010le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_i422_10be() {
            init();
            check_profile(VideoFormat::I42210be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_i422_10le() {
            init();
            check_profile(VideoFormat::I42210le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y444_10be() {
            init();
            check_profile(VideoFormat::Y44410be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y444_10le() {
            init();
            check_profile(VideoFormat::Y44410le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_gbr() {
            init();
            check_profile(VideoFormat::Gbr, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_gbr_10be() {
            init();
            check_profile(VideoFormat::Gbr10be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_gbr_10le() {
            init();
            check_profile(VideoFormat::Gbr10le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_nv16() {
            init();
            check_profile(VideoFormat::Nv16, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_nv24() {
            init();
            check_profile(VideoFormat::Nv24, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_nv12_64z32() {
            init();
            check_profile(VideoFormat::Nv1264z32, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_a420_10be() {
            init();
            check_profile(VideoFormat::A42010be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_a420_10le() {
            init();
            check_profile(VideoFormat::A42010le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_a422_10be() {
            init();
            check_profile(VideoFormat::A42210be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_a422_10le() {
            init();
            check_profile(VideoFormat::A42210le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_a444_10be() {
            init();
            check_profile(VideoFormat::A44410be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_a444_10le() {
            init();
            check_profile(VideoFormat::A44410le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_nv61() {
            init();
            check_profile(VideoFormat::Nv61, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_p010_10be() {
            init();
            check_profile(VideoFormat::P01010be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_p010_10le() {
            init();
            check_profile(VideoFormat::P01010le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_iyu2() {
            init();
            check_profile(VideoFormat::Iyu2, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_vyuy() {
            init();
            check_profile(VideoFormat::Vyuy, "vyuy".as_bytes());
        }

        #[test]
        fn profile_4cc_gbra() {
            init();
            check_profile(VideoFormat::Gbra, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_gbra_10be() {
            init();
            check_profile(VideoFormat::Gbra10be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_gbra_10le() {
            init();
            check_profile(VideoFormat::Gbra10le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_gbr_12be() {
            init();
            check_profile(VideoFormat::Gbr12be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_gbr_12le() {
            init();
            check_profile(VideoFormat::Gbr12le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_gbra_12be() {
            init();
            check_profile(VideoFormat::Gbra12be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_gbra_12le() {
            init();
            check_profile(VideoFormat::Gbra12le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_i420_12be() {
            init();
            check_profile(VideoFormat::I42012be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_i420_12le() {
            init();
            check_profile(VideoFormat::I42012le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_i422_12be() {
            init();
            check_profile(VideoFormat::I42212be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_i422_12le() {
            init();
            check_profile(VideoFormat::I42212le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y444_12be() {
            init();
            check_profile(VideoFormat::Y44412be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y444_12le() {
            init();
            check_profile(VideoFormat::Y44412le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_gray10_le32() {
            init();
            check_profile(VideoFormat::Gray10Le32, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_nv12_10le32() {
            init();
            check_profile(VideoFormat::Nv1210le32, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_nv16_10le32() {
            init();
            check_profile(VideoFormat::Nv1610le32, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_nv12_10le40() {
            init();
            check_profile(VideoFormat::Nv1210le40, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y210() {
            init();
            check_profile(VideoFormat::Y210, "y210".as_bytes());
        }

        #[test]
        fn profile_4cc_y410() {
            init();
            check_profile(VideoFormat::Y410, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_vuya() {
            init();
            check_profile(VideoFormat::Vuya, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_bgra10a2_le() {
            init();
            check_profile(VideoFormat::Bgr10a2Le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_rgb10a2_le() {
            init();
            check_profile(VideoFormat::Rgb10a2Le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y444_16be() {
            init();
            check_profile(VideoFormat::Y44416be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y444_16le() {
            init();
            check_profile(VideoFormat::Y44416le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_p016be() {
            init();
            check_profile(VideoFormat::P016Be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_p016le() {
            init();
            check_profile(VideoFormat::P016Le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_p012be() {
            init();
            check_profile(VideoFormat::P012Be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_p012le() {
            init();
            check_profile(VideoFormat::P012Le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y212be() {
            init();
            check_profile(VideoFormat::Y212Be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y212le() {
            init();
            check_profile(VideoFormat::Y212Le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y412be() {
            init();
            check_profile(VideoFormat::Y412Be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_y412le() {
            init();
            check_profile(VideoFormat::Y412Le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_nv12_4l4() {
            init();
            check_profile(VideoFormat::Nv124l4, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_nv12_32l32() {
            init();
            check_profile(VideoFormat::Nv1232l32, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_rgbp() {
            init();
            check_profile(VideoFormat::Rgbp, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_bgrp() {
            init();
            check_profile(VideoFormat::Bgrp, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_av12() {
            init();
            check_profile(VideoFormat::Av12, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_argb64_le() {
            init();
            check_profile(VideoFormat::Argb64Le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_argb64_be() {
            init();
            check_profile(VideoFormat::Argb64Be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_rgba64_le() {
            init();
            check_profile(VideoFormat::Rgba64Le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_rgba64_be() {
            init();
            check_profile(VideoFormat::Rgba64Be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_bgra64_le() {
            init();
            check_profile(VideoFormat::Bgra64Le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_bgra64_be() {
            init();
            check_profile(VideoFormat::Bgra64Be, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_abgr64_le() {
            init();
            check_profile(VideoFormat::Abgr64Le, &[0u8, 0u8, 0u8, 0u8]);
        }

        #[test]
        fn profile_4cc_abgr64_be() {
            init();
            check_profile(VideoFormat::Abgr64Be, &[0u8, 0u8, 0u8, 0u8]);
        }

        // TODO: NV12_16L32S (110) and after - requires gst-video 1.22
    }
}
