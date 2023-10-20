// GStreamer Icecast Sink - ogg stream header utils
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use super::imp::CAT;
use crate::icecastsink::mediaformat::AudioInfo;

pub(crate) fn get_streamheaders_from_caps(
    s: &gst::StructureRef,
) -> Result<Vec<gst::Buffer>, gst::LoggableError> {
    let Ok(streamheader_array) = s.get::<gst::ArrayRef>("streamheader") else {
        return Err(gst::loggable_error!(
            CAT,
            "No streamheader field in caps {s}"
        ));
    };

    let mut streamheaders = Vec::new();
    for hdr in streamheader_array.as_slice() {
        streamheaders.push(hdr.get::<gst::Buffer>().unwrap());
    }

    if streamheaders.len() < 2 {
        return Err(gst::loggable_error!(
            CAT,
            "Not enough headers in streamheaders field in caps {s}",
        ));
    };

    Ok(streamheaders)
}

const OGG_PAGE_HEADER_FLAG_CONT: u8 = 0x01;
const OGG_PAGE_HEADER_FLAG_BOS: u8 = 0x02;
const OGG_PAGE_HEADER_FLAG_EOS: u8 = 0x04;

// Don't warn about unused fields
#[allow(dead_code)]
#[derive(Debug)]
struct OggPage<'a> {
    is_cont: bool,
    is_bos: bool,
    is_eos: bool,
    granule_pos: u64,
    stream_serial: u32,
    seqnum: u32,
    segments: Vec<&'a [u8]>,
}

impl OggPage<'_> {
    // TODO: use anyhow perhaps
    fn parse(data: &[u8]) -> Result<OggPage<'_>, String> {
        let mut data = data;

        if data.len() < 28 {
            return Err("Streamheader too short for an Ogg page".into());
        }

        if !data.starts_with(b"OggS\0") {
            return Err("Streamheader is not an Ogg page".into());
        }

        let is_bos = (data[5] & OGG_PAGE_HEADER_FLAG_BOS) != 0;
        let is_eos = (data[5] & OGG_PAGE_HEADER_FLAG_EOS) != 0;
        let is_cont = (data[5] & OGG_PAGE_HEADER_FLAG_CONT) != 0;

        let granule_pos = u64::from_be_bytes([
            data[6], data[7], data[8], data[9], data[10], data[11], data[12], data[13],
        ]);

        let stream_serial = u32::from_be_bytes([data[14], data[15], data[16], data[17]]);

        let seqnum = u32::from_be_bytes([data[18], data[19], data[20], data[21]]);

        let n_segments = data[26] as usize;

        let segment_lens = data
            .get(27..27 + n_segments)
            .ok_or("Streamheader too short for an Ogg page".to_string())?;

        let mut segments = Vec::with_capacity(n_segments);

        data = data
            .get(27 + n_segments..)
            .ok_or("Streamheader too short for an Ogg page".to_string())?;

        gst::trace!(CAT, "segment lengths {segment_lens:?}");

        for len in segment_lens {
            let segment_len = *len as usize;

            if data.len() < segment_len {
                return Err("Error parsing Ogg page segments".to_string());
            }

            let (segment, remainder) = data.split_at(segment_len);

            segments.push(segment);

            data = remainder;
        }

        Ok(OggPage {
            is_cont,
            is_bos,
            is_eos,
            granule_pos,
            stream_serial,
            seqnum,
            segments,
        })
    }
}

pub(crate) fn parse_ogg_audio_streamheaders(
    streamheaders: &[gst::Buffer],
) -> Result<(&'static str, AudioInfo), String> {
    assert!(streamheaders.len() >= 2);

    let map = streamheaders[0]
        .map_readable()
        .map_err(|_| "Could not map streamheader buffer".to_string())?;

    let page = OggPage::parse(map.as_slice())?;

    gst::trace!(CAT, "{page:?}");

    if page.seqnum != 0 || !page.is_bos || page.is_cont || page.is_eos {
        return Err(format!(
            "Unexpected Ogg page seqnum {} or flags in stream header",
            page.seqnum
        ));
    }

    if page.segments.len() != 1 {
        return Err("Expected Ogg page with exactly one segment".into());
    }

    let packet = page.segments[0];

    if packet.len() < 8 {
        return Err("Ogg page segment too small".into());
    }

    let (codec_name, rate, channels) = match packet {
        // Vorbis - https://xiph.org/vorbis/doc/Vorbis_I_spec.html#x1-610004.2
        [1, b'v', b'o', b'r', b'b', b'i', b's', ..] => {
            if packet.len() < 22 {
                return Err("Vorbis identification header too small".into());
            }

            let data = packet.get(7..).unwrap();

            let channels = data[4] as i32;

            let rate = u32::from_le_bytes([data[5], data[6], data[7], data[8]]);

            if rate > 96000 {
                return Err(format!("Unsupported Vorbis sample rate {rate}Hz"));
            }

            let rate = rate as i32;

            let _bitrate_max = u32::from_le_bytes([data[9], data[10], data[11], data[12]]);
            let bitrate = u32::from_le_bytes([data[13], data[14], data[15], data[16]]);
            let _bitrate_min = u32::from_le_bytes([data[17], data[18], data[19], data[20]]);

            gst::info!(
                CAT,
                "Have Vorbis audio: {channels} chans @ {rate} Hz, bitrate {bitrate}"
            );

            ("vorbis", rate, channels)
        }
        // FLAC - https://xiph.org/flac/ogg_mapping.html
        // There's a different mapping as well with just a fLaC id, but let's not worry about that,
        // GStreamer's ogg muxer will produce the right one.
        #[allow(clippy::unusual_byte_groupings)]
        [127, b'F', b'L', b'A', b'C', 1, 0, _, _, b'f', b'L', b'a', b'C', ..] => {
            if packet.len() < 51 {
                return Err("FLAC identification header too small".into());
            }
            // Skip marker + block/frame size fields
            let data = packet.get(13 + 10 + 4..).unwrap();
            let rate_chans = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            let rate = ((rate_chans & 0b1111111111111111111_000_00000_0000) >> 12) as i32;
            let channels = (((rate_chans & 0b0000000000000000000_111_00000_0000) >> 9) + 1) as i32;

            gst::info!(CAT, "Have FLAC audio: {channels} chans @ {rate} Hz");

            ("flac", rate, channels)
        }
        // Opus - https://www.rfc-editor.org/rfc/rfc7845.html#section-5.1
        [b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', 1, channel_count, ..] => {
            if packet.len() < 19 {
                return Err("Opus identification header too small".into());
            }
            let channels = *channel_count as i32;
            let rate = u32::from_le_bytes([packet[12], packet[13], packet[14], packet[15]]);

            if rate > 48000 {
                return Err(format!("Unsupported Opus sample rate {rate}Hz"));
            }

            let rate = rate as i32;

            ("opus", rate, channels)
        }
        _ => return Err("Unsupported Ogg audio codec".into()),
    };

    gst::info!(CAT, "Have {codec_name} audio: {channels} chans @ {rate} Hz");

    Ok((codec_name, AudioInfo { rate, channels }))
}
