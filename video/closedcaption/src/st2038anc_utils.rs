// GStreamer SMPTE ST-2038 ancillary metadata utils
//
// Copyright (C) 2024 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AncDataHeader {
    pub(crate) c_not_y_channel_flag: bool,
    pub(crate) did: u8,
    pub(crate) sdid: u8,
    pub(crate) line_number: u16,
    pub(crate) horizontal_offset: u16,
}

impl AncDataHeader {
    pub(crate) fn from_buffer(buffer: &gst::Buffer) -> anyhow::Result<AncDataHeader> {
        use anyhow::Context;
        use bitstream_io::{BigEndian, BitRead, BitReader};

        let mut r = BitReader::endian(buffer.as_cursor_readable(), BigEndian);

        let zeroes = r.read::<u8>(6).context("zero bits")?;
        if zeroes != 0 {
            anyhow::bail!("Zero bits not zero!");
        }
        let c_not_y_channel_flag = r.read_bit().context("c_not_y_channel_flag")?;
        let line_number = r.read::<u16>(11).context("line number")?;
        let horizontal_offset = r.read::<u16>(12).context("horizontal offset")?;
        // Top two bits are parity bits and can be stripped off
        let did = (r.read::<u16>(10).context("DID")? & 0xff) as u8;
        let sdid = (r.read::<u16>(10).context("SDID")? & 0xff) as u8;
        let _data_count = (r.read::<u16>(10).context("data count")? & 0xff) as u8;

        Ok(AncDataHeader {
            c_not_y_channel_flag,
            line_number,
            horizontal_offset,
            did,
            sdid,
        })
    }
}
