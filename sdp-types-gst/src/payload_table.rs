// Copyright (C) 2024 Nirbheek Chauhan <nirbheek@centricular.com>
// Copyright (C) 2025 Matthew Waters <matthew@centricular.com>
// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! RTP payload type tables, used to fill in missing rtpmap details.

// Based on the static/dynamic tables in rtspsrc2
// (`net/rtsp/src/rtspsrc/sdp.rs`), checked against the C table in
// `gst-plugins-base/gst-libs/gst/rtp/gstrtppayloads.c` (IANA registry plus
// known non-registered encodings). The "reserved" and "unassigned"
// placeholder entries from the C table are dropped, and the entries only
// present there (H266, AV1, x-pn-*) are added back.

macro_rules! init_payload_info {
    ($($t:expr),*,) => {
        [
            $(
                {
                    let (pt, media, encoding_name, clock_rate, encoding_params) = $t;
                    PayloadInfo {
                        pt,
                        media,
                        encoding_name,
                        clock_rate,
                        encoding_params,
                    }
                }
            ),*
        ]
    };
}

pub(crate) struct PayloadInfo<'a> {
    pub(crate) pt: u8,
    pub(crate) media: &'a str,
    pub(crate) encoding_name: &'a str,
    pub(crate) clock_rate: u32,
    pub(crate) encoding_params: Option<&'a str>,
}

// Static payload types, can be looked up by payload type.
static STATIC_PAYLOAD_INFO: &[PayloadInfo] = &init_payload_info!(
    (0, "audio", "PCMU", 8000, Some("1")),
    (3, "audio", "GSM", 8000, Some("1")),
    (4, "audio", "G723", 8000, Some("1")),
    (5, "audio", "DVI4", 8000, Some("1")),
    (6, "audio", "DVI4", 16000, Some("1")),
    (7, "audio", "LPC", 8000, Some("1")),
    (8, "audio", "PCMA", 8000, Some("1")),
    (9, "audio", "G722", 8000, Some("1")),
    (10, "audio", "L16", 44100, Some("2")),
    (11, "audio", "L16", 44100, Some("1")),
    (12, "audio", "QCELP", 8000, Some("1")),
    (13, "audio", "CN", 8000, Some("1")),
    (14, "audio", "MPA", 90000, None),
    (15, "audio", "G728", 8000, Some("1")),
    (16, "audio", "DVI4", 11025, Some("1")),
    (17, "audio", "DVI4", 22050, Some("1")),
    (18, "audio", "G729", 8000, Some("1")),
    (25, "video", "CelB", 90000, None),
    (26, "video", "JPEG", 90000, None),
    (28, "video", "nv", 90000, None),
    (31, "video", "H261", 90000, None),
    (32, "video", "MPV", 90000, None),
    (33, "video", "MP2T", 90000, None),
    (34, "video", "H263", 90000, None),
);

// Known encodings with dynamic payload types, can only be matched by name.
static DYNAMIC_PAYLOAD_INFO: &[PayloadInfo] = &init_payload_info!(
    (0, "application", "parityfec", 0, None),
    (0, "application", "rtx", 0, None),
    (0, "audio", "AMR", 8000, None),
    (0, "audio", "AMR-WB", 16000, None),
    (0, "audio", "DAT12", 0, None),
    (0, "audio", "dsr-es201108", 0, None),
    (0, "audio", "EVRC", 8000, Some("1")),
    (0, "audio", "EVRC0", 8000, Some("1")),
    (0, "audio", "EVRC1", 8000, Some("1")),
    (0, "audio", "EVRCB", 8000, Some("1")),
    (0, "audio", "EVRCB0", 8000, Some("1")),
    (0, "audio", "EVRCB1", 8000, Some("1")),
    (0, "audio", "G7221", 16000, Some("1")),
    (0, "audio", "G726-16", 8000, Some("1")),
    (0, "audio", "G726-24", 8000, Some("1")),
    (0, "audio", "G726-32", 8000, Some("1")),
    (0, "audio", "G726-40", 8000, Some("1")),
    (0, "audio", "G729D", 8000, Some("1")),
    (0, "audio", "G729E", 8000, Some("1")),
    (0, "audio", "GSM-EFR", 8000, Some("1")),
    (0, "audio", "L8", 0, None),
    (0, "audio", "RED", 0, None),
    (0, "audio", "rtx", 0, None),
    (0, "audio", "VDVI", 0, Some("1")),
    (0, "audio", "L20", 0, None),
    (0, "audio", "L24", 0, None),
    (0, "audio", "MP4A-LATM", 0, None),
    (0, "audio", "mpa-robust", 90000, None),
    (0, "audio", "parityfec", 0, None),
    (0, "audio", "SMV", 8000, Some("1")),
    (0, "audio", "SMV0", 8000, Some("1")),
    (0, "audio", "t140c", 0, None),
    (0, "audio", "t38", 0, None),
    (0, "audio", "telephone-event", 0, None),
    (0, "audio", "tone", 0, None),
    (0, "audio", "DVI4", 0, None),
    (0, "audio", "G722", 0, None),
    (0, "audio", "G723", 0, None),
    (0, "audio", "G728", 0, None),
    (0, "audio", "G729", 0, None),
    (0, "audio", "GSM", 0, None),
    (0, "audio", "L16", 0, None),
    (0, "audio", "LPC", 0, None),
    (0, "audio", "PCMA", 0, None),
    (0, "audio", "PCMU", 0, None),
    (0, "text", "parityfec", 0, None),
    (0, "text", "red", 1000, None),
    (0, "text", "rtx", 0, None),
    (0, "text", "t140", 1000, None),
    (0, "video", "BMPEG", 90000, None),
    (0, "video", "BT656", 90000, None),
    (0, "video", "DV", 90000, None),
    (0, "video", "H263-1998", 90000, None),
    (0, "video", "H263-2000", 90000, None),
    (0, "video", "MP1S", 90000, None),
    (0, "video", "MP2P", 90000, None),
    (0, "video", "MP4V-ES", 90000, None),
    (0, "video", "parityfec", 0, None),
    (0, "video", "pointer", 90000, None),
    (0, "video", "raw", 90000, None),
    (0, "video", "rtx", 0, None),
    (0, "video", "SMPTE292M", 0, None),
    (0, "video", "vc1", 90000, None),
    (0, "audio", "AC3", 0, None),
    (0, "audio", "ILBC", 8000, None),
    (0, "audio", "MPEG4-GENERIC", 0, None),
    (0, "audio", "SPEEX", 0, None),
    (0, "audio", "OPUS", 48000, None),
    (0, "application", "MPEG4-GENERIC", 0, None),
    (0, "video", "H264", 90000, None),
    (0, "video", "H265", 90000, None),
    (0, "video", "H266", 90000, None),
    (0, "video", "AV1", 90000, None),
    (0, "video", "MPEG4-GENERIC", 90000, None),
    (0, "video", "THEORA", 0, None),
    (0, "video", "VORBIS", 0, None),
    (0, "video", "X-SV3V-ES", 90000, None),
    (0, "video", "X-SORENSON-VIDEO", 90000, None),
    (0, "video", "VP8", 90000, None),
    (0, "video", "VP9", 90000, None),
    // RealNetworks codecs, from the table in gstrtppayloads.c.
    (0, "application", "x-pn-realmedia", 1000, None),
    (0, "audio", "x-pn-realaudio", 1000, None),
    (0, "video", "x-pn-realvideo", 1000, None),
);

pub(crate) fn guess_payload_info(
    media_type: &str,
    pt: u8,
    encoding_name: Option<&str>,
) -> Option<&'static PayloadInfo<'static>> {
    if pt < 96 {
        STATIC_PAYLOAD_INFO
            .iter()
            .find(|info| info.pt == pt && info.media == media_type)
    } else {
        let encoding_name = encoding_name?;
        DYNAMIC_PAYLOAD_INFO.iter().find(|info| {
            info.media == media_type && info.encoding_name.eq_ignore_ascii_case(encoding_name)
        })
    }
}
