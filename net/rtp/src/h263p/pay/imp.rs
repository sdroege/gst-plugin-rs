// GStreamer RTP H.263+ Payloader
//
// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtph263ppay2
 * @see_also: rtph263pdepay2, avenc_h263p
 *
 * Payloads an H.263+ (H.263-1998) or H.263++ (H.263-2000) video stream into
 * RTP packets as per [RFC 4629][rfc-4629].
 *
 * Each input buffer must contain exactly one complete picture. Frames that
 * don't fit into the configured MTU are fragmented either with follow-on
 * packets or at segment boundaries, depending on the `fragmentation-mode`
 * property. Segment boundaries are picture, GOB, slice, EOS and EOSBS start
 * codes; encoders should be configured to emit GOB or slice start codes for
 * this to be useful.
 *
 * The SDP annex parameters (`f`, `i`, `j`, `t`, `k`, `n`) are derived from
 * `annex-*` fields of the input caps; without them receivers must assume
 * Baseline H.263 (RFC 4629 section 8.1.1). Placing a parser such as
 * `h263parse` before this element provides those fields.
 *
 * [rfc-4629]: https://www.rfc-editor.org/rfc/rfc4629.html
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 videotestsrc ! video/x-raw,format=I420,width=176,height=144 ! avenc_h263p ! rtph263ppay2 ! udpsink2 host=127.0.0.1 port=5004
 * ]| This will encode a test pattern as H.263+ and payload it as RTP to
 * localhost port 5004.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::{glib, prelude::*, subclass::prelude::*};

use std::ops::Range;
use std::sync::LazyLock;

use crate::basepay::{RtpBasePay2Ext, RtpBasePay2Impl};
use crate::h263p::startcode::{is_sequence_end, segment_offsets, starts_with_startcode};

use super::FragmentationMode;

/// Size of the general H.263+ payload header (RFC 4629 section 5.1).
const PAYLOAD_HEADER_LEN: usize = 2;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtph263ppay2",
        gst::DebugColorFlags::empty(),
        Some("RTP H.263+ Payloader"),
    )
});

#[derive(Default)]
struct Settings {
    fragmentation_mode: FragmentationMode,
}

#[derive(Default)]
pub struct RtpH263PPay {
    settings: std::sync::Mutex<Settings>,
}

impl RtpH263PPay {
    fn fragmentation_mode(&self) -> FragmentationMode {
        self.settings.lock().unwrap().fragmentation_mode
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RtpH263PPay {
    const NAME: &'static str = "GstRtpH263PPay2";
    type Type = super::RtpH263PPay;
    type ParentType = crate::basepay::RtpBasePay2;
}

impl ObjectImpl for RtpH263PPay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecEnum::builder_with_default(
                    "fragmentation-mode",
                    Settings::default().fragmentation_mode,
                )
                .nick("Fragmentation mode")
                .blurb(
                    "How to fragment frames larger than the MTU: follow-on packets \
                 ('normal') or packets that start at segment boundaries ('sync')",
                )
                .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "fragmentation-mode" => {
                self.settings.lock().unwrap().fragmentation_mode = value
                    .get::<FragmentationMode>()
                    .expect("type checked upstream");
            }
            name => unimplemented!("Property '{name}'"),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "fragmentation-mode" => self.fragmentation_mode().to_value(),
            name => unimplemented!("Property '{name}'"),
        }
    }
}

impl GstObjectImpl for RtpH263PPay {}

impl ElementImpl for RtpH263PPay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP H.263+ Payloader",
                "Codec/Payloader/Network/RTP",
                "Payload an H.263+/H.263++ video stream into RTP packets (RFC 4629)",
                "Edward Hervey <edward@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                // One complete picture per buffer is required for the
                // fragmented framing below.
                &gst::Caps::builder("video/x-h263")
                    .field("variant", "itu")
                    .field("parsed", true)
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder_full()
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("encoding-name", "H263-1998")
                            .field("clock-rate", 90_000i32)
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("encoding-name", "H263-2000")
                            .field("clock-rate", 90_000i32)
                            .build(),
                    )
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl RtpBasePay2Impl for RtpH263PPay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["video"];

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        // The sink pad template is a single fixed structure.
        let s = caps.structure(0).unwrap();

        // The encoding-name follows the stream: only H.263 (2000) content
        // may be signalled as H263-2000 (RFC 4629 section 8.1).
        let encoding_name = match s.get::<&str>("h263version").ok() {
            Some("h263pp") => "H263-2000",
            _ => "H263-1998",
        };

        let mut builder = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", encoding_name)
            .field("clock-rate", 90_000i32);

        // RFC 4629 section 8.1: annex SDP parameters. An upstream parser
        // (h263parse) may describe the stream annexes in the input caps;
        // propagate them, otherwise receivers must assume Baseline
        // (section 8.1.1).
        for (field, param) in [
            ("annex-f", "f"),
            ("annex-i", "i"),
            ("annex-j", "j"),
            ("annex-t", "t"),
            ("annex-k", "k"),
            ("annex-n", "n"),
        ] {
            if s.get::<bool>(field).unwrap_or(false) {
                builder = builder.field(param, "1");
            }
        }
        let src_caps = builder.build();

        gst::debug!(CAT, imp = self, "Negotiated src caps: {src_caps:?}");
        self.obj().set_src_caps(&src_caps);

        true
    }

    fn handle_buffer(
        &self,
        buffer: &gst::Buffer,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let map = buffer.map_readable().map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Read,
                ["Failed to map buffer readable"]
            );

            gst::FlowError::Error
        })?;
        let data = map.as_slice();

        if !starts_with_startcode(data) {
            gst::element_imp_error!(
                self,
                gst::StreamError::Format,
                ["Buffer does not start with an H.263+ start code"]
            );
            return Err(gst::FlowError::Error);
        }

        let max_payload_size = (self.obj().max_payload_size() as usize)
            .checked_sub(PAYLOAD_HEADER_LEN)
            .filter(|size| *size > 0)
            .ok_or_else(|| {
                gst::element_imp_error!(
                    self,
                    gst::LibraryError::Settings,
                    ["MTU too small to add payload header"]
                );
                gst::FlowError::Error
            })?;

        match self.fragmentation_mode() {
            FragmentationMode::Normal => self.packetize_follow_on(data, id, max_payload_size),
            FragmentationMode::Sync => self.packetize_segments(data, id, max_payload_size),
        }
    }
}

impl RtpH263PPay {
    /// Packetize one picture with follow-on packets (RFC 4629 section 6.2).
    ///
    /// The first packet carries P=1 and the picture start code minus its two
    /// leading zero bytes; all further packets carry P=0 and continue the
    /// bitstream at an arbitrary bit position.
    fn packetize_follow_on(
        &self,
        data: &[u8],
        id: u64,
        max_payload_size: usize,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // The two leading zero bytes of the picture start code are omitted and
        // represented by the P bit.
        let chunks = data[2..].chunks(max_payload_size);
        let num_chunks = chunks.len();

        for (index, chunk) in chunks.enumerate() {
            let p_bit = if index == 0 { 0x04 } else { 0x00 };
            let last = index + 1 == num_chunks;

            gst::trace!(
                CAT,
                imp = self,
                "Queueing follow-on packet, P={}, {} bytes of payload, marker={}",
                p_bit != 0,
                chunk.len(),
                last
            );

            self.obj().queue_packet(
                id.into(),
                rtp_types::RtpPacketBuilder::new()
                    .marker_bit(last)
                    .payload(&[p_bit, 0x00][..])
                    .payload(chunk),
            )?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    /// Packetize one picture into picture segment packets (RFC 4629 section
    /// 6.1).
    ///
    /// Every packet starts with a segment start code (minus its two leading
    /// zero bytes, represented by the P bit). As many complete segments as fit
    /// are packed into each packet; a segment larger than the MTU is continued
    /// with follow-on packets.
    fn packetize_segments(
        &self,
        data: &[u8],
        id: u64,
        max_payload_size: usize,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let packets = segment_packets(data, max_payload_size);

        for (packet_index, packet) in packets.iter().enumerate() {
            let chunks = data[packet.clone()].chunks(max_payload_size);
            let num_chunks = chunks.len();
            let last_packet = packet_index + 1 == packets.len();

            for (chunk_index, chunk) in chunks.enumerate() {
                let p_bit = if chunk_index == 0 { 0x04 } else { 0x00 };
                let last = last_packet && chunk_index + 1 == num_chunks;

                gst::trace!(
                    CAT,
                    imp = self,
                    "Queueing segment packet, P={}, {} bytes of payload, marker={}",
                    p_bit != 0,
                    chunk.len(),
                    last
                );

                self.obj().queue_packet(
                    id.into(),
                    rtp_types::RtpPacketBuilder::new()
                        .marker_bit(last)
                        .payload(&[p_bit, 0x00][..])
                        .payload(chunk),
                )?;
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

/// Group the segments of a picture into packets.
///
/// A packet starts at a segment start code, omitting its two leading zero
/// bytes (represented by the P bit), and holds as many following complete
/// segments as fit into `max_payload_size`. Sequence-ending segments
/// (EOS/EOSBS) always get a packet of their own (RFC 4629 section 6.1.3).
///
/// The returned ranges start after the omitted start code bytes.
fn segment_packets(data: &[u8], max_payload_size: usize) -> Vec<Range<usize>> {
    let mut offsets = segment_offsets(data);
    offsets.push(data.len());

    // One segment per pair of consecutive start codes, the last segment
    // ending at the end of the picture.
    let mut packets = Vec::new();
    let mut packet: Option<Range<usize>> = None;
    let mut packet_sequence_end = false;

    for offsets in offsets.windows(2) {
        let [start, end] = [offsets[0], offsets[1]];
        let segment = start + 2..end;
        let sequence_end = is_sequence_end(data, start);

        match packet.take() {
            // A merged segment keeps its start code: the packet grows to
            // `end - current.start` bytes, not by `segment.len()`.
            Some(mut current)
                if !packet_sequence_end
                    && !sequence_end
                    && end - current.start <= max_payload_size =>
            {
                current.end = end;
                packet = Some(current);
            }
            previous => {
                if let Some(previous) = previous {
                    packets.push(previous);
                }
                packet = Some(segment);
                packet_sequence_end = sequence_end;
            }
        }
    }

    if let Some(packet) = packet {
        packets.push(packet);
    }

    packets
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One segment of `len` bytes with the start code `00 00 <code>`; the
    /// filler never forms a start code.
    fn segment(code: u8, len: usize) -> Vec<u8> {
        let mut segment = vec![0x00, 0x00, code];
        segment.resize(len, 0x11);
        segment
    }

    #[test]
    fn segment_packets_single_segment() {
        let data = segment(0x80, 6);

        assert_eq!(segment_packets(&data, 100), vec![2..6]);
    }

    #[test]
    fn segment_packets_packs_fitting_segments() {
        let mut data = segment(0x80, 4);
        data.extend(segment(0x81, 4));

        assert_eq!(segment_packets(&data, 100), vec![2..8]);
    }

    #[test]
    fn segment_packets_splits_segments_that_do_not_fit() {
        let mut data = segment(0x80, 6);
        data.extend(segment(0x81, 6));

        assert_eq!(segment_packets(&data, 5), vec![2..6, 8..12]);
    }

    #[test]
    fn segment_packets_counts_the_retained_start_code() {
        // The first segment contributes 1 byte (its two leading zero bytes
        // are omitted), the second its full 5 bytes: 6 bytes do not fit 5.
        let mut data = segment(0x80, 3);
        data.extend(segment(0x81, 5));

        assert_eq!(segment_packets(&data, 5), vec![2..3, 5..8]);
    }

    #[test]
    fn segment_packets_merges_at_the_mtu_boundary() {
        let mut data = segment(0x80, 3);
        data.extend(segment(0x81, 5));

        assert_eq!(segment_packets(&data, 6), vec![2..8]);
    }

    #[test]
    fn segment_packets_keeps_oversized_segment_whole() {
        // Chunking to the MTU happens when the packet is emitted.
        let data = segment(0x80, 10);

        assert_eq!(segment_packets(&data, 3), vec![2..10]);
    }

    #[test]
    fn segment_packets_isolates_sequence_end() {
        let mut data = segment(0x80, 4);
        data.extend(segment(0xFC, 4)); // EOS
        data.extend(segment(0x81, 4));

        assert_eq!(segment_packets(&data, 100), vec![2..4, 6..8, 10..12]);
    }

    #[test]
    fn segment_packets_isolates_trailing_sequence_end() {
        let mut data = segment(0x80, 4);
        data.extend(segment(0xF8, 4)); // EOSBS

        assert_eq!(segment_packets(&data, 100), vec![2..4, 6..8]);
    }

    #[test]
    fn segment_packets_isolates_consecutive_sequence_ends() {
        let mut data = segment(0xFC, 4); // EOS
        data.extend(segment(0xFD, 4)); // EOS

        assert_eq!(segment_packets(&data, 100), vec![2..4, 6..8]);
    }
}
