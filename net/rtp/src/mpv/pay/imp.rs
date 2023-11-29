// GStreamer RTP MPEG-1/MPEG-2 Video Elementary Stream Payloader
//
// Copyright (C) 2023-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpmpvpay2
 * @see_also: rtpmpvdepay2, rtpmpvdepay, rtpmpvpay, mpeg2enc, avdec_mpeg2video, avenc_mpeg2video
 *
 * Payload an MPEG-1 or MPEG-2 Video Elementary Stream into RTP packets as per [RFC 2250][rfc-2250].
 *
 * [rfc-2250]: https://www.rfc-editor.org/rfc/rfc2250#section-3
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 videotestsrc ! video/x-raw,width=1280,height=720,format=I420 ! timeoverlay font-desc=Sans,22 ! avenc_mpeg2video ! rtpmpvpay2 ! udpsink host=127.0.0.1 port=5004
 * ]| This will create and payload an MPEG-2 Video elementary stream with a test pattern and
 * send it out via UDP to localhost port 5004.
 *
 * Since: plugins-rs-0.16.0
 */
use atomic_refcell::AtomicRefCell;

use std::sync::LazyLock;

use gst::{prelude::*, subclass::prelude::*};

use crate::basepay::RtpBasePay2Ext;

use crate::mpv::mpeg_video_packet;
use crate::mpv::mpeg_video_packet::{Packet, PacketType, PictureHeader};
use crate::mpv::packet_header;

const RTP_MPEG_VIDEO_DEFAULT_PT: u32 = 32;

// https://datatracker.ietf.org/doc/html/rfc2250#section-3.4
// We only add the general header for now, but there's also
// an MPEG-2 specific header extension.
const MPEG_VIDEO_SPECIFIC_HEADER_LEN: usize = 4;

#[derive(Default)]
pub struct RtpMpegVideoPay {
    state: AtomicRefCell<State>,
}

#[derive(Default)]
struct State {
    // Last sequence header we saw
    sequence_header: Option<Vec<u8>>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpmpvpay2",
        gst::DebugColorFlags::empty(),
        Some("RTP MPEG-2 Video Payloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpMpegVideoPay {
    const NAME: &'static str = "GstRtpMpegVideoPay2";
    type Type = super::RtpMpegVideoPay;
    type ParentType = crate::basepay::RtpBasePay2;
}

impl ObjectImpl for RtpMpegVideoPay {
    fn constructed(&self) {
        self.parent_constructed();

        self.obj().set_property("pt", RTP_MPEG_VIDEO_DEFAULT_PT);
    }
}

impl GstObjectImpl for RtpMpegVideoPay {}

impl ElementImpl for RtpMpegVideoPay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP MPEG Video Elementary Stream Payloader",
                "Codec/Payloader/Network/RTP",
                "Payload an MPEG-1 or MPEG-2 Elementary Stream into RTP packets (RFC 2250)",
                "Tim-Philipp Müller <tim centricular com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder_full()
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("encoding-name", "MPV")
                            .field("clock-rate", 90000i32)
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("payload", RTP_MPEG_VIDEO_DEFAULT_PT as i32)
                            .field("clock-rate", 90000i32)
                            .build(),
                    )
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/mpeg")
                    .field("systemstream", false)
                    .field("mpegversion", gst::IntRange::new(1i32, 2))
                    .field("parsed", true)
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basepay::RtpBasePay2Impl for RtpMpegVideoPay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["video"];

    fn set_sink_caps(&self, _caps: &gst::Caps) -> bool {
        let src_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", "MPV")
            .field("clock-rate", 90000i32)
            .build();

        self.obj().set_src_caps(&src_caps);

        true
    }

    // Encapsulation of MPEG Video Elementary Streams:
    // https://datatracker.ietf.org/doc/html/rfc2250#section-3.1
    //
    fn handle_buffer(
        &self,
        buffer: &gst::Buffer,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let map = buffer.map_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "Can't map buffer readable");
            gst::FlowError::Error
        })?;

        gst::trace!(
            CAT,
            imp = self,
            "Got frame with id {id}, {} bytes",
            map.size()
        );

        // Parse frame into packets

        let packets = mpeg_video_packet::parse_packets_from_slice(&map);

        let Ok(packets) = packets else {
            gst::element_imp_error!(
                self,
                gst::StreamError::Format,
                ["Could not parse MPEG-2 video frame"]
            );
            return Err(gst::FlowError::Error);
        };

        // Log packets

        for (i, packet) in packets.iter().enumerate() {
            gst::trace!(
                CAT,
                imp = self,
                "Buf {id}, Packet {i}: {:?} @ {}+{}",
                packet.ptype(),
                packet.offset(),
                packet.len(),
            );
        }

        // Headers are everything before the first slice

        let headers = {
            // Find the first slice
            let first_slice = packets.iter().position(|p| p.ptype() == PacketType::Slice);

            let Some(first_slice) = first_slice else {
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Format,
                    ["MPEG-2 video frame without any slices"]
                );
                return Err(gst::FlowError::Error);
            };

            &packets[..first_slice]
        };

        // Make sure we have a Picture header

        let pic_hdr = headers
            .iter()
            .position(|p| p.ptype() == PacketType::Picture);

        let Some(pic_hdr_idx) = pic_hdr else {
            gst::element_imp_error!(
                self,
                gst::StreamError::Format,
                ["MPEG-2 video frame without picture header"]
            );
            return Err(gst::FlowError::Error);
        };

        // Some optional headers

        let seq_hdr = headers
            .iter()
            .position(|p| p.ptype() == PacketType::Sequence);

        let gop_hdr = headers.iter().position(|p| p.ptype() == PacketType::Gop);

        // Only picture header is required, but order should always be Sequence - Gop - Picture

        let header_order_ok =
            if let Some((seq_hdr_idx, gop_hdr_idx)) = Option::zip(seq_hdr, gop_hdr) {
                seq_hdr_idx < gop_hdr_idx && gop_hdr_idx < pic_hdr_idx
            } else if let Some(gop_hdr_idx) = gop_hdr {
                gop_hdr_idx < pic_hdr_idx
            } else {
                true
            };

        if !header_order_ok {
            gst::element_imp_error!(
                self,
                gst::StreamError::Format,
                ["MPEG-2 video frame with unexpected header ordering"]
            );
            return Err(gst::FlowError::Error);
        }

        // Parse Picture header

        // We require parsed input, so this should be in order
        //let Some(pic_hdr) = pic_hdr.map(|pos| Some(packets[pos])) else {
        let picture_header = match PictureHeader::from_bytes(packets[pic_hdr_idx].data(&map)) {
            Ok(picture_header) => picture_header,
            Err(err) => {
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Format,
                    ["Failed to parse MPEG-2 video frame picture header: {err:?}"]
                );
                return Err(gst::FlowError::Error);
            }
        };

        // Store sequence header

        let mut state = self.state.borrow_mut();

        // Todo: insert it in the unlikely case we find a GOP header w/o sequence header later
        if let Some(seq_hdr_idx) = seq_hdr {
            state.sequence_header = Some(packets[seq_hdr_idx].data(&map).to_vec());
        }

        let Some(sequence_header) = state.sequence_header.as_ref() else {
            gst::debug!(
                CAT,
                imp = self,
                "Picture, but no sequence header yet, dropping..."
            );
            self.obj().drop_buffers(id..=id);
            return Ok(gst::FlowSuccess::Ok);
        };

        // Sequence header followed by a sequence extension means MPEG-2. The packet parser will
        // merge the extension into the preceding sequence header, so that they get transmitted
        // as one unit in the same RTP packet.
        let mpeg_version = if sequence_header.windows(4).any(|w| w == [0u8, 0, 1, 0xb5]) {
            2
        } else {
            1
        };
        gst::trace!(CAT, imp = self, "MPEG version: {mpeg_version}");

        // Prepare for payloading

        let max_payload_size = self.obj().mtu() as usize
            - rtp_types::RtpPacket::MIN_RTP_PACKET_LEN
            - MPEG_VIDEO_SPECIFIC_HEADER_LEN;

        // Payload

        let mut to_payload = &packets[0..];

        while !to_payload.is_empty() {
            let from_idx = to_payload[0].index();
            let mut acc_bytes = 0;

            // We want to payload things in such a way that headers are fully contained in a
            // packet or are at the start of a packet. For Slices we want to make sure to start
            // a new RTP packet for every slice, unless the slices are tiny and multiple ones fit
            // into a single packet (as might be the case with something that compresses well,
            // like videotestsrc). The first slice doesn't need to start at the beginning of an RTP
            // packet, we can pack it right after the headers into the packet with the headers, but
            // there must be enough space after the headers to fit the whole slice header (8 bytes).
            //
            let to = to_payload.iter().take_while(|&p| {
                let take = acc_bytes + p.len() <= max_payload_size
                    || (p.first_slice() && acc_bytes > 0 && max_payload_size - acc_bytes >= 8);

                gst::trace!(CAT, imp = self,
                    "Checking packet {} {:?} with len {} and acc {acc_bytes} = {}, max {}, take: {take}",
                    p.index(),
                    p.ptype(),
                    p.len(),
                    acc_bytes + p.len(),
                    max_payload_size,
                );

                if take {
                    acc_bytes += p.len();
                }

                take
            }).last();

            // If the first packet doesn't fit, to will be None and we just payload that one packet
            let to_idx = to.map(|to_packet| to_packet.index()).unwrap_or(from_idx);

            self.payload_packets(
                &packets[from_idx..=to_idx],
                &map,
                id,
                max_payload_size,
                &picture_header,
            )?;

            to_payload = &packets[to_idx + 1..];
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();

        // Make sure configured MTU is large enough
        let mtu = self.obj().mtu() as usize;

        // RFC says min MTU size should be 261 to be able to accommodate the largest possible
        // ES header, but since we squash some headers we need to make it a bit bigger.
        if mtu < 278 {
            return Err(gst::error_msg!(
                gst::LibraryError::Settings,
                ("Configured MTU is too small")
            ));
        }

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();

        Ok(())
    }
}

impl RtpMpegVideoPay {
    // Here we do the actual payloading as instructed. Caller will have decided the packing,
    // and the included MPEG video packets will be payloaded as a single continuous chunk,
    // distributed across multiple RTP packets if needed.
    // If there are header packets, they will either all fit into a single RTP packet
    // (the first one), or it's a single header that needs to be split across multiple packets.
    //
    fn payload_packets(
        &self,
        packets: &[Packet],
        frame_data: &[u8],
        id: u64,
        max_payload_size: usize,
        picture_header: &PictureHeader,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        assert!(!packets.is_empty());

        gst::log!(
            CAT,
            imp = self,
            "Payloading packets {}-{}: {} ({} bytes)",
            packets[0].index(),
            packets.last().map(|p| p.index()).unwrap(),
            packets
                .iter()
                .fold(String::new(), |mut s, p| {
                    s.push_str(&format!("{:?} ", p.ptype()));
                    s
                })
                .trim_end(),
            packets.iter().map(|p| p.len()).sum::<usize>(),
        );

        let have_seq_hdr = packets.iter().any(|p| p.ptype() == PacketType::Sequence);
        let _have_gop_hdr = packets.iter().any(|p| p.ptype() == PacketType::Gop);
        let have_slice = packets.iter().any(|p| p.ptype() == PacketType::Slice);
        let ends_with_slice = packets.iter().last().unwrap().ptype() == PacketType::Slice;

        let start = packets.first().map(|p| p.offset()).unwrap();
        let end = packets.last().map(|p| p.offset() + p.len()).unwrap();

        let n_rtp_packets = (end - start).div_ceil(max_payload_size);

        assert!(n_rtp_packets >= 1);

        for (i, payload) in frame_data[start..end].chunks(max_payload_size).enumerate() {
            let is_last = i == (n_rtp_packets - 1);

            // MPEG video elementary stream specific header
            let mpv_header = if i == 0 {
                packet_header::PacketHeaderBuilder::new(picture_header)
                    .seq_header_present(have_seq_hdr)
                    .beginning_of_slice(have_slice)
                    .end_of_slice(have_slice && n_rtp_packets == 1)
                    .build()
            } else if is_last {
                packet_header::PacketHeaderBuilder::new(picture_header)
                    .end_of_slice(ends_with_slice)
                    .build()
            } else {
                packet_header::PacketHeaderBuilder::new(picture_header).build()
            };

            // M bit: For video, set to 1 on packet containing MPEG frame end code, 0 otherwise.
            let marker = is_last && end == frame_data.len();

            gst::trace!(
                CAT,
                imp = self,
                "RTP packet of {:4} bytes, frame data offset {start:6}-{end:6},\
                hdr {mpv_header:02x?}, marker: {}",
                payload.len(),
                marker as u8,
            );

            self.obj().queue_packet(
                id.into(),
                rtp_types::RtpPacketBuilder::new()
                    .payload(mpv_header.as_slice())
                    .payload(payload)
                    .marker_bit(marker),
            )?;
        }

        Ok(gst::FlowSuccess::Ok)
    }
}
