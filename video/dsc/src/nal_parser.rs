// Copyright (C) 2025, Fluendo S.A.
//      Author: Diego Nieto <dnieto@fluendo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use anyhow::{Result, bail};
use smallvec::SmallVec;
use std::sync::LazyLock;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "dsc-nal-parser",
        gst::DebugColorFlags::empty(),
        Some("NAL Unit Parser"),
    )
});

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VideoCodec {
    H265,
    H266,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StreamFormat {
    ByteStream,
    LengthPrefixed,
}

impl VideoCodec {
    pub fn from_caps(caps: &gst::CapsRef) -> Result<(Self, StreamFormat)> {
        if let Some(structure) = caps.structure(0) {
            let name = structure.name();
            let stream_format = structure
                .get::<&str>("stream-format")
                .unwrap_or("byte-stream");

            if name == "video/x-h265" {
                let fmt = match stream_format {
                    "byte-stream" => StreamFormat::ByteStream,
                    "hvc1" | "hev1" => StreamFormat::LengthPrefixed,
                    _ => bail!(
                        "Unsupported H.265 stream-format '{}': expected byte-stream/hvc1/hev1",
                        stream_format
                    ),
                };
                Ok((VideoCodec::H265, fmt))
            } else if name == "video/x-h266" {
                let fmt = match stream_format {
                    "byte-stream" => StreamFormat::ByteStream,
                    "vvc1" | "vvi1" => StreamFormat::LengthPrefixed,
                    _ => bail!(
                        "Unsupported H.266 stream-format '{}': expected byte-stream/vvc1/vvi1",
                        stream_format
                    ),
                };
                Ok((VideoCodec::H266, fmt))
            } else {
                bail!("Unsupported codec in caps")
            }
        } else {
            bail!("No structure in caps")
        }
    }
}

#[derive(Clone)]
pub struct NalParser {
    codec: VideoCodec,
    stream_format: StreamFormat,
}

pub type NalUnits = SmallVec<[Vec<u8>; 8]>;

impl NalParser {
    pub fn new(codec: VideoCodec, stream_format: StreamFormat) -> Self {
        gst::info!(
            CAT,
            "Created NAL parser for {:?} with {:?}",
            codec,
            stream_format
        );
        Self {
            codec,
            stream_format,
        }
    }

    pub fn extract_signable_data(&self, data: &[u8]) -> Result<NalUnits> {
        match self.stream_format {
            StreamFormat::ByteStream => self.extract_signable_data_byte_stream(data),
            StreamFormat::LengthPrefixed => self.extract_signable_data_length_prefixed(data),
        }
    }

    fn extract_signable_data_byte_stream(&self, data: &[u8]) -> Result<NalUnits> {
        let mut nal_units = NalUnits::new();
        let mut data = data;
        let mut offset = 0;

        while !data.is_empty() {
            let start_code_len = if data.starts_with(&[0, 0, 0, 1]) {
                4
            } else if data.starts_with(&[0, 0, 1]) {
                3
            } else {
                bail!("Invalid NAL unit - no start code at position {}", offset);
            };

            let payload = &data[start_code_len..];
            let mut end = 0;
            while end < payload.len() {
                if (end + 4 <= payload.len() && payload[end..end + 4] == [0, 0, 0, 1])
                    || (end + 3 <= payload.len() && payload[end..end + 3] == [0, 0, 1])
                {
                    break;
                }
                end += 1;
            }

            let nal_data = &payload[..end];

            let nal_type = self.extract_nal_type_from_header(nal_data)?;

            if self.should_include_nal(nal_type) {
                gst::info!(
                    CAT,
                    "Including NAL type {} ({} bytes) - first 32: {:02x?}",
                    nal_type,
                    nal_data.len(),
                    &nal_data[..std::cmp::min(32, nal_data.len())]
                );

                nal_units.push(nal_data.to_vec());
            }

            let consumed = start_code_len + end;
            data = &data[consumed..];
            offset += consumed;
        }

        gst::info!(
            CAT,
            "Total signable data: {} NAL units extracted",
            nal_units.len()
        );
        Ok(nal_units)
    }

    fn extract_signable_data_length_prefixed(&self, data: &[u8]) -> Result<NalUnits> {
        let mut nal_units = NalUnits::new();
        let mut data = data;
        let mut offset = 0;

        while !data.is_empty() {
            if data.len() < 4 {
                bail!(
                    "Invalid length-prefixed NAL unit - truncated length field at position {}",
                    offset
                );
            }

            let nal_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
            data = &data[4..];
            offset += 4;

            if nal_len == 0 {
                continue;
            }

            if nal_len > data.len() {
                bail!(
                    "Invalid length-prefixed NAL unit - size {} exceeds remaining bytes {}",
                    nal_len,
                    data.len()
                );
            }

            let nal_data = &data[..nal_len];
            data = &data[nal_len..];
            offset += nal_len;

            let nal_type = self.extract_nal_type_from_header(nal_data)?;
            if self.should_include_nal(nal_type) {
                gst::info!(
                    CAT,
                    "Including NAL type {} ({} bytes, length-prefixed) - first 32: {:02x?}",
                    nal_type,
                    nal_data.len(),
                    &nal_data[..std::cmp::min(32, nal_data.len())]
                );
                nal_units.push(nal_data.to_vec());
            }
        }

        gst::info!(
            CAT,
            "Total signable data: {} NAL units extracted",
            nal_units.len()
        );
        Ok(nal_units)
    }

    fn extract_nal_type_from_header(&self, nal_data: &[u8]) -> Result<u8> {
        if nal_data.is_empty() {
            bail!("Empty NAL data");
        }

        let nal_type = match self.codec {
            VideoCodec::H265 => {
                if nal_data.len() < 2 {
                    bail!("H.265 NAL header too short");
                }
                (nal_data[0] >> 1) & 0x3F
            }
            VideoCodec::H266 => {
                if nal_data.len() < 2 {
                    bail!("H.266 NAL header too short");
                }
                (nal_data[1] >> 3) & 0x1F
            }
        };

        Ok(nal_type)
    }

    fn should_include_nal(&self, nal_type: u8) -> bool {
        match self.codec {
            VideoCodec::H265 => self.should_include_h265_nal(nal_type),
            VideoCodec::H266 => self.should_include_h266_nal(nal_type),
        }
    }

    fn should_include_h265_nal(&self, nal_type: u8) -> bool {
        match nal_type {
            // VCL NAL units
            0..=31 => true, // Coded slice units (VCL)

            // Non-VCL parameter sets
            32 => true, // VPS
            33 => true, // SPS
            34 => true, // PPS

            // Excluded Non-VCL units
            35 => false, // AUD
            38 => false, // Filler
            39 => false, // PREFIX_SEI
            40 => false, // SUFFIX_SEI
            _ => false,
        }
    }

    fn should_include_h266_nal(&self, nal_type: u8) -> bool {
        match nal_type {
            // VCL NAL units: TRAIL..RSV_IRAP_VCL_11
            0..=11 => true,

            // Non-VCL units signed by VTM's DSC encoder
            14 => true, // VPS
            15 => true, // SPS
            16 => true, // PPS
            17 => true, // PREFIX_APS
            18 => true, // SUFFIX_APS
            19 => true, // PH

            // Non-VCL units not added to the DSC substreams
            12 => false,      // OPI
            13 => false,      // DCI
            20 => false,      // AUD
            21 => false,      // EOS
            22 => false,      // EOB
            23 => false,      // PREFIX_SEI
            24 => false,      // SUFFIX_SEI
            25 => false,      // FD (Filler Data)
            26..=31 => false, // RESERVED_NVCL_26..27, UNSPECIFIED_28..31

            _ => {
                gst::warning!(CAT, "Unknown H.266 NAL type: {}", nal_type);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h265_nal(nal_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0x00, 0x00, 0x00, 0x01];
        out.push((nal_type << 1) & 0x7E);
        out.push(0x01);
        out.extend_from_slice(payload);
        out
    }

    fn h265_nal_length_prefixed(nal_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut nal = Vec::new();
        nal.push((nal_type << 1) & 0x7E);
        nal.push(0x01);
        nal.extend_from_slice(payload);

        let mut out = Vec::new();
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(&nal);
        out
    }

    fn h266_nal_length_prefixed(nal_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut nal = Vec::new();
        nal.push(0x00);
        nal.push((nal_type << 3) & 0xF8);
        nal.extend_from_slice(payload);

        let mut out = Vec::new();
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(&nal);
        out
    }

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
        });
    }

    #[test]
    fn test_h266_support() {
        init();

        let caps = gst::Caps::builder("video/x-h266").build();
        let (codec, stream_format) = VideoCodec::from_caps(&caps).unwrap();
        assert_eq!(codec, VideoCodec::H266);
        assert_eq!(stream_format, StreamFormat::ByteStream);

        let _parser = NalParser::new(VideoCodec::H266, StreamFormat::ByteStream);
    }

    #[test]
    fn test_h265_extract_signable_data_hm_minimum_set() {
        init();

        let mut data = Vec::new();
        // VPS/SPS/PPS + one VCL + one PREFIX_SEI (excluded)
        data.extend_from_slice(&h265_nal(32, &[0xAA]));
        data.extend_from_slice(&h265_nal(33, &[0xBB]));
        data.extend_from_slice(&h265_nal(34, &[0xCC]));
        data.extend_from_slice(&h265_nal(1, &[0xDD]));
        data.extend_from_slice(&h265_nal(39, &[0xEE]));

        let parser = NalParser::new(VideoCodec::H265, StreamFormat::ByteStream);
        let nal_units = parser.extract_signable_data(&data).unwrap();

        assert_eq!(nal_units.len(), 4, "Expected VPS/SPS/PPS/VCL only");

        let nal_types: Vec<u8> = nal_units.iter().map(|n| (n[0] >> 1) & 0x3F).collect();

        assert_eq!(nal_types, vec![32, 33, 34, 1]);
    }

    #[test]
    fn test_h265_hvc1_and_hev1_caps_support() {
        init();

        let caps_hvc1 = gst::Caps::builder("video/x-h265")
            .field("stream-format", "hvc1")
            .field("alignment", "au")
            .build();
        let (codec_hvc1, fmt_hvc1) = VideoCodec::from_caps(&caps_hvc1).unwrap();
        assert_eq!(codec_hvc1, VideoCodec::H265);
        assert_eq!(fmt_hvc1, StreamFormat::LengthPrefixed);

        let caps_hev1 = gst::Caps::builder("video/x-h265")
            .field("stream-format", "hev1")
            .field("alignment", "au")
            .build();
        let (codec_hev1, fmt_hev1) = VideoCodec::from_caps(&caps_hev1).unwrap();
        assert_eq!(codec_hev1, VideoCodec::H265);
        assert_eq!(fmt_hev1, StreamFormat::LengthPrefixed);
    }

    #[test]
    fn test_h266_vvc1_and_vvi1_caps_support() {
        init();

        let caps_vvc1 = gst::Caps::builder("video/x-h266")
            .field("stream-format", "vvc1")
            .field("alignment", "au")
            .build();
        let (codec_vvc1, fmt_vvc1) = VideoCodec::from_caps(&caps_vvc1).unwrap();
        assert_eq!(codec_vvc1, VideoCodec::H266);
        assert_eq!(fmt_vvc1, StreamFormat::LengthPrefixed);

        let caps_vvi1 = gst::Caps::builder("video/x-h266")
            .field("stream-format", "vvi1")
            .field("alignment", "au")
            .build();
        let (codec_vvi1, fmt_vvi1) = VideoCodec::from_caps(&caps_vvi1).unwrap();
        assert_eq!(codec_vvi1, VideoCodec::H266);
        assert_eq!(fmt_vvi1, StreamFormat::LengthPrefixed);
    }

    #[test]
    fn test_h265_length_prefixed_extract_signable_data_hm_minimum_set() {
        init();

        let mut data = Vec::new();
        data.extend_from_slice(&h265_nal_length_prefixed(32, &[0xAA]));
        data.extend_from_slice(&h265_nal_length_prefixed(33, &[0xBB]));
        data.extend_from_slice(&h265_nal_length_prefixed(34, &[0xCC]));
        data.extend_from_slice(&h265_nal_length_prefixed(1, &[0xDD]));
        data.extend_from_slice(&h265_nal_length_prefixed(39, &[0xEE]));

        let parser = NalParser::new(VideoCodec::H265, StreamFormat::LengthPrefixed);
        let nal_units = parser.extract_signable_data(&data).unwrap();

        assert_eq!(nal_units.len(), 4, "Expected VPS/SPS/PPS/VCL only");

        let nal_types: Vec<u8> = nal_units.iter().map(|n| (n[0] >> 1) & 0x3F).collect();

        assert_eq!(nal_types, vec![32, 33, 34, 1]);
    }

    #[test]
    fn test_h266_length_prefixed_extract_signable_data_supported_set() {
        init();

        let mut data = Vec::new();
        data.extend_from_slice(&h266_nal_length_prefixed(14, &[0xA1])); // VPS include
        data.extend_from_slice(&h266_nal_length_prefixed(15, &[0xA2])); // SPS include
        data.extend_from_slice(&h266_nal_length_prefixed(16, &[0xA3])); // PPS include
        data.extend_from_slice(&h266_nal_length_prefixed(17, &[0xA4])); // PREFIX_APS include
        data.extend_from_slice(&h266_nal_length_prefixed(18, &[0xA5])); // SUFFIX_APS include
        data.extend_from_slice(&h266_nal_length_prefixed(19, &[0xA6])); // PH include
        data.extend_from_slice(&h266_nal_length_prefixed(12, &[0xA7])); // OPI exclude
        data.extend_from_slice(&h266_nal_length_prefixed(13, &[0xA8])); // DCI exclude
        data.extend_from_slice(&h266_nal_length_prefixed(20, &[0xA9])); // AUD exclude
        data.extend_from_slice(&h266_nal_length_prefixed(23, &[0xAA])); // PREFIX_SEI exclude
        data.extend_from_slice(&h266_nal_length_prefixed(0, &[0xAB])); // VCL include

        let parser = NalParser::new(VideoCodec::H266, StreamFormat::LengthPrefixed);
        let nal_units = parser.extract_signable_data(&data).unwrap();

        assert_eq!(
            nal_units.len(),
            7,
            "Expected VPS/SPS/PPS/PREFIX_APS/SUFFIX_APS/PH/VCL only"
        );

        let nal_types: Vec<u8> = nal_units.iter().map(|n| (n[1] >> 3) & 0x1F).collect();

        assert_eq!(nal_types, vec![14, 15, 16, 17, 18, 19, 0]);
    }
}
