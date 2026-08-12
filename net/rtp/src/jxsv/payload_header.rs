// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use std::io;

use anyhow::{Context as _, bail};
use bitstream_io::{BigEndian, BitRead as _, BitReader, BitWrite as _, BitWriter};

/// RFC 9134 JPEG XS RTP payload header (4 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadHeader {
    pub transmission_mode: bool,
    pub packetization_mode: bool,
    pub last: bool,
    pub interlaced: u8,
    pub frame_counter: u8,
    pub sep_counter: u16,
    pub p_counter: u16,
}

impl PayloadHeader {
    pub const SIZE: usize = 4;

    pub fn pack(&self) -> Result<[u8; Self::SIZE], anyhow::Error> {
        let mut cursor = io::Cursor::new([0u8; Self::SIZE]);
        {
            let mut w = BitWriter::endian(&mut cursor, BigEndian);
            w.write_bit(self.transmission_mode)
                .context("transmission_mode")?;
            w.write_bit(self.packetization_mode)
                .context("packetization_mode")?;
            w.write_bit(self.last).context("last")?;
            w.write::<2, u8>(self.interlaced).context("interlaced")?;
            w.write::<5, u8>(self.frame_counter)
                .context("frame_counter")?;
            w.write::<11, u16>(self.sep_counter)
                .context("sep_counter")?;
            w.write::<11, u16>(self.p_counter).context("p_counter")?;
            w.byte_align().context("byte_align")?;
        }
        Ok(cursor.into_inner())
    }

    pub fn parse(data: &[u8]) -> Result<Self, anyhow::Error> {
        if data.len() < Self::SIZE {
            bail!("JXSV payload header too short");
        }

        let mut r = BitReader::endian(&data[..Self::SIZE], BigEndian);
        Ok(Self {
            transmission_mode: r.read_bit().context("transmission_mode")?,
            packetization_mode: r.read_bit().context("packetization_mode")?,
            last: r.read_bit().context("last")?,
            interlaced: r.read::<2, u8>().context("interlaced")?,
            frame_counter: r.read::<5, u8>().context("frame_counter")?,
            sep_counter: r.read::<11, u16>().context("sep_counter")?,
            p_counter: r.read::<11, u16>().context("p_counter")?,
        })
    }

    pub fn progressive_codestream(frame_counter: u8, packet_index: u32, last: bool) -> Self {
        let p_counter = (packet_index % 2048) as u16;
        let sep_counter = (packet_index / 2048) as u16;
        Self {
            transmission_mode: true,
            packetization_mode: false,
            last,
            interlaced: 0,
            frame_counter: frame_counter & 0x1f,
            sep_counter,
            p_counter,
        }
    }
}

pub fn format_exact_framerate(framerate: gst::Fraction) -> String {
    if framerate.denom() == 1 {
        framerate.numer().to_string()
    } else {
        format!("{}/{}", framerate.numer(), framerate.denom())
    }
}

pub fn parse_exact_framerate(s: &str) -> Result<gst::Fraction, anyhow::Error> {
    let s = s.trim();
    if let Some((numer, denom)) = s.split_once('/') {
        let numer = numer
            .trim()
            .parse::<i32>()
            .context("exactframerate numerator")?;
        let denom = denom
            .trim()
            .parse::<i32>()
            .context("exactframerate denominator")?;
        Ok(gst::Fraction::new(numer, denom))
    } else {
        let numer = s.parse::<i32>().context("exactframerate")?;
        Ok(gst::Fraction::new(numer, 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_progressive_codestream_header() {
        let header = PayloadHeader::progressive_codestream(7, 2049, true);
        let packed = header.pack().unwrap();
        let parsed = PayloadHeader::parse(&packed).unwrap();
        assert_eq!(header, parsed);
        assert_eq!(header.p_counter, 1);
        assert_eq!(header.sep_counter, 1);
    }

    #[test]
    fn roundtrip_first_non_last_packet() {
        let header = PayloadHeader::progressive_codestream(0, 0, false);
        let packed = header.pack().unwrap();
        assert_eq!(PayloadHeader::parse(&packed).unwrap(), header);
        // T=1, K=0, L=0, I=0, F=0, SEP=0, P=0
        assert_eq!(packed, [0x80, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn pack_rejects_out_of_range_fields() {
        let mut header = PayloadHeader::progressive_codestream(0, 0, true);
        header.frame_counter = 32;
        assert!(header.pack().is_err());
    }
}
