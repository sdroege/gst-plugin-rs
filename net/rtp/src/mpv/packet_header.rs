// GStreamer RTP MPEG-2 Video Elementary Stream Payloader - MPEG Video-Specific Header Handling
//
// Copyright (C) 2023-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use super::mpeg_video_packet::{PictureHeader, PictureType};

// https://datatracker.ietf.org/doc/html/rfc2250#section-3.4

// Struct for building a new MPEG Video-specific packet header
#[derive(Clone, Debug)]
pub(crate) struct PacketHeaderBuilder {
    // From picture header
    temporal_ref: u16,
    picture_type: PictureType,
    full_pel_forward_vector: bool,
    full_pel_backward_vector: bool,

    forward_f_code: u8,
    backward_f_code: u8,

    // User provided
    seq_header_present: Option<bool>,
    beginning_of_slice: Option<bool>,
    end_of_slice: Option<bool>,
}

impl PacketHeaderBuilder {
    pub(crate) fn new(picture_header: &PictureHeader) -> PacketHeaderBuilder {
        assert!(picture_header.tsn < 1024);

        Self {
            temporal_ref: picture_header.tsn,
            picture_type: picture_header.pic_type,
            full_pel_forward_vector: picture_header.full_pel_forward_vector.unwrap_or(false),
            full_pel_backward_vector: picture_header.full_pel_backward_vector.unwrap_or(false),
            forward_f_code: picture_header.forward_f_code.unwrap_or(0b111),
            backward_f_code: picture_header.backward_f_code.unwrap_or(0b111),
            seq_header_present: None,
            beginning_of_slice: None,
            end_of_slice: None,
        }
    }

    pub(crate) fn seq_header_present(mut self, seq_header_present: bool) -> Self {
        self.seq_header_present = Some(seq_header_present);
        self
    }

    // Set when the start of the packet payload is a slice start code, or when a slice start
    // code is preceded only by one or more of a Video_Sequence_Header, GOP_header and/or
    // Picture_Header.
    //
    pub(crate) fn beginning_of_slice(mut self, beginning_of_slice: bool) -> Self {
        self.beginning_of_slice = Some(beginning_of_slice);
        self
    }

    // Set when the last byte of the payload is the end of an MPEG slice.
    //
    pub(crate) fn end_of_slice(mut self, end_of_slice: bool) -> Self {
        self.end_of_slice = Some(end_of_slice);
        self
    }

    pub(crate) fn build(self) -> [u8; 4] {
        let seq_header_flag: u8 = self
            .seq_header_present
            .filter(|&b| b)
            .map(|_| 0x20)
            .unwrap_or(0);

        let bs_flag: u8 = self
            .beginning_of_slice
            .filter(|&b| b)
            .map(|_| 0x10)
            .unwrap_or(0);

        let es_flag: u8 = self.end_of_slice.filter(|&b| b).map(|_| 0x08).unwrap_or(0);

        let pic_type = self.picture_type as u8;

        // https://datatracker.ietf.org/doc/html/rfc2250#section-3.4
        //
        // 0                   1                   2                   3
        // 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
        // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        // |    MBZ  |T|         TR        | |N|S|B|E|  P  | | BFC | | FFC |
        // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        //                                 AN              FBV     FFV

        let mut buf = [0u8; 4];

        buf[0..2].copy_from_slice(&self.temporal_ref.to_be_bytes());

        buf[2] = pic_type | es_flag | bs_flag | seq_header_flag;

        buf[3] = 0x00;

        if matches!(self.picture_type, PictureType::P | PictureType::B) {
            buf[3] = ((self.full_pel_backward_vector as u8) << 7)
                | (self.backward_f_code << 4)
                | ((self.full_pel_forward_vector as u8) << 3)
                | self.forward_f_code;
        }

        buf
    }
}
