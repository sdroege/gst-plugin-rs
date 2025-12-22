// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::isobmff::Variant;
use std::collections::BTreeSet;

fn is_video_codec(name: &str) -> bool {
    matches!(
        name,
        "video/x-h264"
            | "video/x-h265"
            | "video/x-vp8"
            | "video/x-vp9"
            | "video/x-av1"
            | "image/jpeg"
            | "video/x-raw"
    )
}

fn supports_mp4_brands(name: &str) -> bool {
    matches!(
        name,
        "video/x-h264"
            | "video/x-h265"
            | "video/x-vp8"
            | "video/x-vp9"
            | "image/jpeg"
            | "video/x-raw"
            | "audio/mpeg"
            | "audio/x-opus"
            | "audio/x-flac"
            | "audio/x-alaw"
            | "audio/x-mulaw"
            | "audio/x-adpcm"
            | "audio/x-ac3"
            | "audio/x-eac3"
    )
}

fn cmaf_brands_from_caps(caps: &gst::CapsRef, compatible_brands: &mut BTreeSet<[u8; 4]>) {
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
                                compatible_brands.insert(*b"cfsd");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.insert(*b"cfsd");
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
                                compatible_brands.insert(*b"cfhd");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.insert(*b"cfhd");
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
                                compatible_brands.insert(*b"chdf");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.insert(*b"chdf");
                        }
                    }
                }
            }
        }
        "audio/mpeg" => {
            compatible_brands.insert(*b"caac");
        }
        "audio/x-eac3" => {
            compatible_brands.insert(*b"ceac");
        }
        "video/x-av1" => {
            compatible_brands.insert(*b"cmf2");
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
                                compatible_brands.insert(*b"chhd");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.insert(*b"chhd");
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
                                compatible_brands.insert(*b"cud8");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.insert(*b"cud8");
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
                                compatible_brands.insert(*b"chh1");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.insert(*b"chh1");
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
                                compatible_brands.insert(*b"cud1");
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
                                compatible_brands.insert(*b"chd1");
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
                                compatible_brands.insert(*b"clg1");
                            }
                        } else {
                            // Assume it's OK
                            compatible_brands.insert(*b"cud1");
                        }
                    }
                }
            }
        }
        _ => (),
    }
}

pub(crate) fn brands_from_variant_and_caps<'a>(
    variant: Variant,
    caps: impl Iterator<Item = &'a gst::Caps> + Clone,
    image_sequence_mode: bool,
    with_precision_timestamps: bool,
    extra_brands: &[[u8; 4]],
) -> (u32, [u8; 4], Vec<[u8; 4]>) {
    let mut major_brand = *b"iso6";
    let mut minor_version = 0u32;
    let mut compatible_brands = BTreeSet::new();
    let mut have_image_sequence = false; // Marked true if an image sequence
    let mut have_only_image_sequence = true; // Marked false if video found
    let non_fragmented = variant == Variant::ISO || variant == Variant::ONVIF;

    match variant {
        Variant::FragmentedISO | Variant::FragmentedONVIF => {}
        Variant::DASH => {
            major_brand = *b"msdh";

            // FIXME: `dsms` / `dash` brands, `msix`
            compatible_brands.insert(*b"dums");
            compatible_brands.insert(*b"msdh");
            compatible_brands.insert(*b"iso6");
        }
        Variant::CMAF => {
            major_brand = *b"cmf2";
            compatible_brands.insert(*b"iso6");
            compatible_brands.insert(*b"cmfc");

            let mut caps = caps.clone();
            cmaf_brands_from_caps(caps.next().unwrap(), &mut compatible_brands);
            assert_eq!(caps.next(), None);
        }
        Variant::ISO | Variant::ONVIF => {
            major_brand = *b"iso4";
            if image_sequence_mode {
                compatible_brands.insert(*b"iso8");
                compatible_brands.insert(*b"unif");
                compatible_brands.insert(*b"msf1");
                have_image_sequence = true;
            }

            if with_precision_timestamps {
                // Required for saiz/saio support
                compatible_brands.insert(*b"iso6");
            }
        }
    }

    for caps in caps {
        let caps_structure = caps.structure(0).unwrap();

        if non_fragmented && !image_sequence_mode {
            let name = caps_structure.name().as_str();

            if is_video_codec(name) {
                have_only_image_sequence = false;
            }

            if supports_mp4_brands(name) {
                compatible_brands.insert(*b"mp41");
                compatible_brands.insert(*b"mp42");
                compatible_brands.insert(*b"isom");
            }
        }

        match caps_structure.name().as_str() {
            "video/x-av1" => {
                minor_version = 1;
                compatible_brands.insert(*b"av01");
            }
            "video/x-h264" => {
                compatible_brands.insert(*b"avc1");
            }
            "audio/x-ac3" | "audio/x-eac3" => {
                compatible_brands.insert(*b"dby1");
            }
            "audio/x-opus" => {
                compatible_brands.insert(*b"opus");
            }
            _ => {}
        }
    }

    if non_fragmented && have_image_sequence && have_only_image_sequence {
        major_brand = *b"msf1";
    }

    for brand in extra_brands {
        compatible_brands.insert(*brand);
    }
    compatible_brands.insert(major_brand);

    (
        minor_version,
        major_brand,
        compatible_brands.into_iter().collect(),
    )
}
