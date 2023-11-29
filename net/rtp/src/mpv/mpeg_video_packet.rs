// GStreamer RTP MPEG-1/MPEG-2 Video Elementary Stream Payloading - Packet Parser
//
// Copyright (C) 2023-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use bitstream_io::{BigEndian, BitRead, BitReader};

use smallvec::SmallVec;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PacketType {
    Sequence,
    SequenceExtension,
    SequenceDisplayExtension,
    Gop,
    Picture,
    PictureCodingExtension,
    UnknownExtension,
    Slice,
    UserData,
    SequenceEnd,
    Unknown,
}

#[derive(Debug)]
pub(crate) struct Packet {
    ptype: PacketType,
    offset: u32,
    len: u32,

    // Not strictly needed, but makes things easier and it
    // fits into what would otherwise be struct padding anyway
    idx: u16,

    // Whether this is the first slice
    first_slice: bool,
}

impl Packet {
    pub(crate) fn new(offset: usize, len: usize, idx: u16, data: &[u8]) -> Self {
        assert!(data.starts_with(&[0u8, 0, 1]) && data.len() >= 4);
        assert!(offset <= u32::MAX as usize);
        assert!(len <= u32::MAX as usize);

        let ptype = match data[3] {
            0x00 => PacketType::Picture,
            0x01..=0xaf => PacketType::Slice,
            0xb2 => PacketType::UserData,
            0xb3 => PacketType::Sequence,
            0xb5 if data.len() > 4 => match (data[4] & 0xf0) >> 4 {
                1 => PacketType::SequenceExtension,
                2 => PacketType::SequenceDisplayExtension,
                8 => PacketType::PictureCodingExtension,
                _ => PacketType::UnknownExtension,
            },
            0xb7 => PacketType::SequenceEnd,
            0xb8 => PacketType::Gop,
            _ => PacketType::Unknown,
        };

        Packet {
            ptype,
            offset: offset as u32,
            len: len as u32,
            idx,
            first_slice: false, // set this later
        }
    }

    pub(crate) fn ptype(&self) -> PacketType {
        self.ptype
    }

    pub(crate) fn offset(&self) -> usize {
        self.offset as usize
    }

    pub(crate) fn len(&self) -> usize {
        self.len as usize
    }

    pub(crate) fn index(&self) -> usize {
        self.idx as usize
    }

    pub(crate) fn data<'a>(&'a self, frame_data: &'a [u8]) -> &'a [u8] {
        &frame_data[self.offset()..][..self.len()]
    }

    pub(crate) fn first_slice(&self) -> bool {
        self.first_slice
    }
}
// Magic number: 76 = 1088 lines / 16 macro block height = 68 slices + a few headers
pub(crate) type PacketVec = SmallVec<[Packet; 76]>;

pub(crate) fn parse_packets_from_slice(frame_data: &[u8]) -> Result<PacketVec, ()> {
    // Skip be any number of leading zeros
    let Some(first_nonzero) = frame_data.iter().position(|&b| b != 0x00) else {
        return Err(()); // all zeros
    };

    // Make sure we have at least two zeroes in front, i.e. 00 00 01
    if first_nonzero < 2 || frame_data[first_nonzero] != 0x01 {
        return Err(());
    }

    let initial_offset = first_nonzero - 2;

    // There are cleverer ways to scan for sync markers, but for now KISS.
    fn scan_for_sync_marker(bytes: &[u8]) -> Option<usize> {
        bytes.windows(3).position(|window| window == [0, 0, 1])
    }

    let mut packets: PacketVec = smallvec::smallvec![];

    let mut frame_data = &frame_data[initial_offset..];
    let mut offset = initial_offset;

    while frame_data.len() > 3 {
        let idx = packets.len() as u16;

        // Look for the start of the next packet to figure out where this packet ends
        let packet = if let Some(next_offset) = scan_for_sync_marker(&frame_data[2..]) {
            let len = next_offset + 2;
            let packet = Packet::new(offset, len, idx, frame_data);
            frame_data = &frame_data[next_offset + 2..];
            packet
        } else {
            // Packet is all the remaining data (we assume parsed input)
            let len = frame_data.len();
            let packet = Packet::new(offset, len, idx, frame_data);
            frame_data = &[];
            packet
        };

        offset += packet.len();

        // Keep some extensions together with their kin for payloading purposes
        if let Some(prev_packet) = packets.last_mut() {
            use PacketType::*;

            #[allow(clippy::match_like_matches_macro)]
            let merge_into_prev = match (&prev_packet.ptype, &packet.ptype) {
                (Sequence, SequenceExtension) => true,
                (SequenceExtension, SequenceDisplayExtension) => true,
                (Sequence, SequenceDisplayExtension) => true,
                (Picture, PictureCodingExtension) => true,
                _ => false,
            };

            if merge_into_prev {
                prev_packet.len += packet.len;
            } else {
                packets.push(packet);
            }
        } else {
            packets.push(packet);
        }

        // Sanity check. Should be enough in practice, but is
        // much lower than what's allowed theoretically.
        if packets.len() > 256 {
            return Err(());
        }
    }

    // Mark first slice for convenience
    if let Some(p) = packets.iter_mut().find(|p| p.ptype() == PacketType::Slice) {
        p.first_slice = true;
    };

    Ok(packets)
}

#[derive(Copy, Clone, Debug, PartialEq)]
#[allow(dead_code)]
pub(crate) enum PictureType {
    I = 1,
    P = 2,
    B = 3,
    D = 4,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PictureHeader {
    pub(crate) tsn: u16, // Temporal sequence number
    pub(crate) pic_type: PictureType,

    _vbv_delay: u16,

    pub(crate) full_pel_forward_vector: Option<bool>, // only for MPEG-1, 0 for MPEG-2
    pub(crate) full_pel_backward_vector: Option<bool>, // only for MPEG-1, 0 for MPEG-2

    pub(crate) forward_f_code: Option<u8>, // only for MPEG-1, 0b111 for MPEG-2
    pub(crate) backward_f_code: Option<u8>, // only for MPEG-1, 0b111 for MPEG-2
}

// Errors produced when writing a packet
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PictureHeaderError {
    // No start code at beginning of byte slice
    #[error("No start code")]
    NoStartCode,

    // Not enough bytes
    #[error("Too short")]
    TooShort,

    // Invalid picture type
    #[error("Invalid Picture Type")]
    InvalidPictureType,
}

impl PictureHeader {
    pub(crate) fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        use anyhow::Context;

        if bytes.len() < 4 + 4 {
            Err(PictureHeaderError::TooShort)?;
        }

        if !bytes.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
            Err(PictureHeaderError::NoStartCode)?;
        }

        let mut bits = BitReader::endian(&bytes[4..], BigEndian);

        let tsn = bits.read::<10, u16>().context("tsn")?;

        let pic_type = match bits.read::<3, u8>().context("pic_type")? {
            1 => PictureType::I,
            2 => PictureType::P,
            3 => PictureType::B,
            4 => PictureType::D,
            _ => Err(PictureHeaderError::InvalidPictureType)?,
        };

        let vbv_delay = bits.read::<16, u16>().context("vbv_delay")?;

        let mut full_pel_forward_vector = None;
        let mut forward_f_code = None;

        if pic_type == PictureType::P || pic_type == PictureType::B {
            full_pel_forward_vector = Some(bits.read_bit().context("ffv")?);
            forward_f_code = Some(bits.read::<3, u8>().context("ffc")?);
        }

        let mut full_pel_backward_vector = None;
        let mut backward_f_code = None;

        if pic_type == PictureType::B {
            full_pel_backward_vector = Some(bits.read_bit().context("fbv")?);
            backward_f_code = Some(bits.read::<3, u8>().context("bfc")?);
        }

        Ok(PictureHeader {
            tsn,
            pic_type,

            _vbv_delay: vbv_delay,

            full_pel_forward_vector,
            full_pel_backward_vector,

            forward_f_code,
            backward_f_code,
        })
    }
}
