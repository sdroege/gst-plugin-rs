// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Map GStreamer [`gst_video::VideoColorimetry`] to/from SDP fmtp colour
//! parameters shared by RFC 9134 (`video/jxsv`) and ST 2110-20 (`video/raw`).
//!
//! The token vocabularies match (`colorimetry`, `TCS`, `RANGE`), but
//! **omission defaults differ**. Callers pass a [`FmtpColorDefaults`] profile
//! so the same helpers can serve both payload formats.

use std::str::FromStr;

use gst_video::{
    VideoColorMatrix, VideoColorPrimaries, VideoColorRange, VideoColorimetry, VideoTransferFunction,
};

/// How to treat colour fmtp fields when they are absent from the SDP.
///
/// | Profile | default `TCS` when absent | default `RANGE` when absent |
/// |---|---|---|
/// | [`FmtpColorDefaults::Rfc9134`] | `SDR` for `BT601`/`BT709`/`BT2020`/`SMPTE240M`; none for `BT2100` | `NARROW`, except `UNSPECIFIED` --> `FULL` |
/// | [`FmtpColorDefaults::St2110_20`] | `SDR` | `NARROW` |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FmtpColorDefaults {
    /// RFC 9134 section 7.1 omission defaults for `video/jxsv`.
    Rfc9134,
    /// ST 2110-20 section 7.3 omission defaults for `video/raw`.
    St2110_20,
}

impl FmtpColorDefaults {
    /// Default `TCS` when the parameter is absent for the given `colorimetry`.
    ///
    /// ST 2110-20 defaults every system to `SDR`. RFC 9134 lists `TCS` values
    /// but unlike `RANGE` does not define a value to assume when the parameter
    /// is absent; for reconstructing GStreamer colorimetry we treat the usual
    /// SDR systems as `SDR`, and leave `BT2100` unresolved (PQ/HLG must be
    /// signalled).
    pub fn default_transfer_characteristic(self, colorimetry: &str) -> Option<&'static str> {
        match self {
            Self::St2110_20 => Some("SDR"),
            Self::Rfc9134 => match colorimetry {
                "BT601-5" | "BT601" | "BT709-2" | "BT709" | "BT2020" | "SMPTE240M" => Some("SDR"),
                // RFC 9134 also defines these; none have an assumed TCS when absent
                // (BT2100 needs PQ/HLG; the rest have no GStreamer four-tuple mapping).
                "BT2100" | "ST2065-1" | "ST2065-3" | "XYZ" | "UNSPECIFIED" => None,
                _ => None,
            },
        }
    }

    /// Default `RANGE` when the parameter is absent for the given `colorimetry`.
    pub fn default_range(self, colorimetry: &str) -> &'static str {
        match self {
            Self::Rfc9134 if colorimetry == "UNSPECIFIED" => "FULL",
            Self::Rfc9134 | Self::St2110_20 => "NARROW",
        }
    }
}

/// Colour media-type parameters for an SDP `a=fmtp` line / RTP caps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FmtpColorParams<'a> {
    pub colorimetry: &'a str,
    pub tcs: Option<&'a str>,
    pub range: Option<&'a str>,
}

/// Map a GStreamer colorimetry string to fmtp colour parameters.
///
/// Named presets (`bt709`, `bt2100-pq`, etc.) and numeric `r:m:t:p` forms are
/// both accepted. When the GStreamer transfer function and range map cleanly,
/// `TCS` and `RANGE` are always included (even if they match a profile default).
/// They are omitted only when the GStreamer field has no matching token (e.g.
/// `Unknown`) - `colorimetry` is still emitted in that case.
pub(crate) fn fmtp_color_params_from_gst_colorimetry(
    colorimetry: &str,
) -> Option<FmtpColorParams<'static>> {
    let video_colorimetry = VideoColorimetry::from_str(colorimetry).ok()?;
    let colorimetry = fmtp_colorimetry_from_gst(&video_colorimetry)?;

    let tcs = fmtp_transfer_characteristic_from_gst(video_colorimetry.transfer());
    let range = fmtp_range_from_gst(video_colorimetry.range());

    Some(FmtpColorParams {
        colorimetry,
        tcs,
        range,
    })
}

/// Map fmtp colour parameters to a GStreamer colorimetry string.
///
/// Builds the four [`VideoColorimetry`] fields explicitly so `RANGE` and
/// non-preset transfer functions (e.g. `BT2100` + `TCS=SDR`) are representable
/// without defaulting `BT2100` to `bt2100-pq`. Returns `None` when the transfer
/// function cannot be resolved.
///
/// `depth` is the SDP `depth` media-type parameter when known. It is only
/// consulted for `colorimetry=BT2020` or `BT2100` with an SDR transfer: values
/// `>= 12` select [`VideoTransferFunction::Bt202012`] (`"bt2020"`), and missing
/// or smaller depths select [`VideoTransferFunction::Bt202010`] (`"bt2020-10"`).
/// `BT2100` + `TCS=SDR` has no named GStreamer preset and therefore collapses
/// to the same strings.
pub(crate) fn gst_colorimetry_from_fmtp_color_params(
    colorimetry: &str,
    tcs: Option<&str>,
    range: Option<&str>,
    depth: Option<u8>,
    defaults: FmtpColorDefaults,
) -> Option<String> {
    // Unrecognised RANGE tokens fall back to the profile default.
    let range = range
        .and_then(gst_range_from_fmtp)
        .or_else(|| gst_range_from_fmtp(defaults.default_range(colorimetry)))?;
    let (matrix, primaries) = gst_matrix_and_primaries_from_fmtp(colorimetry)?;
    // Prefer an explicit TCS when recognised; otherwise use the profile default.
    // Unrecognised tokens (e.g. LINEAR) are ignored.
    let transfer = tcs
        .and_then(|tcs| gst_transfer_characteristic_from_fmtp(tcs, colorimetry, depth))
        .or_else(|| {
            defaults
                .default_transfer_characteristic(colorimetry)
                .and_then(|tcs| gst_transfer_characteristic_from_fmtp(tcs, colorimetry, depth))
        })?;

    let video_colorimetry = VideoColorimetry::new(range, matrix, transfer, primaries);
    Some(video_colorimetry.to_string())
}

fn fmtp_colorimetry_from_gst(video_colorimetry: &VideoColorimetry) -> Option<&'static str> {
    match (
        video_colorimetry.matrix(),
        video_colorimetry.primaries(),
        video_colorimetry.transfer(),
    ) {
        (VideoColorMatrix::Bt601, VideoColorPrimaries::Smpte170m, _) => Some("BT601"),
        (VideoColorMatrix::Bt709, VideoColorPrimaries::Bt709, _) => Some("BT709"),
        (VideoColorMatrix::Smpte240m, VideoColorPrimaries::Smpte240m, _) => Some("SMPTE240M"),
        (
            VideoColorMatrix::Bt2020,
            VideoColorPrimaries::Bt2020,
            VideoTransferFunction::Smpte2084 | VideoTransferFunction::AribStdB67,
        ) => Some("BT2100"),
        (VideoColorMatrix::Bt2020, VideoColorPrimaries::Bt2020, _) => Some("BT2020"),
        _ => None,
    }
}

fn gst_matrix_and_primaries_from_fmtp(
    colorimetry: &str,
) -> Option<(VideoColorMatrix, VideoColorPrimaries)> {
    match colorimetry {
        "BT601-5" | "BT601" => Some((VideoColorMatrix::Bt601, VideoColorPrimaries::Smpte170m)),
        "BT709-2" | "BT709" => Some((VideoColorMatrix::Bt709, VideoColorPrimaries::Bt709)),
        "BT2020" | "BT2100" => Some((VideoColorMatrix::Bt2020, VideoColorPrimaries::Bt2020)),
        "SMPTE240M" => Some((VideoColorMatrix::Smpte240m, VideoColorPrimaries::Smpte240m)),
        // RFC 9134 and ST 2110-20 section 7.5 Other values have no equivalent GStreamer colorimetry.
        "ST2065-1" | "ST2065-3" | "UNSPECIFIED" | "XYZ" | "ALPHA" => None,
        _ => None,
    }
}

fn fmtp_transfer_characteristic_from_gst(transfer: VideoTransferFunction) -> Option<&'static str> {
    match transfer {
        VideoTransferFunction::Bt709
        | VideoTransferFunction::Bt601
        | VideoTransferFunction::Smpte240m
        | VideoTransferFunction::Bt202010
        | VideoTransferFunction::Bt202012 => Some("SDR"),
        VideoTransferFunction::Smpte2084 => Some("PQ"),
        VideoTransferFunction::AribStdB67 => Some("HLG"),
        // Other GStreamer variants with no equivalent ST 2110-20 / RFC 9134 value omit TCS.
        _ => None,
    }
}

fn gst_transfer_characteristic_from_fmtp(
    tcs: &str,
    colorimetry: &str,
    depth: Option<u8>,
) -> Option<VideoTransferFunction> {
    match tcs {
        "SDR" => Some(match colorimetry {
            "BT601-5" | "BT601" => VideoTransferFunction::Bt601,
            "BT709-2" | "BT709" => VideoTransferFunction::Bt709,
            "SMPTE240M" => VideoTransferFunction::Smpte240m,
            // For BT2020, Rec. ITU-R BT.2020 defines slightly different
            // constants for 10-bit vs 12-bit systems; GStreamer exposes those
            // as Bt202010 / Bt202012. Unknown depth is treated as below 12 bits.
            // BT2100 + TCS=SDR has no named GStreamer preset; same OETF family as
            // BT2020 (stringifies as bt2020 / bt2020-10).
            "BT2020" | "BT2100" => {
                if depth.is_some_and(|d| d >= 12) {
                    VideoTransferFunction::Bt202012
                } else {
                    VideoTransferFunction::Bt202010
                }
            }
            _ => unreachable!(
                "SDR transfer only for colorimetries from gst_matrix_and_primaries_from_fmtp"
            ),
        }),
        "PQ" => Some(VideoTransferFunction::Smpte2084),
        "HLG" => Some(VideoTransferFunction::AribStdB67),
        // ST 2110-20 section 7.6 LINEAR / BT2100LIN* / etc. have no GstVideoTransferFunction we can use yet.
        _ => None,
    }
}

fn fmtp_range_from_gst(range: VideoColorRange) -> Option<&'static str> {
    match range {
        VideoColorRange::Range16_235 => Some("NARROW"),
        VideoColorRange::Range0_255 => Some("FULL"),
        _ => None,
    }
}

fn gst_range_from_fmtp(token: &str) -> Option<VideoColorRange> {
    match token {
        "NARROW" => Some(VideoColorRange::Range16_235),
        // FULLPROTECT (ST 2077) has no equivalent GStreamer Range1_254; treat as full.
        "FULL" | "FULLPROTECT" => Some(VideoColorRange::Range0_255),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        crate::test_init();
    }

    #[test]
    fn rfc9134_named_presets_roundtrip() {
        init();
        let d = FmtpColorDefaults::Rfc9134;

        assert_eq!(
            fmtp_color_params_from_gst_colorimetry("bt709"),
            Some(FmtpColorParams {
                colorimetry: "BT709",
                tcs: Some("SDR"),
                range: Some("NARROW"),
            })
        );
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("BT709", None, None, None, d).as_deref(),
            Some("bt709")
        );

        assert_eq!(
            fmtp_color_params_from_gst_colorimetry("bt601"),
            Some(FmtpColorParams {
                colorimetry: "BT601",
                tcs: Some("SDR"),
                range: Some("NARROW"),
            })
        );
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("BT601", None, None, None, d).as_deref(),
            Some("bt601")
        );

        assert_eq!(
            fmtp_color_params_from_gst_colorimetry("smpte240m"),
            Some(FmtpColorParams {
                colorimetry: "SMPTE240M",
                tcs: Some("SDR"),
                range: Some("NARROW"),
            })
        );
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("SMPTE240M", None, None, None, d).as_deref(),
            Some("smpte240m")
        );

        assert_eq!(
            fmtp_color_params_from_gst_colorimetry("bt2020-10"),
            Some(FmtpColorParams {
                colorimetry: "BT2020",
                tcs: Some("SDR"),
                range: Some("NARROW"),
            })
        );
        assert_eq!(
            fmtp_color_params_from_gst_colorimetry("bt2020"),
            Some(FmtpColorParams {
                colorimetry: "BT2020",
                tcs: Some("SDR"),
                range: Some("NARROW"),
            })
        );
        // Depay needs depth to distinguish bt2020-10 vs bt2020 (see depth test).
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("BT2020", None, None, Some(10), d).as_deref(),
            Some("bt2020-10")
        );
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("BT2020", None, None, Some(12), d).as_deref(),
            Some("bt2020")
        );

        assert_eq!(
            fmtp_color_params_from_gst_colorimetry("bt2100-pq"),
            Some(FmtpColorParams {
                colorimetry: "BT2100",
                tcs: Some("PQ"),
                range: Some("NARROW"),
            })
        );
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("BT2100", Some("PQ"), None, None, d).as_deref(),
            Some("bt2100-pq")
        );

        assert_eq!(
            fmtp_color_params_from_gst_colorimetry("bt2100-hlg"),
            Some(FmtpColorParams {
                colorimetry: "BT2100",
                tcs: Some("HLG"),
                range: Some("NARROW"),
            })
        );
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("BT2100", Some("HLG"), None, None, d).as_deref(),
            Some("bt2100-hlg")
        );
    }

    #[test]
    fn rfc9134_colorimetry_aliases() {
        init();
        let d = FmtpColorDefaults::Rfc9134;
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("BT709-2", None, None, None, d).as_deref(),
            Some("bt709")
        );
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("BT601-5", None, None, None, d).as_deref(),
            Some("bt601")
        );
    }

    #[test]
    fn rfc9134_unknown_transfer_omits_tcs() {
        init();
        let value = VideoColorimetry::new(
            VideoColorRange::Range16_235,
            VideoColorMatrix::Bt709,
            VideoTransferFunction::Unknown,
            VideoColorPrimaries::Bt709,
        )
        .to_string();
        assert_eq!(
            fmtp_color_params_from_gst_colorimetry(&value),
            Some(FmtpColorParams {
                colorimetry: "BT709",
                tcs: None,
                range: Some("NARROW"),
            })
        );
    }

    #[test]
    fn rfc9134_unknown_range_omits_range() {
        init();
        let value = VideoColorimetry::new(
            VideoColorRange::Unknown,
            VideoColorMatrix::Bt709,
            VideoTransferFunction::Bt709,
            VideoColorPrimaries::Bt709,
        )
        .to_string();
        assert_eq!(
            fmtp_color_params_from_gst_colorimetry(&value),
            Some(FmtpColorParams {
                colorimetry: "BT709",
                tcs: Some("SDR"),
                range: None,
            })
        );
    }

    #[test]
    fn rfc9134_unmapped_gst_colorimetry_is_none() {
        init();
        // RGB matrix has no equivalent RFC 9134 / ST 2110-20 colorimetry.
        let unmapped_matrix = VideoColorMatrix::Rgb;
        let value = VideoColorimetry::new(
            VideoColorRange::Range16_235,
            unmapped_matrix,
            VideoTransferFunction::Bt709,
            VideoColorPrimaries::Bt709,
        )
        .to_string();
        assert_eq!(fmtp_color_params_from_gst_colorimetry(&value), None);
    }

    #[test]
    fn rfc9134_unmapped_fmtp_colorimetry_is_none() {
        init();
        let d = FmtpColorDefaults::Rfc9134;
        for colorimetry in [
            "UNSPECIFIED",
            "XYZ",
            "ST2065-1",
            "ST2065-3",
            "ALPHA",
            "invalid-colorimetry",
        ] {
            assert_eq!(
                gst_colorimetry_from_fmtp_color_params(colorimetry, None, None, None, d),
                None,
                "{colorimetry}"
            );
        }
    }

    #[test]
    fn rfc9134_unrecognised_tokens_fall_back_to_defaults() {
        init();
        let d = FmtpColorDefaults::Rfc9134;
        // Unrecognised TCS/RANGE are ignored and the profile defaults apply
        // (SDR + NARROW --> bt709), same as when those parameters are absent.
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params(
                "BT709",
                Some("LINEAR"),
                Some("invalid-range"),
                None,
                d,
            )
            .as_deref(),
            Some("bt709")
        );
    }

    #[test]
    fn rfc9134_bt2100_without_tcs_is_none() {
        init();
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params(
                "BT2100",
                None,
                None,
                None,
                FmtpColorDefaults::Rfc9134,
            ),
            None
        );
    }

    #[test]
    fn rfc9134_bt2100_sdr_composes_without_defaulting_pq() {
        init();
        let s = gst_colorimetry_from_fmtp_color_params(
            "BT2100",
            Some("SDR"),
            None,
            None,
            FmtpColorDefaults::Rfc9134,
        )
        .unwrap();
        let video_colorimetry = VideoColorimetry::from_str(&s).unwrap();
        assert_eq!(video_colorimetry.matrix(), VideoColorMatrix::Bt2020);
        assert_eq!(video_colorimetry.primaries(), VideoColorPrimaries::Bt2020);
        assert_eq!(
            video_colorimetry.transfer(),
            VideoTransferFunction::Bt202010
        );
        assert_eq!(video_colorimetry.range(), VideoColorRange::Range16_235);
        assert_eq!(s, "bt2020-10");
    }

    #[test]
    fn rfc9134_bt2020_depth_selects_transfer() {
        init();
        let d = FmtpColorDefaults::Rfc9134;

        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("BT2020", None, None, None, d).as_deref(),
            Some("bt2020-10")
        );
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("BT2020", None, None, Some(10), d).as_deref(),
            Some("bt2020-10")
        );
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("BT2020", None, None, Some(12), d).as_deref(),
            Some("bt2020")
        );
        assert_eq!(
            gst_colorimetry_from_fmtp_color_params("BT2100", Some("SDR"), None, Some(12), d)
                .as_deref(),
            Some("bt2020")
        );
    }

    #[test]
    fn rfc9134_full_range_roundtrip() {
        init();
        let d = FmtpColorDefaults::Rfc9134;
        let full = VideoColorimetry::new(
            VideoColorRange::Range0_255,
            VideoColorMatrix::Bt709,
            VideoTransferFunction::Bt709,
            VideoColorPrimaries::Bt709,
        )
        .to_string();
        assert_eq!(
            fmtp_color_params_from_gst_colorimetry(&full),
            Some(FmtpColorParams {
                colorimetry: "BT709",
                tcs: Some("SDR"),
                range: Some("FULL"),
            })
        );
        let back =
            gst_colorimetry_from_fmtp_color_params("BT709", None, Some("FULL"), None, d).unwrap();
        let video_colorimetry = VideoColorimetry::from_str(&back).unwrap();
        assert_eq!(video_colorimetry.range(), VideoColorRange::Range0_255);
        assert_eq!(video_colorimetry.matrix(), VideoColorMatrix::Bt709);
    }

    #[test]
    fn rfc9134_fullprotect_maps_to_full() {
        init();
        let s = gst_colorimetry_from_fmtp_color_params(
            "BT709",
            None,
            Some("FULLPROTECT"),
            None,
            FmtpColorDefaults::Rfc9134,
        )
        .unwrap();
        assert_eq!(
            VideoColorimetry::from_str(&s).unwrap().range(),
            VideoColorRange::Range0_255
        );
    }

    #[test]
    fn rfc9134_unspecified_default_range_is_full() {
        init();
        assert_eq!(
            FmtpColorDefaults::Rfc9134.default_range("UNSPECIFIED"),
            "FULL"
        );
        assert_eq!(FmtpColorDefaults::Rfc9134.default_range("BT709"), "NARROW");
        assert_eq!(
            FmtpColorDefaults::St2110_20.default_range("UNSPECIFIED"),
            "NARROW"
        );
    }

    #[test]
    fn st2110_20_default_tcs_is_sdr() {
        init();
        // With ST 2110-20 defaults, BT2100 without TCS can still resolve via SDR.
        let s = gst_colorimetry_from_fmtp_color_params(
            "BT2100",
            None,
            None,
            None,
            FmtpColorDefaults::St2110_20,
        )
        .unwrap();
        let video_colorimetry = VideoColorimetry::from_str(&s).unwrap();
        assert_eq!(video_colorimetry.matrix(), VideoColorMatrix::Bt2020);
        assert_eq!(video_colorimetry.primaries(), VideoColorPrimaries::Bt2020);
        assert_eq!(
            video_colorimetry.transfer(),
            VideoTransferFunction::Bt202010
        );
        assert_eq!(video_colorimetry.range(), VideoColorRange::Range16_235);
        assert_eq!(s, "bt2020-10");
    }
}
