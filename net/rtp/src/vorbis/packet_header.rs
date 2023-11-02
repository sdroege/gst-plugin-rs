// GStreamer RTP Vorbis Depayloader - Vorbis RTP Packet Header Handling
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// https://www.rfc-editor.org/rfc/rfc5215.html#section-2.2

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FragType {
    NotFragmented = 0,
    Start = 1,
    Continuation = 2,
    End = 3,
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)]
pub(crate) enum VorbisDataType {
    RawPacket = 0,
    PackedConfig = 1,
    LegacyComment = 2,
    Reserved = 3,
}

// Errors produced when writing a packet
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PacketHeaderError {
    // Too many raw packets inside this non-fragmented payload, max allowed is 15.
    #[error("Too many packets: {}, max is 15", .0)]
    TooManyPackets(usize),

    // For non-fragmented payloads we should have 0 packets
    #[error("{} packets specified, but expected 0 for fragmented payload", .0)]
    UnexpectedPacketsForFragmentedPayload(usize),

    // Non-fragmented payload, but no packets, need at least 1 packet.
    #[error("Non-fragmented payload, but zero packets")]
    NoPackets,
}

// Struct for building a new Vorbis RTP packet header
#[derive(Clone, Debug, Default)]
pub(crate) struct PacketHeaderBuilder {
    ident: u32,
    frag_type: Option<FragType>,
    vdt: Option<VorbisDataType>,
    n_packets: Option<usize>,
}

impl PacketHeaderBuilder {
    pub(crate) fn new(ident: u32) -> PacketHeaderBuilder {
        Self {
            ident,
            ..Default::default()
        }
    }

    pub(crate) fn frag_type(mut self, frag_type: FragType) -> Self {
        self.frag_type = Some(frag_type);
        self
    }

    pub(crate) fn data_type(mut self, vdt: VorbisDataType) -> Self {
        self.vdt = Some(vdt);
        self
    }

    pub(crate) fn n_packets(mut self, n_packets: usize) -> Self {
        self.n_packets = Some(n_packets);
        self
    }

    pub(crate) fn build(self) -> Result<[u8; 4], PacketHeaderError> {
        let n_packets = self.n_packets.unwrap_or(0);

        let frag_type = self.frag_type.unwrap_or(FragType::NotFragmented);

        if frag_type != FragType::NotFragmented && n_packets != 0 {
            Err(PacketHeaderError::UnexpectedPacketsForFragmentedPayload(
                n_packets,
            ))?
        }

        if frag_type == FragType::NotFragmented && n_packets == 0 {
            Err(PacketHeaderError::NoPackets)?
        }

        if n_packets > 15 {
            Err(PacketHeaderError::TooManyPackets(n_packets))?
        }

        let vdt = self.vdt.unwrap_or(VorbisDataType::RawPacket);

        let ident_bytes = self.ident.to_be_bytes();

        assert_eq!(ident_bytes[0], 0); // ident is 24 bits in u32, big-endian

        // https://www.rfc-editor.org/rfc/rfc5215.html#section-2.2
        //
        //  0                   1                   2                   3
        //  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
        // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        // |                     Ident                     | F |VDT|# pkts.|
        // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        //
        // F: Fragment type (0=none, 1=start, 2=cont, 3=end)
        // VDT: Vorbis data type (0=vorbis, 1=config, 2=comment, 3=reserved)
        // pkts: number of packets.

        let mut buf = [0u8; 4];

        buf[0] = ident_bytes[1];
        buf[1] = ident_bytes[2];
        buf[2] = ident_bytes[3];

        buf[3] = ((frag_type as u8) << 6) | ((vdt as u8) << 4) | n_packets as u8;

        Ok(buf)
    }
}
