// GStreamer SMPTE ST-2038 ancillary metadata utils
//
// Copyright (C) 2024 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst_video::video_meta::AncillaryMeta;

#[derive(Clone, Copy, Debug)]
pub(crate) struct AncDataHeader {
    pub(crate) c_not_y_channel_flag: bool,
    pub(crate) did: u8,
    pub(crate) sdid: u8,
    pub(crate) line_number: u16,
    pub(crate) horizontal_offset: u16,
    pub(crate) data_count: u8,
    #[allow(unused)]
    pub(crate) checksum: u16,
    pub(crate) len: usize,
}

impl AncDataHeader {
    pub(crate) fn from_slice(slice: &[u8]) -> anyhow::Result<AncDataHeader> {
        use anyhow::Context;
        use bitstream_io::{BigEndian, BitRead, BitReader};
        use std::io::Cursor;

        let mut r = BitReader::endian(Cursor::new(slice), BigEndian);

        let zeroes = r.read::<6, u8>().context("zero bits")?;
        if zeroes != 0 {
            anyhow::bail!("Zero bits not zero!");
        }
        let c_not_y_channel_flag = r.read_bit().context("c_not_y_channel_flag")?;
        let line_number = r.read::<11, u16>().context("line number")?;
        let horizontal_offset = r.read::<12, u16>().context("horizontal offset")?;
        // Top two bits are parity bits and can be stripped off
        let did = (r.read::<10, u16>().context("DID")? & 0xff) as u8;
        let sdid = (r.read::<10, u16>().context("SDID")? & 0xff) as u8;
        let data_count = (r.read::<10, u16>().context("data count")? & 0xff) as u8;

        r.skip(data_count as u32 * 10).context("data")?;

        let checksum = r.read::<10, u16>().context("checksum")?;

        while !r.byte_aligned() {
            let one = r.read::<1, u8>().context("alignment")?;
            if one != 1 {
                anyhow::bail!("Alignment bits are not ones!");
            }
        }

        let len = r.position_in_bits().unwrap();
        assert!(len % 8 == 0);
        let len = len as usize / 8;

        Ok(AncDataHeader {
            c_not_y_channel_flag,
            line_number,
            horizontal_offset,
            did,
            sdid,
            data_count,
            checksum,
            len,
        })
    }
}

fn extend_with_even_odd_parity(v: u8, checksum: &mut u16) -> u16 {
    let parity = v.count_ones() & 1;
    let res = if parity == 0 {
        0x1_00 | (v as u16)
    } else {
        0x2_00 | (v as u16)
    };

    *checksum = checksum.wrapping_add(res);

    res
}

pub(crate) fn convert_to_st2038_buffer(
    c_not_y_channel: bool,
    line_number: u16,
    horizontal_offset: u16,
    did: u8,
    sdid: u8,
    payload: &[u8],
) -> Result<gst::Buffer, anyhow::Error> {
    if payload.len() > 255 {
        anyhow::bail!(
            "Payload needs to be less than 256 bytes, got {}",
            payload.len()
        );
    }

    use anyhow::Context;
    use bitstream_io::{BigEndian, BitWrite, BitWriter};

    let mut output = Vec::with_capacity((70 + payload.len() * 10) / 8 + 1);

    let mut w = BitWriter::endian(&mut output, BigEndian);

    w.write::<6, u8>(0b00_0000).context("zero bits")?;
    w.write_bit(c_not_y_channel).context("c_not_y_channel")?;
    w.write::<11, u16>(line_number).context("line number")?;
    w.write::<12, u16>(horizontal_offset)
        .context("horizontal offset")?;

    let mut checksum = 0u16;

    w.write::<10, u16>(extend_with_even_odd_parity(did, &mut checksum))
        .context("DID")?;
    w.write::<10, u16>(extend_with_even_odd_parity(sdid, &mut checksum))
        .context("SDID")?;
    w.write::<10, u16>(extend_with_even_odd_parity(
        payload.len() as u8,
        &mut checksum,
    ))
    .context("data count")?;

    for &b in payload {
        w.write::<10, u16>(extend_with_even_odd_parity(b, &mut checksum))
            .context("payload")?;
    }

    checksum &= 0x1_ff;
    checksum |= ((!(checksum >> 8)) & 0x0_01) << 9;

    w.write::<10, u16>(checksum).context("checksum")?;

    while !w.byte_aligned() {
        w.write_bit(true).context("padding")?;
    }

    w.flush().context("flushing")?;

    Ok(gst::Buffer::from_mut_slice(output))
}

pub(crate) fn to_st2038_with_10bit(
    st2038_buffer: &mut Vec<u8>,
    meta: &AncillaryMeta,
) -> Result<(), anyhow::Error> {
    if meta.data().len() > 255 {
        anyhow::bail!(
            "Payload needs to be less than 256 bytes, got {}",
            meta.data().len()
        );
    }

    use anyhow::Context;
    use bitstream_io::{BigEndian, BitWrite, BitWriter};
    use std::io::Cursor;

    let st2038_buffer_len = st2038_buffer.len() as u64;
    let mut cursor = Cursor::new(st2038_buffer);
    cursor.set_position(st2038_buffer_len);

    let mut w = BitWriter::endian(cursor, BigEndian);

    w.write::<6, u8>(0b00_0000).context("zero bits")?;
    w.write_bit(meta.c_not_y_channel())
        .context("c_not_y_channel")?;
    w.write::<11, u16>(meta.line()).context("line number")?;
    w.write::<12, u16>(meta.offset())
        .context("horizontal offset")?;

    w.write::<10, u16>(meta.did()).context("DID")?;
    w.write::<10, u16>(meta.sdid_block_number())
        .context("SDID")?;
    w.write::<10, u16>(meta.data_count())
        .context("data count")?;

    for &b in meta.data() {
        w.write::<10, u16>(b).context("payload")?;
    }

    w.write::<10, u16>(meta.checksum()).context("checksum")?;

    while !w.byte_aligned() {
        w.write_bit(true).context("padding")?;
    }

    w.flush().context("flushing")?;

    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct AncData {
    pub(crate) header: AncDataHeader,
    pub(crate) data: Vec<u16>,
}

impl AncData {
    pub(crate) fn from_slice(slice: &[u8]) -> anyhow::Result<AncData> {
        use anyhow::Context;
        use bitstream_io::{BigEndian, BitRead, BitReader};
        use std::io::Cursor;

        let mut r = BitReader::endian(Cursor::new(slice), BigEndian);

        let zeroes = r.read::<6, u8>().context("zero bits")?;
        if zeroes != 0 {
            anyhow::bail!("Zero bits not zero!");
        }
        let c_not_y_channel_flag = r.read_bit().context("c_not_y_channel_flag")?;
        let line_number = r.read::<11, u16>().context("line number")?;
        let horizontal_offset = r.read::<12, u16>().context("horizontal offset")?;
        // Top two bits are parity bits and stripped off here, will be
        // restored when adding the AncillaryMeta back on the buffer.
        let did = (r.read::<10, u16>().context("DID")? & 0xff) as u8;
        let sdid = (r.read::<10, u16>().context("SDID")? & 0xff) as u8;
        let data_count = (r.read::<10, u16>().context("data count")? & 0xff) as u8;

        let mut data = Vec::<u16>::with_capacity(255);
        for _ in 0..data_count {
            let udw = r.read::<10, u16>().context("checksum")?;
            data.push(udw);
        }

        let checksum = r.read::<10, u16>().context("checksum")?;

        while !r.byte_aligned() {
            let one = r.read::<1, u8>().context("alignment")?;
            if one != 1 {
                anyhow::bail!("Alignment bits are not ones!");
            }
        }

        let len = r.position_in_bits().unwrap();
        assert!(r.byte_aligned());
        let len = len as usize / 8;

        Ok(AncData {
            header: AncDataHeader {
                c_not_y_channel_flag,
                did,
                sdid,
                line_number,
                horizontal_offset,
                data_count,
                checksum,
                len,
            },
            data,
        })
    }
}

pub(crate) fn add_ancillary_meta_to_buffer(buffer: &mut gst::BufferRef, anc_data: &Vec<AncData>) {
    for anc in anc_data {
        let mut checksum = 0u16;
        let mut meta = AncillaryMeta::add(buffer);

        meta.set_c_not_y_channel(anc.header.c_not_y_channel_flag);
        meta.set_did(extend_with_even_odd_parity(anc.header.did, &mut checksum));
        meta.set_sdid_block_number(extend_with_even_odd_parity(anc.header.sdid, &mut checksum));
        meta.set_line(anc.header.line_number);
        meta.set_offset(anc.header.horizontal_offset);
        meta.set_data(anc.data.clone().into());
        meta.set_checksum(anc.header.checksum);
        meta.set_data_count_upper_two_bits(
            (extend_with_even_odd_parity(anc.data.len() as u8) >> 8) as u8,
        );
    }
}
