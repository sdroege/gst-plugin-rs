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
        assert!(len.is_multiple_of(8));
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

/// Extend an 8-bit value to a 10-bit ST 291 word with even/odd parity.
///
/// Per SMPTE ST 291: b8 is even parity over b7 to b0 (so b0 to b8 have an even
/// number of 1s), and b9 = NOT b8. Matches `SET_WITH_PARITY` in
/// gst-plugins-base `video-anc.c`: odd popcount gives `0x100 | v`, even gives
/// `0x200 | v`.
fn extend_with_even_odd_parity(v: u8) -> u16 {
    if v.count_ones() & 1 != 0 {
        // Odd number of ones in v: b8 = 1, b9 = 0
        0x1_00 | (v as u16)
    } else {
        // Even number of ones in v: b8 = 0, b9 = 1
        0x2_00 | (v as u16)
    }
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

    let did_10bit = extend_with_even_odd_parity(did);
    let sdid_10bit = extend_with_even_odd_parity(sdid);
    let dc_10bit = extend_with_even_odd_parity(payload.len() as u8);

    w.write::<10, u16>(did_10bit).context("DID")?;
    w.write::<10, u16>(sdid_10bit).context("SDID")?;
    w.write::<10, u16>(dc_10bit).context("data count")?;

    /*
     * See Section 6.7 of ST-291 on Checksum Word.
     * Write data words and checksum.
     *
     * In 10-bit applications, the checksum value shall be equal to
     * the nine least significant bits of the sum of the nine least
     * significant bits of the DID, SDID, DC and UDW.
     */
    let mut checksum = 0u16;
    checksum = checksum.wrapping_add(did_10bit & 0x1FF);
    checksum = checksum.wrapping_add(sdid_10bit & 0x1FF);
    checksum = checksum.wrapping_add(dc_10bit & 0x1FF);

    for &b in payload {
        let val = extend_with_even_odd_parity(b);
        w.write::<10, u16>(val).context("payload")?;
        checksum = checksum.wrapping_add(val & 0x1FF);
    }

    checksum &= 0x1_ff;
    // b9 = NOT b8 (ST 291 checksum word)
    if checksum & 0x1_00 == 0 {
        checksum |= 0x2_00;
    }

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

pub(crate) fn add_ancillary_meta_to_buffer(
    buffer: &mut gst::BufferRef,
    anc_data: impl IntoIterator<Item = AncData>,
) {
    for anc in anc_data {
        let data_len = anc.data.len();
        let mut meta = AncillaryMeta::add(buffer);
        meta.set_c_not_y_channel(anc.header.c_not_y_channel_flag);
        meta.set_did(extend_with_even_odd_parity(anc.header.did));
        meta.set_sdid_block_number(extend_with_even_odd_parity(anc.header.sdid));
        meta.set_line(anc.header.line_number);
        meta.set_offset(anc.header.horizontal_offset);
        meta.set_data(anc.data.into());
        meta.set_checksum(anc.header.checksum);
        meta.set_data_count_upper_two_bits(
            (extend_with_even_odd_parity(data_len as u8) >> 8) as u8,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_odd_parity_matches_st291_and_gst_vbi_encoder() {
        // Odd popcount gives b8=1, b9=0 (0x100 | v), as SET_WITH_PARITY in video-anc.c
        assert_eq!(extend_with_even_odd_parity(0x00), 0x200); // 0 ones, even
        assert_eq!(extend_with_even_odd_parity(0x01), 0x101); // 1 one, odd
        assert_eq!(extend_with_even_odd_parity(0x03), 0x203); // 2 ones, even
        assert_eq!(extend_with_even_odd_parity(0x61), 0x161); // 3 ones, odd (CEA-708 DID)

        for v in 0u8..=255 {
            let w = extend_with_even_odd_parity(v);
            assert_eq!(w & 0xff, u16::from(v));
            // Even parity over b0..b8: popcount of those 9 bits must be even
            let popcount_0_to_8 = (w & 0x1ff).count_ones();
            assert!(
                popcount_0_to_8.is_multiple_of(2),
                "b0..b8 must have even popcount for {v:#04x}, got {w:#05x}"
            );
            let b8 = (w >> 8) & 1;
            let b9 = (w >> 9) & 1;
            assert_eq!(b9, 1 - b8, "b9 must be NOT b8 for {v:#04x}");
        }
    }

    #[test]
    fn convert_to_st2038_uses_parity_extended_adf_words() {
        gst::init().unwrap();

        let buf = convert_to_st2038_buffer(false, 9, 0, 0x61, 0x01, &[0xaa, 0xbb]).expect("encode");
        let map = buf.map_readable().unwrap();
        let hdr = AncDataHeader::from_slice(map.as_slice()).expect("parse header");
        assert_eq!(hdr.did, 0x61);
        assert_eq!(hdr.sdid, 0x01);
        assert_eq!(hdr.data_count, 2);

        // Re-parse the 10-bit ADF words and check parity bits were written correctly.
        use bitstream_io::{BigEndian, BitRead, BitReader};
        use std::io::Cursor;
        let mut r = BitReader::endian(Cursor::new(map.as_slice()), BigEndian);
        let _ = r.read::<6, u8>().unwrap();
        let _ = r.read_bit().unwrap();
        let _ = r.read::<11, u16>().unwrap();
        let _ = r.read::<12, u16>().unwrap();
        let did10 = r.read::<10, u16>().unwrap();
        let sdid10 = r.read::<10, u16>().unwrap();
        let dc10 = r.read::<10, u16>().unwrap();
        assert_eq!(did10, 0x161);
        assert_eq!(sdid10, 0x101);
        assert_eq!(dc10, extend_with_even_odd_parity(2));

        let udw0 = r.read::<10, u16>().unwrap();
        let udw1 = r.read::<10, u16>().unwrap();
        assert_eq!(udw0, extend_with_even_odd_parity(0xaa));
        assert_eq!(udw1, extend_with_even_odd_parity(0xbb));

        let checksum = r.read::<10, u16>().unwrap();
        let mut expected = 0u16;
        expected = expected.wrapping_add(did10 & 0x1ff);
        expected = expected.wrapping_add(sdid10 & 0x1ff);
        expected = expected.wrapping_add(dc10 & 0x1ff);
        expected = expected.wrapping_add(udw0 & 0x1ff);
        expected = expected.wrapping_add(udw1 & 0x1ff);
        expected &= 0x1ff;
        if expected & 0x100 == 0 {
            expected |= 0x200;
        }
        assert_eq!(checksum, expected);
        assert_eq!((checksum >> 9) & 1, 1 - ((checksum >> 8) & 1));
    }
}
