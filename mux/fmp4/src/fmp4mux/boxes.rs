// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::fmp4mux::imp::CAT;
use gst::prelude::*;

use anyhow::{anyhow, bail, Context, Error};
use std::convert::TryFrom;

use super::{transform_matrix, Buffer, IDENTITY_MATRIX};

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

fn cmaf_brands_from_caps(caps: &gst::CapsRef, compatible_brands: &mut Vec<&'static [u8; 4]>) {
    let s = caps.structure(0).unwrap();
    match s.name().as_str() {
        "video/x-h264" => {
            let width = s.get::<i32>("width").ok();
            let height = s.get::<i32>("height").ok();
            let fps = s.get::<gst::Fraction>("framerate").ok();
            let profile = s.get::<&str>("profile").ok();
            let level = s
                .get::<&str>("level")
                .ok()
                .map(|l| l.split_once('.').unwrap_or((l, "0")));
            let colorimetry = s.get::<&str>("colorimetry").ok();

            if let (Some(width), Some(height), Some(profile), Some(level), Some(fps)) =
                (width, height, profile, level, fps)
            {
                if profile == "high"
                    || profile == "main"
                    || profile == "baseline"
                    || profile == "constrained-baseline"
                {
                    if width <= 864
                        && height <= 576
                        && level <= ("3", "1")
                        && fps <= gst::Fraction::new(60, 1)
                    {
                        if let Some(colorimetry) =
                            colorimetry.and_then(|c| c.parse::<gst_video::VideoColorimetry>().ok())
                        {
                            if matches!(
                                colorimetry.primaries(),
                                gst_video::VideoColorPrimaries::Bt709
                                    | gst_video::VideoColorPrimaries::Bt470bg
                                    | gst_video::VideoColorPrimaries::Smpte170m
                            ) && matches!(
                                colorimetry.transfer(),
                                gst_video::VideoTransferFunction::Bt709
                                    | gst_video::VideoTransferFunction::Bt601
                            ) && matches!(
                                colorimetry.matrix(),
                                gst_video::VideoColorMatrix::Bt709
                                    | gst_video::VideoColorMatrix::Bt601
                            ) {
                                compatible_brands.push(b"cfsd");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.push(b"cfsd");
                        }
                    } else if width <= 1920
                        && height <= 1080
                        && level <= ("4", "0")
                        && fps <= gst::Fraction::new(60, 1)
                    {
                        if let Some(colorimetry) =
                            colorimetry.and_then(|c| c.parse::<gst_video::VideoColorimetry>().ok())
                        {
                            if matches!(
                                colorimetry.primaries(),
                                gst_video::VideoColorPrimaries::Bt709
                            ) && matches!(
                                colorimetry.transfer(),
                                gst_video::VideoTransferFunction::Bt709
                            ) && matches!(
                                colorimetry.matrix(),
                                gst_video::VideoColorMatrix::Bt709
                            ) {
                                compatible_brands.push(b"cfhd");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.push(b"cfhd");
                        }
                    } else if width <= 1920
                        && height <= 1080
                        && level <= ("4", "2")
                        && fps <= gst::Fraction::new(60, 1)
                    {
                        if let Some(colorimetry) =
                            colorimetry.and_then(|c| c.parse::<gst_video::VideoColorimetry>().ok())
                        {
                            if matches!(
                                colorimetry.primaries(),
                                gst_video::VideoColorPrimaries::Bt709
                            ) && matches!(
                                colorimetry.transfer(),
                                gst_video::VideoTransferFunction::Bt709
                            ) && matches!(
                                colorimetry.matrix(),
                                gst_video::VideoColorMatrix::Bt709
                            ) {
                                compatible_brands.push(b"chdf");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.push(b"chdf");
                        }
                    }
                }
            }
        }
        "audio/mpeg" => {
            compatible_brands.push(b"caac");
        }
        "audio/x-opus" => {
            compatible_brands.push(b"opus");
        }
        "video/x-av1" => {
            compatible_brands.push(b"av01");
            compatible_brands.push(b"cmf2");
        }
        "video/x-h265" => {
            let width = s.get::<i32>("width").ok();
            let height = s.get::<i32>("height").ok();
            let fps = s.get::<gst::Fraction>("framerate").ok();
            let profile = s.get::<&str>("profile").ok();
            let tier = s.get::<&str>("tier").ok();
            let level = s
                .get::<&str>("level")
                .ok()
                .map(|l| l.split_once('.').unwrap_or((l, "0")));
            let colorimetry = s.get::<&str>("colorimetry").ok();

            if let (Some(width), Some(height), Some(profile), Some(tier), Some(level), Some(fps)) =
                (width, height, profile, tier, level, fps)
            {
                if profile == "main" && tier == "main" {
                    if width <= 1920
                        && height <= 1080
                        && level <= ("4", "1")
                        && fps <= gst::Fraction::new(60, 1)
                    {
                        if let Some(colorimetry) =
                            colorimetry.and_then(|c| c.parse::<gst_video::VideoColorimetry>().ok())
                        {
                            if matches!(
                                colorimetry.primaries(),
                                gst_video::VideoColorPrimaries::Bt709
                            ) && matches!(
                                colorimetry.transfer(),
                                gst_video::VideoTransferFunction::Bt709
                            ) && matches!(
                                colorimetry.matrix(),
                                gst_video::VideoColorMatrix::Bt709
                            ) {
                                compatible_brands.push(b"chhd");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.push(b"chhd");
                        }
                    } else if width <= 3840
                        && height <= 2160
                        && level <= ("5", "0")
                        && fps <= gst::Fraction::new(60, 1)
                    {
                        if let Some(colorimetry) =
                            colorimetry.and_then(|c| c.parse::<gst_video::VideoColorimetry>().ok())
                        {
                            if matches!(
                                colorimetry.primaries(),
                                gst_video::VideoColorPrimaries::Bt709
                            ) && matches!(
                                colorimetry.transfer(),
                                gst_video::VideoTransferFunction::Bt709
                            ) && matches!(
                                colorimetry.matrix(),
                                gst_video::VideoColorMatrix::Bt709
                            ) {
                                compatible_brands.push(b"cud8");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.push(b"cud8");
                        }
                    }
                } else if profile == "main-10" && tier == "main-10" {
                    if width <= 1920
                        && height <= 1080
                        && level <= ("4", "1")
                        && fps <= gst::Fraction::new(60, 1)
                    {
                        if let Some(colorimetry) =
                            colorimetry.and_then(|c| c.parse::<gst_video::VideoColorimetry>().ok())
                        {
                            if matches!(
                                colorimetry.primaries(),
                                gst_video::VideoColorPrimaries::Bt709
                            ) && matches!(
                                colorimetry.transfer(),
                                gst_video::VideoTransferFunction::Bt709
                            ) && matches!(
                                colorimetry.matrix(),
                                gst_video::VideoColorMatrix::Bt709
                            ) {
                                compatible_brands.push(b"chh1");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.push(b"chh1");
                        }
                    } else if width <= 3840
                        && height <= 2160
                        && level <= ("5", "1")
                        && fps <= gst::Fraction::new(60, 1)
                    {
                        if let Some(colorimetry) =
                            colorimetry.and_then(|c| c.parse::<gst_video::VideoColorimetry>().ok())
                        {
                            if matches!(
                                colorimetry.primaries(),
                                gst_video::VideoColorPrimaries::Bt709
                                    | gst_video::VideoColorPrimaries::Bt2020
                            ) && matches!(
                                colorimetry.transfer(),
                                gst_video::VideoTransferFunction::Bt709
                                    | gst_video::VideoTransferFunction::Bt202010
                                    | gst_video::VideoTransferFunction::Bt202012
                            ) && matches!(
                                colorimetry.matrix(),
                                gst_video::VideoColorMatrix::Bt709
                                    | gst_video::VideoColorMatrix::Bt2020
                            ) {
                                compatible_brands.push(b"cud1");
                            } else if matches!(
                                colorimetry.primaries(),
                                gst_video::VideoColorPrimaries::Bt2020
                            ) && matches!(
                                colorimetry.transfer(),
                                gst_video::VideoTransferFunction::Smpte2084
                            ) && matches!(
                                colorimetry.matrix(),
                                gst_video::VideoColorMatrix::Bt2020
                            ) {
                                compatible_brands.push(b"chd1");
                            } else if matches!(
                                colorimetry.primaries(),
                                gst_video::VideoColorPrimaries::Bt2020
                            ) && matches!(
                                colorimetry.transfer(),
                                gst_video::VideoTransferFunction::AribStdB67
                            ) && matches!(
                                colorimetry.matrix(),
                                gst_video::VideoColorMatrix::Bt2020
                            ) {
                                compatible_brands.push(b"clg1");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.push(b"cud1");
                        }
                    }
                }
            }
        }
        _ => (),
    }
}

fn brands_from_variant_and_caps<'a>(
    variant: super::Variant,
    mut caps: impl Iterator<Item = &'a gst::Caps>,
) -> (&'static [u8; 4], Vec<&'static [u8; 4]>) {
    match variant {
        super::Variant::ISO | super::Variant::ONVIF => (b"iso6", vec![b"iso6"]),
        super::Variant::DASH => {
            // FIXME: `dsms` / `dash` brands, `msix`
            (b"msdh", vec![b"dums", b"msdh", b"iso6"])
        }
        super::Variant::CMAF => {
            let mut compatible_brands = vec![b"iso6", b"cmfc"];

            cmaf_brands_from_caps(caps.next().unwrap(), &mut compatible_brands);
            assert_eq!(caps.next(), None);

            (b"cmf2", compatible_brands)
        }
    }
}

/// Creates `ftyp` and `moov` boxes
pub(super) fn create_fmp4_header(cfg: super::HeaderConfiguration) -> Result<gst::Buffer, Error> {
    let mut v = vec![];

    let (brand, compatible_brands) =
        brands_from_variant_and_caps(cfg.variant, cfg.streams.iter().map(|s| &s.caps));

    write_box(&mut v, b"ftyp", |v| {
        // major brand
        v.extend(brand);
        // minor version
        v.extend(0u32.to_be_bytes());
        // compatible brands
        v.extend(compatible_brands.into_iter().flatten());

        Ok(())
    })?;

    write_box(&mut v, b"moov", |v| write_moov(v, &cfg))?;

    if cfg.variant == super::Variant::ONVIF {
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
                    v.extend(cfg.start_utc_time.unwrap().to_be_bytes());

                    Ok(())
                })
            },
        )?;
    }

    Ok(gst::Buffer::from_mut_slice(v))
}

fn write_moov(v: &mut Vec<u8>, cfg: &super::HeaderConfiguration) -> Result<(), Error> {
    use gst::glib;

    let base = glib::DateTime::from_utc(1904, 1, 1, 0, 0, 0.0)?;
    let now = glib::DateTime::now_utc()?;
    let creation_time =
        u64::try_from(now.difference(&base).as_seconds()).expect("time before 1904");

    write_full_box(v, b"mvhd", FULL_BOX_VERSION_1, FULL_BOX_FLAGS_NONE, |v| {
        write_mvhd(v, cfg, creation_time)
    })?;
    for (idx, stream) in cfg.streams.iter().enumerate() {
        write_box(v, b"trak", |v| {
            let mut references = vec![];

            // Reference the video track for ONVIF metadata tracks
            if cfg.variant == super::Variant::ONVIF
                && stream.caps.structure(0).unwrap().name() == "application/x-onvif-metadata"
            {
                // Find the first video track
                for (idx, other_stream) in cfg.streams.iter().enumerate() {
                    let s = other_stream.caps.structure(0).unwrap();

                    if matches!(
                        s.name().as_str(),
                        "video/x-h264" | "video/x-h265" | "image/jpeg"
                    ) {
                        references.push(TrackReference {
                            reference_type: *b"cdsc",
                            track_ids: vec![idx as u32 + 1],
                        });
                        break;
                    }
                }
            }

            write_trak(v, cfg, idx, stream, creation_time, &references)
        })?;
    }
    write_box(v, b"mvex", |v| write_mvex(v, cfg))?;

    Ok(())
}

fn caps_to_timescale(caps: &gst::CapsRef) -> u32 {
    let s = caps.structure(0).unwrap();

    if let Ok(fps) = s.get::<gst::Fraction>("framerate") {
        if fps.numer() == 0 {
            return 10_000;
        }

        if fps.denom() != 1 && fps.denom() != 1001 {
            if let Some(fps) = (fps.denom() as u64)
                .nseconds()
                .mul_div_round(1_000_000_000, fps.numer() as u64)
                .and_then(gst_video::guess_framerate)
            {
                return (fps.numer() as u32)
                    .mul_div_round(100, fps.denom() as u32)
                    .unwrap_or(10_000);
            }
        }

        if fps.denom() == 1001 {
            fps.numer() as u32
        } else {
            (fps.numer() as u32)
                .mul_div_round(100, fps.denom() as u32)
                .unwrap_or(10_000)
        }
    } else if let Ok(rate) = s.get::<i32>("rate") {
        rate as u32
    } else {
        10_000
    }
}

fn header_stream_to_timescale(stream: &super::HeaderStream) -> u32 {
    if stream.trak_timescale > 0 {
        stream.trak_timescale
    } else {
        caps_to_timescale(&stream.caps)
    }
}

fn header_configuration_to_timescale(cfg: &super::HeaderConfiguration) -> u32 {
    if cfg.movie_timescale > 0 {
        cfg.movie_timescale
    } else {
        // Use the reference track timescale
        header_stream_to_timescale(&cfg.streams[0])
    }
}

fn fragment_header_stream_to_timescale(stream: &super::FragmentHeaderStream) -> u32 {
    if stream.trak_timescale > 0 {
        stream.trak_timescale
    } else {
        caps_to_timescale(&stream.caps)
    }
}

fn write_mvhd(
    v: &mut Vec<u8>,
    cfg: &super::HeaderConfiguration,
    creation_time: u64,
) -> Result<(), Error> {
    // Creation time
    v.extend(creation_time.to_be_bytes());
    // Modification time
    v.extend(creation_time.to_be_bytes());
    // Timescale
    v.extend(header_configuration_to_timescale(cfg).to_be_bytes());
    // Duration
    v.extend(0u64.to_be_bytes());

    // Rate 1.0
    v.extend((1u32 << 16).to_be_bytes());
    // Volume 1.0
    v.extend((1u16 << 8).to_be_bytes());
    // Reserved
    v.extend([0u8; 2 + 2 * 4]);

    // Matrix
    let matrix = transform_matrix(&cfg.orientation);
    v.extend(matrix.iter().flatten());

    // Pre defined
    v.extend([0u8; 6 * 4]);

    // Next track id
    v.extend((cfg.streams.len() as u32 + 1).to_be_bytes());

    Ok(())
}

const TKHD_FLAGS_TRACK_ENABLED: u32 = 0x1;
const TKHD_FLAGS_TRACK_IN_MOVIE: u32 = 0x2;
const TKHD_FLAGS_TRACK_IN_PREVIEW: u32 = 0x4;

struct TrackReference {
    reference_type: [u8; 4],
    track_ids: Vec<u32>,
}

fn write_edts(v: &mut Vec<u8>, stream: &super::HeaderStream) -> Result<(), Error> {
    write_full_box(v, b"elst", FULL_BOX_VERSION_1, 0, |v| write_elst(v, stream))?;

    Ok(())
}

fn write_elst(v: &mut Vec<u8>, stream: &super::HeaderStream) -> Result<(), Error> {
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

fn write_trak(
    v: &mut Vec<u8>,
    cfg: &super::HeaderConfiguration,
    idx: usize,
    stream: &super::HeaderStream,
    creation_time: u64,
    references: &[TrackReference],
) -> Result<(), Error> {
    write_full_box(
        v,
        b"tkhd",
        FULL_BOX_VERSION_1,
        TKHD_FLAGS_TRACK_ENABLED | TKHD_FLAGS_TRACK_IN_MOVIE | TKHD_FLAGS_TRACK_IN_PREVIEW,
        |v| write_tkhd(v, idx, stream, creation_time),
    )?;

    // TODO: write edts optionally for negative DTS instead of offsetting the DTS
    write_box(v, b"mdia", |v| write_mdia(v, cfg, stream, creation_time))?;
    if !stream.elst_infos.is_empty() && cfg.write_edts {
        if let Err(e) = write_edts(v, stream) {
            gst::warning!(CAT, "Failed to write edts: {e}");
        }
    }

    if !references.is_empty() {
        write_box(v, b"tref", |v| write_tref(v, cfg, references))?;
    }

    Ok(())
}

fn write_tkhd(
    v: &mut Vec<u8>,
    idx: usize,
    stream: &super::HeaderStream,
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
    v.extend(0u64.to_be_bytes());

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
        x if x.starts_with("video/") || x.starts_with("image/") => {
            transform_matrix(&stream.orientation)
        }
        _ => &IDENTITY_MATRIX,
    };

    v.extend(matrix.iter().flatten());

    // Width/height
    match s.name().as_str() {
        "video/x-h264" | "video/x-h265" | "video/x-vp8" | "video/x-vp9" | "video/x-av1"
        | "image/jpeg" => {
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
    cfg: &super::HeaderConfiguration,
    stream: &super::HeaderStream,
    creation_time: u64,
) -> Result<(), Error> {
    write_full_box(v, b"mdhd", FULL_BOX_VERSION_1, FULL_BOX_FLAGS_NONE, |v| {
        write_mdhd(v, stream, creation_time)
    })?;
    write_full_box(v, b"hdlr", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_hdlr(v, cfg, stream)
    })?;

    // TODO: write elng if needed

    write_box(v, b"minf", |v| write_minf(v, cfg, stream))?;

    Ok(())
}

fn write_tref(
    v: &mut Vec<u8>,
    _cfg: &super::HeaderConfiguration,
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

fn language_code(lang: impl std::borrow::Borrow<[u8; 3]>) -> u16 {
    let lang = lang.borrow();

    assert!(lang.iter().all(u8::is_ascii_lowercase));

    (((lang[0] as u16 - 0x60) & 0x1F) << 10)
        + (((lang[1] as u16 - 0x60) & 0x1F) << 5)
        + ((lang[2] as u16 - 0x60) & 0x1F)
}

fn write_mdhd(
    v: &mut Vec<u8>,
    stream: &super::HeaderStream,
    creation_time: u64,
) -> Result<(), Error> {
    // Creation time
    v.extend(creation_time.to_be_bytes());
    // Modification time
    v.extend(creation_time.to_be_bytes());
    // Timescale
    v.extend(header_stream_to_timescale(stream).to_be_bytes());
    // Duration
    v.extend(0u64.to_be_bytes());

    // Language as ISO-639-2/T
    if let Some(lang) = stream.language_code {
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
    _cfg: &super::HeaderConfiguration,
    stream: &super::HeaderStream,
) -> Result<(), Error> {
    // Pre-defined
    v.extend([0u8; 4]);

    let s = stream.caps.structure(0).unwrap();
    let (handler_type, name) = match s.name().as_str() {
        "video/x-h264" | "video/x-h265" | "video/x-vp8" | "video/x-vp9" | "video/x-av1"
        | "image/jpeg" => (b"vide", b"VideoHandler\0".as_slice()),
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
    cfg: &super::HeaderConfiguration,
    stream: &super::HeaderStream,
) -> Result<(), Error> {
    let s = stream.caps.structure(0).unwrap();

    match s.name().as_str() {
        "video/x-h264" | "video/x-h265" | "video/x-vp8" | "video/x-vp9" | "video/x-av1"
        | "image/jpeg" => {
            // Flags are always 1 for unspecified reasons
            write_full_box(v, b"vmhd", FULL_BOX_VERSION_0, 1, |v| write_vmhd(v, cfg))?
        }
        "audio/mpeg" | "audio/x-opus" | "audio/x-flac" | "audio/x-alaw" | "audio/x-mulaw"
        | "audio/x-adpcm" => {
            write_full_box(v, b"smhd", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
                write_smhd(v, cfg)
            })?
        }
        "application/x-onvif-metadata" => {
            write_full_box(v, b"nmhd", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |_v| {
                Ok(())
            })?
        }
        _ => unreachable!(),
    }

    write_box(v, b"dinf", |v| write_dinf(v, cfg))?;

    write_box(v, b"stbl", |v| write_stbl(v, cfg, stream))?;

    Ok(())
}

fn write_vmhd(v: &mut Vec<u8>, _cfg: &super::HeaderConfiguration) -> Result<(), Error> {
    // Graphics mode
    v.extend([0u8; 2]);

    // opcolor
    v.extend([0u8; 2 * 3]);

    Ok(())
}

fn write_smhd(v: &mut Vec<u8>, _cfg: &super::HeaderConfiguration) -> Result<(), Error> {
    // Balance
    v.extend([0u8; 2]);

    // Reserved
    v.extend([0u8; 2]);

    Ok(())
}

fn write_dinf(v: &mut Vec<u8>, cfg: &super::HeaderConfiguration) -> Result<(), Error> {
    write_full_box(v, b"dref", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_dref(v, cfg)
    })?;

    Ok(())
}

const DREF_FLAGS_MEDIA_IN_SAME_FILE: u32 = 0x1;

fn write_dref(v: &mut Vec<u8>, _cfg: &super::HeaderConfiguration) -> Result<(), Error> {
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
    cfg: &super::HeaderConfiguration,
    stream: &super::HeaderStream,
) -> Result<(), Error> {
    write_full_box(v, b"stsd", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_stsd(v, cfg, stream)
    })?;
    write_full_box(v, b"stts", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_stts(v, cfg)
    })?;
    write_full_box(v, b"stsc", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_stsc(v, cfg)
    })?;
    write_full_box(v, b"stsz", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_stsz(v, cfg)
    })?;

    write_full_box(v, b"stco", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_stco(v, cfg)
    })?;

    // For video write a sync sample box as indication that not all samples are sync samples
    if !stream.delta_frames.intra_only() {
        write_full_box(v, b"stss", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
            write_stss(v, cfg)
        })?
    }

    Ok(())
}

fn write_stsd(
    v: &mut Vec<u8>,
    cfg: &super::HeaderConfiguration,
    stream: &super::HeaderStream,
) -> Result<(), Error> {
    // Entry count
    v.extend(1u32.to_be_bytes());

    let s = stream.caps.structure(0).unwrap();
    match s.name().as_str() {
        "video/x-h264" | "video/x-h265" | "video/x-vp8" | "video/x-vp9" | "video/x-av1"
        | "image/jpeg" => write_visual_sample_entry(v, cfg, stream)?,
        "audio/mpeg" | "audio/x-opus" | "audio/x-flac" | "audio/x-alaw" | "audio/x-mulaw"
        | "audio/x-adpcm" => write_audio_sample_entry(v, cfg, stream)?,
        "application/x-onvif-metadata" => write_xml_meta_data_sample_entry(v, cfg, stream)?,
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
    _cfg: &super::HeaderConfiguration,
    stream: &super::HeaderStream,
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
                        // configOBUs
                        v.extend_from_slice(extra_data.as_slice());
                    }
                    Ok(())
                })?;
            }
            "video/x-vp8" | "image/jpeg" => {
                // Nothing to do here
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

        // TODO: write btrt bitrate box based on tags

        Ok(())
    })?;

    Ok(())
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
    _cfg: &super::HeaderConfiguration,
    stream: &super::HeaderStream,
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
            1 + (u16::from_be_bytes([streaminfo[16], streaminfo[17]]) >> 4 & 0b11111)
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
    _cfg: &super::HeaderConfiguration,
    stream: &super::HeaderStream,
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

fn write_stts(v: &mut Vec<u8>, _cfg: &super::HeaderConfiguration) -> Result<(), Error> {
    // Entry count
    v.extend(0u32.to_be_bytes());

    Ok(())
}

fn write_stsc(v: &mut Vec<u8>, _cfg: &super::HeaderConfiguration) -> Result<(), Error> {
    // Entry count
    v.extend(0u32.to_be_bytes());

    Ok(())
}

fn write_stsz(v: &mut Vec<u8>, _cfg: &super::HeaderConfiguration) -> Result<(), Error> {
    // Sample size
    v.extend(0u32.to_be_bytes());

    // Sample count
    v.extend(0u32.to_be_bytes());

    Ok(())
}

fn write_stco(v: &mut Vec<u8>, _cfg: &super::HeaderConfiguration) -> Result<(), Error> {
    // Entry count
    v.extend(0u32.to_be_bytes());

    Ok(())
}

fn write_stss(v: &mut Vec<u8>, _cfg: &super::HeaderConfiguration) -> Result<(), Error> {
    // Entry count
    v.extend(0u32.to_be_bytes());

    Ok(())
}

fn write_mvex(v: &mut Vec<u8>, cfg: &super::HeaderConfiguration) -> Result<(), Error> {
    if cfg.write_mehd {
        if cfg.update && cfg.duration.is_some() {
            write_full_box(v, b"mehd", FULL_BOX_VERSION_1, FULL_BOX_FLAGS_NONE, |v| {
                write_mehd(v, cfg)
            })?;
        } else {
            write_box(v, b"free", |v| {
                // version/flags of full box
                v.extend(0u32.to_be_bytes());
                // mehd duration
                v.extend(0u64.to_be_bytes());

                Ok(())
            })?;
        }
    }

    for (idx, _stream) in cfg.streams.iter().enumerate() {
        write_full_box(v, b"trex", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
            write_trex(v, cfg, idx)
        })?;
    }

    Ok(())
}

fn write_mehd(v: &mut Vec<u8>, cfg: &super::HeaderConfiguration) -> Result<(), Error> {
    // Use the reference track timescale
    let timescale = header_configuration_to_timescale(cfg);

    let duration = cfg
        .duration
        .expect("no duration")
        .mul_div_ceil(timescale as u64, gst::ClockTime::SECOND.nseconds())
        .context("too long duration")?;

    // Media duration in mvhd.timescale units
    v.extend(duration.to_be_bytes());

    Ok(())
}

fn write_trex(v: &mut Vec<u8>, _cfg: &super::HeaderConfiguration, idx: usize) -> Result<(), Error> {
    // Track ID
    v.extend((idx as u32 + 1).to_be_bytes());

    // Default sample description index
    v.extend(1u32.to_be_bytes());

    // Default sample duration
    v.extend(0u32.to_be_bytes());

    // Default sample size
    v.extend(0u32.to_be_bytes());

    // Default sample flags
    v.extend(0u32.to_be_bytes());

    // Default sample duration/size/etc will be provided in the traf/trun if one can be determined
    // for a whole fragment

    Ok(())
}

/// Creates `styp` and `moof` boxes and `mdat` header
pub(super) fn create_fmp4_fragment_header(
    cfg: super::FragmentHeaderConfiguration,
) -> Result<(gst::Buffer, u64), Error> {
    let mut v = vec![];

    // Don't write a `styp` if this is only a chunk.
    if !cfg.chunk {
        let (brand, compatible_brands) =
            brands_from_variant_and_caps(cfg.variant, cfg.streams.iter().map(|s| &s.caps));

        write_box(&mut v, b"styp", |v| {
            // major brand
            v.extend(brand);
            // minor version
            v.extend(0u32.to_be_bytes());
            // compatible brands
            v.extend(compatible_brands.into_iter().flatten());

            Ok(())
        })?;
    }

    let styp_len = v.len();

    let data_offset_offsets = write_box(&mut v, b"moof", |v| write_moof(v, &cfg))?;

    let size = cfg
        .buffers
        .iter()
        .map(|buffer| buffer.buffer.size() as u64)
        .sum::<u64>();
    if let Ok(size) = u32::try_from(size + 8) {
        v.extend(size.to_be_bytes());
        v.extend(b"mdat");
    } else {
        v.extend(1u32.to_be_bytes());
        v.extend(b"mdat");
        v.extend((size + 16).to_be_bytes());
    }

    let data_offset = v.len() - styp_len;
    for data_offset_offset in data_offset_offsets {
        let val = u32::from_be_bytes(v[data_offset_offset..][..4].try_into()?)
            .checked_add(u32::try_from(data_offset)?)
            .ok_or_else(|| anyhow!("can't calculate track run data offset"))?;
        v[data_offset_offset..][..4].copy_from_slice(&val.to_be_bytes());
    }

    Ok((gst::Buffer::from_mut_slice(v), styp_len as u64))
}

fn write_moof(
    v: &mut Vec<u8>,
    cfg: &super::FragmentHeaderConfiguration,
) -> Result<Vec<usize>, Error> {
    write_full_box(v, b"mfhd", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
        write_mfhd(v, cfg)
    })?;

    let mut data_offset_offsets = vec![];
    for (idx, stream) in cfg.streams.iter().enumerate() {
        // Skip tracks without any buffers for this fragment.
        if stream.start_time.is_none() {
            continue;
        }

        write_box(v, b"traf", |v| {
            write_traf(v, cfg, &mut data_offset_offsets, idx, stream)
        })?;
    }

    Ok(data_offset_offsets)
}

fn write_mfhd(v: &mut Vec<u8>, cfg: &super::FragmentHeaderConfiguration) -> Result<(), Error> {
    v.extend(cfg.sequence_number.to_be_bytes());

    Ok(())
}

#[allow(clippy::identity_op)]
#[allow(clippy::bool_to_int_with_if)]
fn sample_flags_from_buffer(stream: &super::FragmentHeaderStream, buffer: &gst::BufferRef) -> u32 {
    if stream.delta_frames.intra_only() {
        (0b00u32 << (16 + 10)) | // leading: unknown
        (0b10u32 << (16 + 8)) | // depends: no
        (0b10u32 << (16 + 6)) | // depended: no
        (0b00u32 << (16 + 4)) | // redundancy: unknown
        (0b000u32 << (16 + 1)) | // padding: no
        (0b0u32 << 16) | // non-sync-sample: no
        (0u32) // degradation priority
    } else {
        let depends = if buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            0b01u32
        } else {
            0b10u32
        };
        let depended = if buffer.flags().contains(gst::BufferFlags::DROPPABLE) {
            0b10u32
        } else {
            0b00u32
        };
        let non_sync_sample = if buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            0b1u32
        } else {
            0b0u32
        };

        (0b00u32 << (16 + 10)) | // leading: unknown
        (depends << (16 + 8)) | // depends
        (depended << (16 + 6)) | // depended
        (0b00u32 << (16 + 4)) | // redundancy: unknown
        (0b000u32 << (16 + 1)) | // padding: no
        (non_sync_sample << 16) | // non-sync-sample
        (0u32) // degradation priority
    }
}

const DEFAULT_SAMPLE_DURATION_PRESENT: u32 = 0x08;
const DEFAULT_SAMPLE_SIZE_PRESENT: u32 = 0x10;
const DEFAULT_SAMPLE_FLAGS_PRESENT: u32 = 0x20;
const DEFAULT_BASE_IS_MOOF: u32 = 0x2_00_00;

const DATA_OFFSET_PRESENT: u32 = 0x0_01;
const FIRST_SAMPLE_FLAGS_PRESENT: u32 = 0x0_04;
const SAMPLE_DURATION_PRESENT: u32 = 0x1_00;
const SAMPLE_SIZE_PRESENT: u32 = 0x2_00;
const SAMPLE_FLAGS_PRESENT: u32 = 0x4_00;
const SAMPLE_COMPOSITION_TIME_OFFSET_PRESENT: u32 = 0x8_00;

#[allow(clippy::type_complexity)]
fn analyze_buffers(
    cfg: &super::FragmentHeaderConfiguration,
    idx: usize,
    stream: &super::FragmentHeaderStream,
    timescale: u32,
) -> Result<
    (
        // tf_flags
        u32,
        // tr_flags
        u32,
        // default size
        Option<u32>,
        // default duration
        Option<u32>,
        // default flags
        Option<u32>,
        // negative composition time offsets
        bool,
    ),
    Error,
> {
    let mut tf_flags = DEFAULT_BASE_IS_MOOF;
    let mut tr_flags = DATA_OFFSET_PRESENT;

    let mut duration = None;
    let mut size = None;
    let mut first_buffer_flags = None;
    let mut flags = None;

    let mut negative_composition_time_offsets = false;

    for Buffer {
        idx: _idx,
        buffer,
        timestamp: _timestamp,
        duration: sample_duration,
        composition_time_offset,
    } in cfg.buffers.iter().filter(|b| b.idx == idx)
    {
        if size.is_none() {
            size = Some(buffer.size() as u32);
        }
        if Some(buffer.size() as u32) != size {
            tr_flags |= SAMPLE_SIZE_PRESENT;
        }

        let sample_duration = u32::try_from(
            sample_duration
                .nseconds()
                .mul_div_round(timescale as u64, gst::ClockTime::SECOND.nseconds())
                .context("too big sample duration")?,
        )
        .context("too big sample duration")?;

        if duration.is_none() {
            duration = Some(sample_duration);
        }
        if Some(sample_duration) != duration {
            tr_flags |= SAMPLE_DURATION_PRESENT;
        }

        let f = sample_flags_from_buffer(stream, buffer);
        if first_buffer_flags.is_none() {
            // First buffer, remember as first buffer flags
            first_buffer_flags = Some(f);
        } else if flags.is_none() {
            // Second buffer, remember as general flags and if they're
            // different from the first buffer's flags then also remember
            // that
            flags = Some(f);
            if Some(f) != first_buffer_flags {
                tr_flags |= FIRST_SAMPLE_FLAGS_PRESENT;
            }
        } else if Some(f) != flags {
            // Third or later buffer, and the flags are different than the second buffer's flags.
            // In that case each sample will have to store its own flags.
            tr_flags &= !FIRST_SAMPLE_FLAGS_PRESENT;
            tr_flags |= SAMPLE_FLAGS_PRESENT;
        }

        if let Some(composition_time_offset) = *composition_time_offset {
            assert!(stream.delta_frames.requires_dts());
            if composition_time_offset != 0 {
                tr_flags |= SAMPLE_COMPOSITION_TIME_OFFSET_PRESENT;
            }
            if composition_time_offset < 0 {
                negative_composition_time_offsets = true;
            }
        }
    }

    if (tr_flags & SAMPLE_SIZE_PRESENT) == 0 {
        tf_flags |= DEFAULT_SAMPLE_SIZE_PRESENT;
    } else {
        size = None;
    }

    if (tr_flags & SAMPLE_DURATION_PRESENT) == 0 {
        tf_flags |= DEFAULT_SAMPLE_DURATION_PRESENT;
    } else {
        duration = None;
    }

    // If there is only a single buffer use its flags as default sample flags
    // instead of first sample flags.
    if flags.is_none() && first_buffer_flags.is_some() {
        tr_flags &= !FIRST_SAMPLE_FLAGS_PRESENT;
        flags = first_buffer_flags.take();
    }

    // If all but possibly the first buffer had the same flags then only store them once instead of
    // with every single sample.
    if (tr_flags & SAMPLE_FLAGS_PRESENT) == 0 {
        tf_flags |= DEFAULT_SAMPLE_FLAGS_PRESENT;
    } else {
        flags = None;
    }

    Ok((
        tf_flags,
        tr_flags,
        size,
        duration,
        flags,
        negative_composition_time_offsets,
    ))
}

#[allow(clippy::ptr_arg)]
fn write_traf(
    v: &mut Vec<u8>,
    cfg: &super::FragmentHeaderConfiguration,
    data_offset_offsets: &mut Vec<usize>,
    idx: usize,
    stream: &super::FragmentHeaderStream,
) -> Result<(), Error> {
    let timescale = fragment_header_stream_to_timescale(stream);

    // Analyze all buffers to know what values can be put into the tfhd for all samples and what
    // has to be stored for every single sample
    let (
        tf_flags,
        mut tr_flags,
        default_size,
        default_duration,
        default_flags,
        negative_composition_time_offsets,
    ) = analyze_buffers(cfg, idx, stream, timescale)?;

    assert!((tf_flags & DEFAULT_SAMPLE_SIZE_PRESENT == 0) ^ default_size.is_some());
    assert!((tf_flags & DEFAULT_SAMPLE_DURATION_PRESENT == 0) ^ default_duration.is_some());
    assert!((tf_flags & DEFAULT_SAMPLE_FLAGS_PRESENT == 0) ^ default_flags.is_some());

    write_full_box(v, b"tfhd", FULL_BOX_VERSION_0, tf_flags, |v| {
        write_tfhd(v, cfg, idx, default_size, default_duration, default_flags)
    })?;
    write_full_box(v, b"tfdt", FULL_BOX_VERSION_1, FULL_BOX_FLAGS_NONE, |v| {
        write_tfdt(v, cfg, idx, stream, timescale)
    })?;

    let mut current_data_offset = 0;

    for run in GroupBy::new(cfg.buffers, |a: &Buffer, b: &Buffer| a.idx == b.idx) {
        if run[0].idx != idx {
            // FIXME: What to do with >4GB offsets?
            current_data_offset = (current_data_offset as u64
                + run.iter().map(|b| b.buffer.size() as u64).sum::<u64>())
            .try_into()?;
            continue;
        }

        let data_offset_offset = write_full_box(
            v,
            b"trun",
            if negative_composition_time_offsets {
                FULL_BOX_VERSION_1
            } else {
                FULL_BOX_VERSION_0
            },
            tr_flags,
            |v| {
                write_trun(
                    v,
                    cfg,
                    current_data_offset,
                    tr_flags,
                    timescale,
                    stream,
                    run,
                )
            },
        )?;
        data_offset_offsets.push(data_offset_offset);

        // FIXME: What to do with >4GB offsets?
        current_data_offset = (current_data_offset as u64
            + run.iter().map(|b| b.buffer.size() as u64).sum::<u64>())
        .try_into()?;

        // Don't include first sample flags in any trun boxes except for the first
        tr_flags &= !FIRST_SAMPLE_FLAGS_PRESENT;
    }

    // TODO: saio, saiz, sbgp, sgpd, subs?

    Ok(())
}

fn write_tfhd(
    v: &mut Vec<u8>,
    _cfg: &super::FragmentHeaderConfiguration,
    idx: usize,
    default_size: Option<u32>,
    default_duration: Option<u32>,
    default_flags: Option<u32>,
) -> Result<(), Error> {
    // Track ID
    v.extend((idx as u32 + 1).to_be_bytes());

    // No base data offset, no sample description index

    if let Some(default_duration) = default_duration {
        v.extend(default_duration.to_be_bytes());
    }

    if let Some(default_size) = default_size {
        v.extend(default_size.to_be_bytes());
    }

    if let Some(default_flags) = default_flags {
        v.extend(default_flags.to_be_bytes());
    }

    Ok(())
}

fn write_tfdt(
    v: &mut Vec<u8>,
    _cfg: &super::FragmentHeaderConfiguration,
    _idx: usize,
    stream: &super::FragmentHeaderStream,
    timescale: u32,
) -> Result<(), Error> {
    let base_time = stream
        .start_time
        .unwrap()
        .mul_div_floor(timescale as u64, gst::ClockTime::SECOND.nseconds())
        .context("base time overflow")?;

    v.extend(base_time.to_be_bytes());

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_trun(
    v: &mut Vec<u8>,
    _cfg: &super::FragmentHeaderConfiguration,
    current_data_offset: u32,
    tr_flags: u32,
    timescale: u32,
    stream: &super::FragmentHeaderStream,
    buffers: &[Buffer],
) -> Result<usize, Error> {
    // Sample count
    v.extend((buffers.len() as u32).to_be_bytes());

    let data_offset_offset = v.len();
    // Data offset, will be rewritten later
    v.extend(current_data_offset.to_be_bytes());

    if (tr_flags & FIRST_SAMPLE_FLAGS_PRESENT) != 0 {
        v.extend(sample_flags_from_buffer(stream, &buffers[0].buffer).to_be_bytes());
    }

    for Buffer {
        idx: _idx,
        ref buffer,
        timestamp: _timestamp,
        duration,
        composition_time_offset,
    } in buffers.iter()
    {
        if (tr_flags & SAMPLE_DURATION_PRESENT) != 0 {
            // Sample duration
            let sample_duration = u32::try_from(
                duration
                    .nseconds()
                    .mul_div_round(timescale as u64, gst::ClockTime::SECOND.nseconds())
                    .context("too big sample duration")?,
            )
            .context("too big sample duration")?;
            v.extend(sample_duration.to_be_bytes());
        }

        if (tr_flags & SAMPLE_SIZE_PRESENT) != 0 {
            // Sample size
            v.extend((buffer.size() as u32).to_be_bytes());
        }

        if (tr_flags & SAMPLE_FLAGS_PRESENT) != 0 {
            assert!((tr_flags & FIRST_SAMPLE_FLAGS_PRESENT) == 0);

            // Sample flags
            v.extend(sample_flags_from_buffer(stream, buffer).to_be_bytes());
        }

        if (tr_flags & SAMPLE_COMPOSITION_TIME_OFFSET_PRESENT) != 0 {
            // Sample composition time offset
            let composition_time_offset = i32::try_from(
                composition_time_offset
                    .unwrap_or(0)
                    .mul_div_round(timescale as i64, gst::ClockTime::SECOND.nseconds() as i64)
                    .context("too big composition time offset")?,
            )
            .context("too big composition time offset")?;
            v.extend(composition_time_offset.to_be_bytes());
        }
    }

    Ok(data_offset_offset)
}

/// Creates `mfra` box
pub(crate) fn create_mfra(
    caps: &gst::CapsRef,
    fragment_offsets: &[super::FragmentOffset],
) -> Result<gst::Buffer, Error> {
    let timescale = caps_to_timescale(caps);

    let mut v = vec![];

    let offset = write_box(&mut v, b"mfra", |v| {
        write_full_box(v, b"tfra", FULL_BOX_VERSION_1, FULL_BOX_FLAGS_NONE, |v| {
            // Track ID
            v.extend(1u32.to_be_bytes());

            // Reserved / length of traf/trun/sample
            v.extend(0u32.to_be_bytes());

            // Number of entries
            v.extend(
                u32::try_from(fragment_offsets.len())
                    .context("too many fragments")?
                    .to_be_bytes(),
            );

            for super::FragmentOffset { time, offset } in fragment_offsets {
                // Time
                let time = time
                    .nseconds()
                    .mul_div_round(timescale as u64, gst::ClockTime::SECOND.nseconds())
                    .context("time overflow")?;
                v.extend(time.to_be_bytes());

                // moof offset
                v.extend(offset.to_be_bytes());

                // traf/trun/sample number
                v.extend_from_slice(&[1u8; 3][..]);
            }

            Ok(())
        })?;

        let offset = write_full_box(v, b"mfro", FULL_BOX_VERSION_0, FULL_BOX_FLAGS_NONE, |v| {
            let offset = v.len();
            // Parent size
            v.extend(0u32.to_be_bytes());
            Ok(offset)
        })?;

        Ok(offset)
    })?;

    let len = u32::try_from(v.len() as u64).context("too big mfra")?;
    v[offset..][..4].copy_from_slice(&len.to_be_bytes());

    Ok(gst::Buffer::from_mut_slice(v))
}

// Copy from std while this is still nightly-only
use std::{fmt, str::FromStr};

/// An iterator over slice in (non-overlapping) chunks separated by a predicate.
///
/// This struct is created by the [`group_by`] method on [slices].
///
/// [`group_by`]: slice::group_by
/// [slices]: slice
struct GroupBy<'a, T: 'a, P> {
    slice: &'a [T],
    predicate: P,
}

impl<'a, T: 'a, P> GroupBy<'a, T, P> {
    fn new(slice: &'a [T], predicate: P) -> Self {
        GroupBy { slice, predicate }
    }
}

impl<'a, T: 'a, P> Iterator for GroupBy<'a, T, P>
where
    P: FnMut(&T, &T) -> bool,
{
    type Item = &'a [T];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.slice.is_empty() {
            None
        } else {
            let mut len = 1;
            let mut iter = self.slice.windows(2);
            while let Some([l, r]) = iter.next() {
                if (self.predicate)(l, r) {
                    len += 1
                } else {
                    break;
                }
            }
            let (head, tail) = self.slice.split_at(len);
            self.slice = tail;
            Some(head)
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.slice.is_empty() {
            (0, Some(0))
        } else {
            (1, Some(self.slice.len()))
        }
    }

    #[inline]
    fn last(mut self) -> Option<Self::Item> {
        self.next_back()
    }
}

impl<'a, T: 'a, P> DoubleEndedIterator for GroupBy<'a, T, P>
where
    P: FnMut(&T, &T) -> bool,
{
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.slice.is_empty() {
            None
        } else {
            let mut len = 1;
            let mut iter = self.slice.windows(2);
            while let Some([l, r]) = iter.next_back() {
                if (self.predicate)(l, r) {
                    len += 1
                } else {
                    break;
                }
            }
            let (head, tail) = self.slice.split_at(self.slice.len() - len);
            self.slice = head;
            Some(tail)
        }
    }
}

impl<'a, T: 'a, P> std::iter::FusedIterator for GroupBy<'a, T, P> where P: FnMut(&T, &T) -> bool {}

impl<'a, T: 'a + fmt::Debug, P> fmt::Debug for GroupBy<'a, T, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GroupBy")
            .field("slice", &self.slice)
            .finish()
    }
}
