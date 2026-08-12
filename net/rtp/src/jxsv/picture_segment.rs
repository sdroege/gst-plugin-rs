// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use anyhow::{Context as _, bail};
use gst::prelude::MulDiv;
use std::str::FromStr;

use super::profile_level::ProfileLevel;

const BOX_HEADER_SIZE: usize = 8;
const EXTENDED_BOX_HEADER_SIZE: usize = 16;
const JPEG_XS_SOC: [u8; 2] = [0xff, 0x10];
const MARKER_CAP: [u8; 2] = [0xff, 0x50];
const MARKER_PIH: [u8; 2] = [0xff, 0x12];

const VIDEO_SUPPORT_BOX_TYPE: [u8; 4] = *b"jpvs";
const VIDEO_INFORMATION_BOX_TYPE: [u8; 4] = *b"jpvi";
const PROFILE_LEVEL_BOX_TYPE: [u8; 4] = *b"jxpl";
const COLOUR_SPECIFICATION_BOX_TYPE: [u8; 4] = *b"colr";

const JPVI_BOX_SIZE: u32 = 22;
const JXPL_BOX_SIZE: u32 = 12;
const JPVS_BOX_SIZE: u32 = BOX_HEADER_SIZE as u32 + JPVI_BOX_SIZE + JXPL_BOX_SIZE;
const COLR_BOX_SIZE: u32 = 18;

/// Size of the RFC 9134 picture-segment prefix (`jpvs` + `colr`) before the codestream.
pub const PICTURE_SEGMENT_PREFIX_SIZE: usize = (JPVS_BOX_SIZE + COLR_BOX_SIZE) as usize;

const BRAT_OFFSET: usize = 16;
const TCOD_OFFSET: usize = 26;
const PPIH_OFFSET: usize = 38;
const PLEV_OFFSET: usize = 40;

#[derive(Clone, Debug)]
pub struct PictureSegmentBuilder {
    template: Vec<u8>,
    framerate: gst::Fraction,
    codestream_length: Option<u32>,
    profile_level: ProfileLevel,
}

impl PictureSegmentBuilder {
    pub fn from_caps(caps: &gst::Caps) -> Result<Self, anyhow::Error> {
        let s = caps
            .structure(0)
            .context("JPEG XS caps without structure")?;

        let depth = s.get::<i32>("depth").ok();
        let sampling = s.get::<&str>("sampling").ok();
        let framerate = s
            .get::<gst::Fraction>("framerate")
            .ok()
            .filter(|fps| *fps > gst::Fraction::new(0, 1))
            .context("JPEG XS jpvi brat requires a usable sink framerate")?;
        let codestream_length = match s.get_optional::<i32>("codestream-length") {
            Ok(Some(n)) if n > 0 => Some(n as u32),
            Ok(None) => None,
            Ok(Some(_)) | Err(_) => {
                bail!("JPEG XS codestream-length must be a positive integer");
            }
        };
        let profile_level = ProfileLevel::from_caps(s).map_err(anyhow::Error::msg)?;

        let colorimetry = s
            .get::<&str>("colorimetry")
            .ok()
            .and_then(|value| gst_video::VideoColorimetry::from_str(value).ok())
            .unwrap_or_else(|| gst_video::VideoColorimetry::from_str("bt709").unwrap());

        let mut template = Vec::with_capacity(PICTURE_SEGMENT_PREFIX_SIZE);

        template.extend_from_slice(&JPVS_BOX_SIZE.to_be_bytes());
        template.extend_from_slice(&VIDEO_SUPPORT_BOX_TYPE);
        template.extend_from_slice(&JPVI_BOX_SIZE.to_be_bytes());
        template.extend_from_slice(&VIDEO_INFORMATION_BOX_TYPE);
        template.extend_from_slice(&1u32.to_be_bytes()); // brat placeholder
        template.extend_from_slice(&encode_frat(framerate).to_be_bytes());
        template.extend_from_slice(&encode_schar(depth, sampling).to_be_bytes());
        template.extend_from_slice(&[0, 0, 0, 1]); // tcod placeholder 00:00:00:01 (ISO 21122-3 requires FF in 1..60)
        template.extend_from_slice(&JXPL_BOX_SIZE.to_be_bytes());
        template.extend_from_slice(&PROFILE_LEVEL_BOX_TYPE);
        template.extend_from_slice(&0u16.to_be_bytes()); // Ppih placeholder
        template.extend_from_slice(&0u16.to_be_bytes()); // Plev placeholder

        template.extend_from_slice(&COLR_BOX_SIZE.to_be_bytes());
        template.extend_from_slice(&COLOUR_SPECIFICATION_BOX_TYPE);
        template.push(5); // METH = CICP
        template.push(0); // PREC
        template.push(0); // APPR
        template.extend_from_slice(&(colorimetry.primaries().to_iso() as u16).to_be_bytes());
        template.extend_from_slice(&(colorimetry.transfer().to_iso() as u16).to_be_bytes());
        template.extend_from_slice(&(colorimetry.matrix().to_iso() as u16).to_be_bytes());
        // CICP video_full_range_flag is the most significant bit of the trailing byte.
        template.push(
            if colorimetry.range() == gst_video::VideoColorRange::Range0_255 {
                0x80
            } else {
                0x00
            },
        );

        debug_assert_eq!(template.len(), PICTURE_SEGMENT_PREFIX_SIZE);

        Ok(Self {
            template,
            framerate,
            codestream_length,
            profile_level,
        })
    }

    pub fn wrap_codestream(
        &self,
        codestream: &[u8],
        pts: Option<gst::ClockTime>,
    ) -> Result<(Vec<u8>, Vec<String>), anyhow::Error> {
        if !codestream.starts_with(&JPEG_XS_SOC) {
            bail!(
                "JPEG XS codestream must start with SOC marker 0x{:02x}{:02x}",
                JPEG_XS_SOC[0],
                JPEG_XS_SOC[1]
            );
        }

        let mut prefix = self.template.clone();
        // ISO/IEC 21122-1 requires a PIH marker in every codestream. Ppih/Plev may
        // still be zero (encoder-unset); that is distinct from a missing PIH.
        let (lcod, codestream_ppih, codestream_plev) =
            parse_pih_fields(codestream).context("JPEG XS codestream without PIH marker")?;
        let (ppih, plev, warnings) = self
            .profile_level
            .resolve_ppih_plev(codestream_ppih, codestream_plev)
            .map_err(anyhow::Error::msg)?;
        let brat = encode_brat(lcod, self.codestream_length, self.framerate)?;

        prefix[BRAT_OFFSET..BRAT_OFFSET + 4].copy_from_slice(&brat.to_be_bytes());
        prefix[PPIH_OFFSET..PPIH_OFFSET + 2].copy_from_slice(&ppih.to_be_bytes());
        prefix[PLEV_OFFSET..PLEV_OFFSET + 2].copy_from_slice(&plev.to_be_bytes());

        if let Some(pts) = pts {
            let tcod = encode_tcod(pts, self.framerate);
            prefix[TCOD_OFFSET..TCOD_OFFSET + 4].copy_from_slice(&tcod.to_be_bytes());
        }

        let mut picture_segment = prefix;
        picture_segment.extend_from_slice(codestream);
        Ok((picture_segment, warnings))
    }
}

pub fn codestream_offset_in_picture_segment(data: &[u8]) -> Result<usize, anyhow::Error> {
    let mut offset = 0;

    while offset + 2 <= data.len() {
        if data[offset..].starts_with(&JPEG_XS_SOC) {
            return Ok(offset);
        }

        let size = box_size(data, offset)?;
        if size == 0 {
            bail!("JPEG XS picture segment box extends to end of buffer before codestream");
        }

        offset = offset
            .checked_add(size)
            .context("JPEG XS picture segment box offset overflow")?;
    }

    bail!("JPEG XS picture segment without codestream SOC marker");
}

#[cfg(test)]
pub fn codestream_from_picture_segment(data: &[u8]) -> Result<&[u8], anyhow::Error> {
    let offset = codestream_offset_in_picture_segment(data)?;
    Ok(&data[offset..])
}

fn box_size(data: &[u8], offset: usize) -> Result<usize, anyhow::Error> {
    let header = data
        .get(offset..offset + BOX_HEADER_SIZE)
        .context("truncated JPEG XS picture segment box header")?;
    let lbox = u32::from_be_bytes(header[..4].try_into().unwrap());

    match lbox {
        0 => Ok(0),
        1 => {
            let extended_header = data
                .get(offset..offset + EXTENDED_BOX_HEADER_SIZE)
                .context("truncated JPEG XS picture segment extended box header")?;
            let xlbox = u64::from_be_bytes(extended_header[8..16].try_into().unwrap());
            let size = usize::try_from(xlbox).context("JPEG XS picture segment box too large")?;
            if size < EXTENDED_BOX_HEADER_SIZE {
                bail!("invalid JPEG XS picture segment extended box size {size}");
            }
            if offset + size > data.len() {
                bail!("truncated JPEG XS picture segment extended box");
            }
            Ok(size)
        }
        size => {
            let size = usize::try_from(size).unwrap();
            if size < BOX_HEADER_SIZE {
                bail!("invalid JPEG XS picture segment box size {size}");
            }
            if offset + size > data.len() {
                bail!("truncated JPEG XS picture segment box");
            }
            Ok(size)
        }
    }
}

fn encode_frat(framerate: gst::Fraction) -> u32 {
    let interlace_mode = 0u32;
    let (denominator_code, numerator) = if framerate.denom() == 1001 {
        (2u32, ((framerate.numer() + 500) / 1001).max(1) as u32)
    } else if framerate.denom() == 1 {
        (1u32, framerate.numer().max(1) as u32)
    } else {
        (
            1u32,
            ((framerate.numer() + framerate.denom() / 2) / framerate.denom()).max(1) as u32,
        )
    };

    (interlace_mode << 30) | (denominator_code << 24) | (numerator & 0xffff)
}

fn encode_schar(depth: Option<i32>, sampling: Option<&str>) -> u16 {
    let Some(depth) = depth else {
        // Valid_Flag = 0: sample characteristics not specified in jpvi.
        return 0;
    };
    let Some(sampling_code) = sampling.and_then(sampling_to_schar) else {
        // RFC 9134 sampling values KEY / UNSPECIFIED (and anything else we
        // cannot map into ISO/IEC 21122-3 Table A.10) leave Valid_Flag cleared
        // rather than inventing a Sampling_Structure code.
        return 0;
    };

    let bitdepth = (depth - 1).clamp(0, 15) as u16;
    (1u16 << 15) | ((bitdepth & 0xf) << 4) | (u16::from(sampling_code) & 0xf)
}

fn sampling_to_schar(sampling: &str) -> Option<u8> {
    // ISO/IEC 21122-3 Table A.10 Sampling_Structure codes.
    // RFC 9134 sampling values KEY, XYZ and UNSPECIFIED have no entry here:
    // KEY is single-component, XYZ is a different colour space than RGB, and
    // UNSPECIFIED means "signaled by the payload". Leave Valid_Flag cleared.
    match sampling {
        "YCbCr-4:2:2" | "CLYCbCr-4:2:2" | "ICtCp-4:2:2" => Some(0),
        "YCbCr-4:4:4" | "CLYCbCr-4:4:4" | "ICtCp-4:4:4" => Some(1),
        "RGB" => Some(2),
        "YCbCr-4:2:0" | "CLYCbCr-4:2:0" | "ICtCp-4:2:0" => Some(3),
        _ => None,
    }
}

/// Maximum codestream bit rate in Mbit/s from PIH Lcod / caps `codestream-length`
/// and framerate, matching mpegtsmux's
/// `ceil(bytes * fps_n / (fps_d * 125000))` (`* 8 / 1e6` ≡ `/ 125000`).
fn encode_brat(
    lcod: u32,
    codestream_length: Option<u32>,
    framerate: gst::Fraction,
) -> Result<u32, anyhow::Error> {
    let bytes = match codestream_length {
        Some(cap) if lcod > cap => {
            bail!("JPEG XS PIH Lcod {lcod} exceeds caps codestream-length {cap}")
        }
        Some(cap) => cap,
        None if lcod > 0 => lcod,
        None => bail!("JPEG XS jpvi brat requires codestream-length or a non-zero PIH Lcod"),
    };
    let brat = u64::from(bytes)
        .mul_div_ceil(framerate.numer() as u64, framerate.denom() as u64 * 125_000)
        .context("JPEG XS jpvi brat calculation overflow")?;
    u32::try_from(brat).context("JPEG XS jpvi brat exceeds u32")
}

fn encode_tcod(pts: gst::ClockTime, framerate: gst::Fraction) -> u32 {
    let total_seconds = pts.seconds();
    let hours = (total_seconds / 3600) % 24;
    let minutes = (total_seconds / 60) % 60;
    let seconds = total_seconds % 60;

    // tcod FF is a 1-based frame count within the second (ISO 21122-3 Table A.5, range 1..60).
    // pts is quantised to whole nanoseconds and so usually sits just below the ideal frame
    // time, hence rounding to the nearest frame rather than flooring. Rounding can reach the
    // frame count itself just before the next second; clamp so that belongs to HHMMSS there.
    let frames_per_second = (framerate.numer() as u64).div_ceil(framerate.denom() as u64);
    let frames = (1
        + (pts.nseconds() % 1_000_000_000)
            .mul_div_round(
                framerate.numer() as u64,
                1_000_000_000 * framerate.denom() as u64,
            )
            .unwrap_or(0))
    .min(frames_per_second);

    u32::from(hours as u8) << 24
        | u32::from(minutes as u8) << 16
        | u32::from(seconds as u8) << 8
        | u32::from(frames as u8)
}

fn skip_marker_segment(data: &[u8], offset: usize) -> Option<usize> {
    if data.len() < offset + 4 {
        return None;
    }

    // Lxxx is the parameter byte count including the two Lxxx bytes themselves,
    // but excluding the two-byte marker (ISO/IEC 21122-1 Annex A).
    let segment_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
    if segment_len < 2 {
        return None;
    }

    let next = offset.checked_add(2)?.checked_add(segment_len)?;
    (next <= data.len()).then_some(next)
}

pub(crate) fn parse_pih_fields(codestream: &[u8]) -> Option<(u32, u16, u16)> {
    if !codestream.starts_with(&JPEG_XS_SOC) {
        return None;
    }

    let mut offset = 2;
    if codestream.get(offset..offset + 2)? == MARKER_CAP {
        offset = skip_marker_segment(codestream, offset)?;
    }
    if codestream.get(offset..offset + 2)? != MARKER_PIH {
        return None;
    }

    let lpih = u16::from_be_bytes([codestream[offset + 2], codestream[offset + 3]]) as usize;
    if lpih < 8 || codestream.len() < offset + lpih {
        return None;
    }

    let payload = offset + 4;
    if codestream.len() < payload + 8 {
        return None;
    }

    let lcod = u32::from_be_bytes(codestream[payload..payload + 4].try_into().ok()?);
    let ppih = u16::from_be_bytes([codestream[payload + 4], codestream[payload + 5]]);
    let plev = u16::from_be_bytes([codestream[payload + 6], codestream[payload + 7]]);
    Some((lcod, ppih, plev))
}

/// Find the payload range of the first box of `box_type` between `start` and `end`.
fn find_box(data: &[u8], box_type: [u8; 4], start: usize, end: usize) -> Option<(usize, usize)> {
    let end = end.min(data.len());
    let mut offset = start;

    while offset + BOX_HEADER_SIZE <= end {
        if data[offset..].starts_with(&JPEG_XS_SOC) {
            break;
        }

        let size = box_size(data, offset).ok()?;
        if size == 0 {
            break;
        }

        let box_end = offset.checked_add(size)?;
        if box_end > end {
            break;
        }

        if data[offset + 4..offset + 8] == box_type {
            return Some((offset + BOX_HEADER_SIZE, box_end));
        }

        offset = box_end;
    }

    None
}

/// The `jxpl` box carrying `Ppih`/`Plev` is nested inside the top-level `jpvs` box.
fn parse_jxpl_fields(data: &[u8]) -> Option<(u16, u16)> {
    let (jpvs_start, jpvs_end) = find_box(data, VIDEO_SUPPORT_BOX_TYPE, 0, data.len())?;
    let (jxpl_start, jxpl_end) = find_box(data, PROFILE_LEVEL_BOX_TYPE, jpvs_start, jpvs_end)?;
    if jxpl_end < jxpl_start + 4 {
        return None;
    }
    let ppih = u16::from_be_bytes([data[jxpl_start], data[jxpl_start + 1]]);
    let plev = u16::from_be_bytes([data[jxpl_start + 2], data[jxpl_start + 3]]);
    Some((ppih, plev))
}

/// Warn when `jxpl`, codestream PIH and caps profile/level/sublevel disagree.
pub(crate) fn profile_level_warnings(
    picture_segment: &[u8],
    profile_level: &ProfileLevel,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let jxpl = parse_jxpl_fields(picture_segment);
    let codestream = codestream_offset_in_picture_segment(picture_segment)
        .ok()
        .and_then(|offset| {
            parse_pih_fields(&picture_segment[offset..]).map(|(_, ppih, plev)| (ppih, plev))
        });

    if let (Some((jxpl_ppih, jxpl_plev)), Some((codestream_ppih, codestream_plev))) =
        (jxpl, codestream)
    {
        if jxpl_ppih != 0 && codestream_ppih != 0 && jxpl_ppih != codestream_ppih {
            warnings.push(format!(
                "jxpl Ppih 0x{jxpl_ppih:04x} does not match codestream Ppih 0x{codestream_ppih:04x}"
            ));
        }
        if jxpl_plev != 0 && codestream_plev != 0 && jxpl_plev != codestream_plev {
            warnings.push(format!(
                "jxpl Plev 0x{jxpl_plev:04x} does not match codestream Plev 0x{codestream_plev:04x}"
            ));
        }
    }

    let (ppih, plev) = codestream.or(jxpl).unwrap_or((0, 0));
    if (ppih != 0 || plev != 0)
        && let Ok((_, _, mut caps_warnings)) = profile_level.resolve_ppih_plev(ppih, plev)
    {
        warnings.append(&mut caps_warnings);
    }

    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_box(box_type: [u8; 4]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&8u32.to_be_bytes());
        data.extend_from_slice(&box_type);
        data
    }

    #[test]
    fn strips_boxes_before_codestream() {
        let mut data = empty_box(VIDEO_SUPPORT_BOX_TYPE);
        data.extend_from_slice(&empty_box(COLOUR_SPECIFICATION_BOX_TYPE));
        data.extend_from_slice(&[0xff, 0x10, 0xff, 0x50]);

        assert_eq!(
            codestream_from_picture_segment(&data).unwrap(),
            &[0xff, 0x10, 0xff, 0x50]
        );
    }

    #[test]
    fn accepts_bare_codestream() {
        let data = [0xff, 0x10, 0xff, 0x50];

        assert_eq!(codestream_from_picture_segment(&data).unwrap(), data);
    }

    fn minimal_codestream_with_pih() -> Vec<u8> {
        // SOC + CAP + PIH (Ppih/Plev unset).
        vec![
            0xff, 0x10, 0xff, 0x50, 0x00, 0x04, 0x00, 0x80, 0xff, 0x12, 0x00, 0x0c, 0x00, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]
    }

    fn jxsc_caps(framerate: gst::Fraction, codestream_length: Option<i32>) -> gst::Caps {
        let mut builder = gst::Caps::builder("image/x-jxsc")
            .field("alignment", "frame")
            .field("interlace-mode", "progressive")
            .field("width", 640i32)
            .field("height", 480i32)
            .field("depth", 10i32)
            .field("sampling", "YCbCr-4:2:2")
            .field("framerate", framerate);
        if let Some(codestream_length) = codestream_length {
            builder = builder.field("codestream-length", codestream_length);
        }
        builder.build()
    }

    #[test]
    fn builds_rfc9134_picture_segment_prefix() {
        gst::init().unwrap();

        let caps = jxsc_caps(gst::Fraction::new(25, 1), None);
        let builder = PictureSegmentBuilder::from_caps(&caps).unwrap();
        let codestream = minimal_codestream_with_pih();
        let (picture_segment, warnings) = builder.wrap_codestream(&codestream, None).unwrap();
        assert!(warnings.is_empty());

        assert_eq!(
            picture_segment.len(),
            PICTURE_SEGMENT_PREFIX_SIZE + codestream.len()
        );
        assert_eq!(&picture_segment[4..8], b"jpvs");
        assert_eq!(&picture_segment[PICTURE_SEGMENT_PREFIX_SIZE..], &codestream);
        assert_eq!(
            u16::from_be_bytes(picture_segment[24..26].try_into().unwrap()),
            0x8090
        );
    }

    #[test]
    fn rejects_codestream_without_pih() {
        gst::init().unwrap();

        let caps = jxsc_caps(gst::Fraction::new(25, 1), None);
        let builder = PictureSegmentBuilder::from_caps(&caps).unwrap();
        let codestream = [0xff, 0x10, 0xff, 0x50, 0x00, 0x04, 0x00, 0x80];
        let err = builder.wrap_codestream(&codestream, None).unwrap_err();
        assert!(
            err.to_string().contains("without PIH"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_missing_framerate() {
        gst::init().unwrap();

        let caps = gst::Caps::builder("image/x-jxsc")
            .field("alignment", "frame")
            .field("interlace-mode", "progressive")
            .field("width", 640i32)
            .field("height", 480i32)
            .field("depth", 10i32)
            .field("sampling", "YCbCr-4:2:2")
            .build();

        let err = PictureSegmentBuilder::from_caps(&caps).unwrap_err();
        assert!(
            err.to_string().contains("usable sink framerate"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_invalid_codestream_length() {
        gst::init().unwrap();

        let err = PictureSegmentBuilder::from_caps(&jxsc_caps(gst::Fraction::new(25, 1), Some(0)))
            .unwrap_err();
        assert!(
            err.to_string().contains("codestream-length"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_zero_lcod_without_codestream_length() {
        let err = encode_brat(0, None, gst::Fraction::new(25, 1)).unwrap_err();
        assert!(
            err.to_string().contains("codestream-length") && err.to_string().contains("Lcod"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn uses_codestream_length_for_brat() {
        // brat = ceil(bytes * 25 / 125000).
        // Lcod (5_000) alone would give brat 1; caps codestream-length (5_001) must win → brat 2.
        let brat = encode_brat(5_000, Some(5_001), gst::Fraction::new(25, 1)).unwrap();
        assert_eq!(brat, 2);
    }

    #[test]
    fn uses_lcod_when_codestream_length_absent() {
        // ceil(30_000 * 25 / 125000) == 6
        let brat = encode_brat(30_000, None, gst::Fraction::new(25, 1)).unwrap();
        assert_eq!(brat, 6);
    }

    #[test]
    fn rejects_lcod_exceeding_codestream_length() {
        let err = encode_brat(30_000, Some(1_000), gst::Fraction::new(25, 1)).unwrap_err();
        assert!(
            err.to_string().contains("exceeds caps codestream-length"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn encodes_brat_with_1001_framerate() {
        // ceil(121000 * 30000 / (1001 * 125000)) == 30; trunc or round would be 29.
        let brat = encode_brat(0, Some(121_000), gst::Fraction::new(30_000, 1001)).unwrap();
        assert_eq!(brat, 30);
    }

    #[test]
    fn rounds_tcod_frames_to_nearest_frame() {
        gst::init().unwrap();

        // 30000/1001: frame 1 lands on 33366666ns once quantised, which floors to frame 0.
        let tcod = encode_tcod(
            gst::ClockTime::from_nseconds(33_366_666),
            gst::Fraction::new(30_000, 1001),
        );
        assert_eq!(tcod as u8, 2);
    }

    #[test]
    fn clamps_tcod_frames_within_the_second() {
        gst::init().unwrap();

        // Just before the next second rounds up to frame 25, which belongs to that second.
        let tcod = encode_tcod(
            gst::ClockTime::from_nseconds(999_999_999),
            gst::Fraction::new(25, 1),
        );
        assert_eq!(tcod as u8, 25);
    }

    #[test]
    fn patches_ppih_plev_from_caps_when_codestream_unset() {
        gst::init().unwrap();

        let caps = gst::Caps::builder("image/x-jxsc")
            .field("alignment", "frame")
            .field("interlace-mode", "progressive")
            .field("width", 640i32)
            .field("height", 480i32)
            .field("depth", 10i32)
            .field("sampling", "YCbCr-4:2:2")
            .field("framerate", gst::Fraction::new(25, 1))
            .field("profile", "Main422.10")
            .field("level", "1k-1")
            .field("sublevel", "Full")
            .build();

        let builder = PictureSegmentBuilder::from_caps(&caps).unwrap();
        let codestream = [
            0xff, 0x10, 0xff, 0x12, 0x00, 0x0c, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let (picture_segment, warnings) = builder.wrap_codestream(&codestream, None).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(
            u16::from_be_bytes(picture_segment[38..40].try_into().unwrap()),
            0x3540
        );
        assert_eq!(
            u16::from_be_bytes(picture_segment[40..42].try_into().unwrap()),
            0x0480
        );
    }

    #[test]
    fn profile_level_warnings_detect_jxpl_codestream_mismatch() {
        gst::init().unwrap();

        let caps = gst::Caps::builder("image/x-jxsc")
            .field("alignment", "frame")
            .field("interlace-mode", "progressive")
            .field("width", 640i32)
            .field("height", 480i32)
            .field("depth", 10i32)
            .field("sampling", "YCbCr-4:2:2")
            .field("framerate", gst::Fraction::new(25, 1))
            .field("profile", "Main422.10")
            .field("level", "1k-1")
            .field("sublevel", "Full")
            .build();

        let builder = PictureSegmentBuilder::from_caps(&caps).unwrap();
        let codestream = [
            0xff, 0x10, 0xff, 0x12, 0x00, 0x0c, 0x00, 0x00, 0x01, 0x00, 0x35, 0x40, 0x04, 0x80,
        ];
        let (mut picture_segment, _) = builder.wrap_codestream(&codestream, None).unwrap();
        picture_segment[38..40].copy_from_slice(&0x1500u16.to_be_bytes());
        let warnings = profile_level_warnings(
            &picture_segment,
            &ProfileLevel::from_caps(caps.structure(0).unwrap()).unwrap(),
        );
        assert!(
            warnings.iter().any(|warning| warning.contains("jxpl Ppih")),
            "warnings: {warnings:?}"
        );
    }
}
