// GStreamer RTP Raw Video Payloader - Frame Packing Template
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// This creates a template how to divvy up the pixels in a video frame and pack them into
// RTP packets. RTP packets can contain one or multiple line fragments.
use smallvec::{SmallVec, smallvec};

pub(crate) const VRAW_EXT_SEQNUM_LEN: usize = 2;
pub(crate) const VRAW_CHUNK_HDR_LEN: usize = 6;

#[derive(Debug)]
pub(crate) struct LineChunk {
    pub length: u16, // Length in bytes, multiple of pgroup size
    pub y_off: u16,  // Line number
    pub x_off: u16,  // Pixel offset into the line
}

#[derive(Debug)]
pub(crate) struct Packet {
    pub chunks: SmallVec<[LineChunk; 4]>,
}

impl Packet {
    fn add_line_chunk(&mut self, x_off: usize, y_off: usize, length: usize) {
        assert!(length > 0);

        let line_chunk = LineChunk {
            length: length as u16,
            x_off: x_off as u16,
            y_off: y_off as u16,
        };

        self.chunks.push(line_chunk);
    }

    pub fn make_headers(
        &self,
        field: u8,
        extended_seqnum: u32,
    ) -> SmallVec<[u8; VRAW_EXT_SEQNUM_LEN + 4 * VRAW_CHUNK_HDR_LEN]> {
        let field_flag = if field == 1 { 0x8000 } else { 0x0000 };

        let n_chunks = self.chunks.len();

        let hdr_len = VRAW_EXT_SEQNUM_LEN + n_chunks * VRAW_CHUNK_HDR_LEN;

        let mut hdr_buf = smallvec::SmallVec::with_capacity(hdr_len);

        hdr_buf.extend_from_slice(&extended_seqnum.to_be_bytes()[..2]);

        for (i, chunk) in self.chunks.iter().enumerate() {
            let is_last = i == (n_chunks - 1);
            let continuation_flag = if !is_last { 0x8000 } else { 0x0000 };

            hdr_buf.extend_from_slice(&chunk.length.to_be_bytes());
            hdr_buf.extend_from_slice(&(chunk.y_off | field_flag).to_be_bytes());
            hdr_buf.extend_from_slice(&(chunk.x_off | continuation_flag).to_be_bytes());
        }

        hdr_buf
    }
}

pub(crate) struct FramePackingTemplate {
    pub packets: Vec<Packet>,
    pub mtu: usize,
}

impl FramePackingTemplate {
    fn new_for_size(frame_size: usize, max_payload_size: usize) -> Self {
        let n_prealloc = {
            let hdr_size = VRAW_EXT_SEQNUM_LEN + 2 * VRAW_CHUNK_HDR_LEN;

            // We checked the configured mtu size already on start-up
            assert!(hdr_size < max_payload_size);

            let payload_size = max_payload_size - hdr_size;
            (frame_size + payload_size) / payload_size
        };

        FramePackingTemplate {
            packets: Vec::with_capacity(n_prealloc),
            mtu: max_payload_size,
        }
    }

    fn add(&mut self, packet: Packet) {
        self.packets.push(packet);
    }

    pub fn new(
        max_payload_size: usize,
        vinfo: &gst_video::VideoInfo,
        field: u8,
        pgroup_size: usize,
        x_inc: usize,
        y_inc: usize,
    ) -> Result<FramePackingTemplate, ()> {
        assert!(field == 0 || field == 1);
        assert!(pgroup_size > 0);

        let max_payload_size = max_payload_size - VRAW_EXT_SEQNUM_LEN;

        let mut packing_template =
            FramePackingTemplate::new_for_size(vinfo.size(), max_payload_size);

        let mut pbuilder = PacketBuilder::new(max_payload_size, pgroup_size);

        let height = vinfo.height() as usize;
        let width = vinfo.width() as usize;

        // Template caps ensure that already, just letting the compiler know
        assert!(width <= u16::MAX as usize && height <= u16::MAX as usize);

        for line in (0..height).step_by(y_inc) {
            let mut x = 0;

            while x < width {
                if !pbuilder.has_space_left() {
                    let packet = pbuilder.take_packet();
                    packing_template.add(packet);
                }

                let pgroups_left_in_line = (width - x).div_ceil(x_inc);

                let space_left_in_pgroups = pbuilder.space_left_in_pgroups();

                let pgroups_to_payload = std::cmp::min(space_left_in_pgroups, pgroups_left_in_line);

                pbuilder.add_line_chunk(x, line, pgroups_to_payload);

                x += pgroups_to_payload * x_inc;
            }
        }

        if pbuilder.has_data() {
            let packet = pbuilder.take_packet();
            packing_template.add(packet);
        }

        Ok(packing_template)
    }
}

impl Packet {
    fn new() -> Self {
        Packet {
            chunks: smallvec![],
        }
    }
}

struct PacketBuilder {
    pgroup_size: usize,
    max_payload_size: usize,
    bytes_left_in_packet: usize,
    packet: Option<Packet>,
}

impl PacketBuilder {
    fn new(max_payload_size: usize, pgroup_size: usize) -> Self {
        PacketBuilder {
            pgroup_size,
            max_payload_size,
            bytes_left_in_packet: max_payload_size,
            packet: Some(Packet::new()),
        }
    }

    fn has_space_left(&self) -> bool {
        self.bytes_left_in_packet >= (VRAW_CHUNK_HDR_LEN + self.pgroup_size)
    }

    fn space_left_in_pgroups(&self) -> usize {
        if self.has_space_left() {
            (self.bytes_left_in_packet - VRAW_CHUNK_HDR_LEN) / self.pgroup_size
        } else {
            0
        }
    }

    fn has_data(&self) -> bool {
        self.bytes_left_in_packet < self.max_payload_size
    }

    fn take_packet(&mut self) -> Packet {
        // Reset space left, and start a new packet, returning the old one
        self.bytes_left_in_packet = self.max_payload_size;
        self.packet.replace(Packet::new()).unwrap()
    }

    fn add_line_chunk(&mut self, x_off: usize, y_off: usize, n_pgroups: usize) {
        debug_assert!(self.space_left_in_pgroups() >= n_pgroups);

        let packet = self.packet.as_mut().unwrap();
        let length_in_bytes = n_pgroups * self.pgroup_size;
        packet.add_line_chunk(x_off, y_off, length_in_bytes);
        self.bytes_left_in_packet -= length_in_bytes;
    }
}
