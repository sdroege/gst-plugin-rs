// GStreamer RTP H.261 bitstream parsing
//
// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! H.261 (ITU-T H.261, 03/93) bitstream layer used by the payloader to
//! locate macroblock boundaries.
//!
//! The bit cursor is hand-written rather than the crate's `bitstream-io`
//! dependency: prefix-code decoding needs 16-bit peeks and arbitrary
//! repositioning, which `bitstream-io` 4 does not provide.
//!
//! The parser only decodes enough of the syntax to find where each
//! macroblock ends: picture header, GOB headers, and the macroblock layer
//! (MBA, MTYPE, MQUANT, MVD, CBP and TCOEFF). Coefficient values are not
//! decoded, only skipped.
//!
//! All entry points operate on a buffer padded with at least 4 zero bytes
//! past the data end; the padding makes 16-bit table
//! peeks near the end of the buffer safe and makes truncated streams
//! terminate as `EndOfBuffer` or `Invalid` instead of reading garbage.

/// MTYPE flags (H.261 Table 2), decoded into the flags each macroblock
/// carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mtype(u16);

impl Mtype {
    pub const INTER: Self = Self(1 << 0);
    pub const MC: Self = Self(1 << 2);
    pub const MQUANT: Self = Self(1 << 4);
    pub const MVD: Self = Self(1 << 5);
    pub const CBP: Self = Self(1 << 6);
    pub const TCOEFF: Self = Self(1 << 7);

    const fn bits(self) -> u16 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Mtype {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// Picture start code: 20 bits `0000 0000 0000 0001 0000` (H.261 4.2.1.1).
const PSC: u32 = 0x00010;
const PSC_BITS: u32 = 20;
/// Group of blocks start code: 16 bits `0000 0000 0000 0001` (H.261 4.2.2.1).
const GBSC: u16 = 0x0001;
const GBSC_BITS: u32 = 16;

/// Number of zero bytes callers must append past the data end.
pub const PAD_BYTES: usize = 4;

/// Why a parse failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The data ended in the middle of a syntax element.
    #[error("the data ended in the middle of a syntax element")]
    EndOfBuffer,
    /// No codeword matched at the current position.
    #[error("no codeword matched at the cursor position")]
    Invalid,
    /// Fewer group start codes than the picture header announced.
    #[error("fewer group start codes than the picture header announced")]
    TooFewGobs,
}

/// A macroblock header, in the form needed to predict the next one and to
/// fill in RTP payload header state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Macroblock {
    /// Macroblock address within the GOB, 1..=33; 0 means "no macroblock
    /// emitted yet in this GOB".
    pub mba: u8,
    /// GOB number this macroblock belongs to.
    pub gobn: u8,
    /// Quantizer in effect after this macroblock (GQUANT or MQUANT).
    pub quant: u8,
    /// Horizontal motion vector, valid when `mtype` contains `MVD`.
    pub mvx: i8,
    /// Vertical motion vector, valid when `mtype` contains `MVD`.
    pub mvy: i8,
    /// Decoded MTYPE flags.
    pub mtype: Mtype,
}

/// Position-based bit cursor over a padded slice. `data_bits` is the real
/// data end; the padding is only readable through `peek16`.
#[derive(Clone, Debug)]
pub struct BitCursor<'a> {
    data: &'a [u8],
    data_bits: usize,
    pos: usize,
}

impl<'a> BitCursor<'a> {
    pub fn new(data: &'a [u8], data_bits: usize) -> Self {
        debug_assert!(data.len() * 8 >= data_bits + PAD_BYTES * 8);
        Self {
            data,
            data_bits,
            pos: 0,
        }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) {
        debug_assert!(pos <= self.data_bits);
        self.pos = pos;
    }

    /// The 16 bits at the current position, without consuming them. Safe
    /// for any `pos <= data_bits` thanks to the buffer padding.
    pub fn peek16(&self) -> u16 {
        (self.peek32() >> 16) as u16
    }

    /// The 32 bits at the cursor's byte, shifted left by the bit offset.
    fn peek32(&self) -> u32 {
        let byte = self.pos / 8;
        u32::from_be_bytes([
            self.data[byte],
            self.data[byte + 1],
            self.data[byte + 2],
            self.data[byte + 3],
        ]) << (self.pos % 8)
    }

    /// Read `n` bits (0 < n <= 20) as a big-endian value.
    pub fn read(&mut self, n: u32) -> Result<u32, ParseError> {
        debug_assert!(n > 0 && n <= 20);
        if self.pos + n as usize > self.data_bits {
            return Err(ParseError::EndOfBuffer);
        }
        let value = self.peek32() >> (32 - n);
        self.pos += n as usize;
        Ok(value)
    }

    pub fn skip(&mut self, n: u32) -> Result<(), ParseError> {
        if self.pos + n as usize > self.data_bits {
            return Err(ParseError::EndOfBuffer);
        }
        self.pos += n as usize;
        Ok(())
    }
}

/// Bit offset (0..8) at which the picture start code starts in the first
/// four bytes, or `None` if the 20-bit PSC pattern does not appear.
pub fn picture_header_offset(data: &[u8]) -> Option<usize> {
    if data.len() < 4 {
        return None;
    }
    let v = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    (0..8).find(|i| (v >> (12 - i)) & 0xF_FFFF == PSC)
}

/// Parsed picture header: the number of GOBs the picture carries.
pub fn parse_picture_header(cur: &mut BitCursor) -> Result<usize, ParseError> {
    if cur.read(PSC_BITS)? != PSC {
        return Err(ParseError::Invalid);
    }
    // TR (4.2.1.2): 5 bits.
    cur.skip(5)?;
    // PTYPE (4.2.1.3): bit 4 is the source format, 0 = QCIF (3 GOBs),
    // 1 = CIF (12 GOBs). PEI/PSPARE bits are not parsed: the GOB scan
    // resynchronizes on the group start codes.
    let ptype = cur.read(6)?;
    Ok(if ptype & 0x04 != 0 { 12 } else { 3 })
}

/// A GOB start position and the extent it covers in the buffer.
#[derive(Clone, Copy, Debug)]
pub struct Gob {
    /// Bit position of the GOB start code.
    pub start: usize,
    /// Bit position where the next GOB (or the data end) starts.
    pub end: usize,
}

/// Find the bit position of the next group start code at or after `from`.
fn find_gob_start(data: &[u8], data_bits: usize, from: usize) -> Option<usize> {
    let last = data_bits.checked_sub(GBSC_BITS as usize)?;
    (from..=last).find(|&p| {
        let byte = p / 8;
        let v = u32::from_be_bytes([data[byte], data[byte + 1], data[byte + 2], data[byte + 3]]);
        ((v << (p % 8)) >> 16) as u16 == GBSC
    })
}

/// Locate `num_gobs` GOB start codes after the picture header and record
/// each GOB's extent.
pub fn scan_gobs(
    data: &[u8],
    data_bits: usize,
    num_gobs: usize,
    from: usize,
) -> Result<Vec<Gob>, ParseError> {
    let mut gobs = Vec::with_capacity(num_gobs);
    let mut pos = from;
    for i in 0..num_gobs {
        let start = find_gob_start(data, data_bits, pos).ok_or(ParseError::TooFewGobs)?;
        pos = start + GBSC_BITS as usize;
        gobs.push(Gob {
            start,
            end: data_bits,
        });
        if i > 0 {
            gobs[i - 1].end = start;
        }
    }
    Ok(gobs)
}

/// Parsed GOB header fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GobHeader {
    pub gn: u8,
    pub gquant: u8,
}

/// Parse a GOB header (start code, GN, GQUANT and any GEI/GSPARE pairs)
/// at the cursor position.
pub fn parse_gob_header(cur: &mut BitCursor) -> Result<GobHeader, ParseError> {
    if cur.read(GBSC_BITS)? != GBSC as u32 {
        return Err(ParseError::Invalid);
    }
    let gn = cur.read(4)? as u8;
    let gquant = cur.read(5)? as u8;
    // GEI/GSPARE (4.2.2.4/4.2.2.5): while GEI is 1, 8 spare bits follow,
    // then the next GEI.
    while cur.read(1)? != 0 {
        cur.skip(8)?;
    }
    Ok(GobHeader { gn, gquant })
}

/// The table entry whose codeword matches the bits at the cursor,
/// without consuming it.
fn decode<'a, V>(
    cur: &BitCursor,
    table: &'a [(u16, u16, u32, V)],
) -> Option<&'a (u16, u16, u32, V)> {
    let code = cur.peek16();
    table.iter().find(|(c, m, ..)| code & m == *c)
}

/// Decode a codeword from `table`, consume it, and return its value.
fn decode_skip<V: Copy>(
    cur: &mut BitCursor,
    table: &[(u16, u16, u32, V)],
) -> Result<V, ParseError> {
    let (_, _, len, value) = *decode(cur, table).ok_or(ParseError::Invalid)?;
    cur.skip(len)?;
    Ok(value)
}

/// Outcome of parsing one macroblock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MbResult {
    /// A macroblock was parsed; the cursor is at its end.
    Parsed(Macroblock),
    /// The next group start code was reached; the cursor is at that start
    /// code.
    EndOfGob,
    /// The end of the picture was reached (zero padding or data end); the
    /// cursor is at the end of the last macroblock.
    EndOfFrame,
}

/// Decode one macroblock starting at the cursor position, using `prev`
/// (the previous macroblock in the same GOB) for prediction.
pub fn parse_mb(cur: &mut BitCursor, prev: &Macroblock) -> Result<MbResult, ParseError> {
    let mut mb = Macroblock {
        quant: prev.quant,
        gobn: prev.gobn,
        ..Default::default()
    };

    // MBA (H.261 4.2.3.1, Table 1): difference from the previous macroblock
    // address, with a stuffing codeword and the group start code as
    // terminators.
    let diff = loop {
        match decode(cur, MBA_TABLE) {
            Some((_, _, len, MBA_STUFFING)) => cur.skip(*len)?,
            Some((_, _, _, MBA_START_CODE)) => return Ok(MbResult::EndOfGob),
            Some((_, _, len, value)) => {
                cur.skip(*len)?;
                break *value;
            }
            // The zero padding decodes to no codeword: the picture ended.
            None if cur.peek16() == 0 => return Ok(MbResult::EndOfFrame),
            None => return Err(ParseError::Invalid),
        }
    };
    // The "no macroblock emitted yet in this GOB" sentinel is 0, so the
    // first address of a GOB is the difference itself.
    mb.mba = prev.mba + diff;
    // A damaged stream can address past the last macroblock; reject it
    // here so downstream users (payload header fields) stay in range.
    if mb.mba > 33 {
        return Err(ParseError::Invalid);
    }

    // MTYPE (4.2.3.2, Table 2).
    mb.mtype = Mtype(decode_skip(cur, MTYPE_TABLE)?);

    // MQUANT (4.2.3.3).
    if mb.mtype.contains(Mtype::MQUANT) {
        mb.quant = cur.read(5)? as u8;
    }

    // MVD (4.2.3.4, Table 3): each codeword is a pair of difference
    // values; only one of the pair keeps the vector within +/-15. The
    // predictor is zero for macroblocks 1, 12 and 23, when the address
    // difference is not 1, or when the previous macroblock was not MC.
    if mb.mtype.contains(Mtype::MVD) {
        let zero_predictor =
            matches!(mb.mba, 1 | 12 | 23) || diff != 1 || !prev.mtype.contains(Mtype::MC);
        let predictor = if zero_predictor {
            [0i8; 2]
        } else {
            [prev.mvx, prev.mvy]
        };
        let mut mv = [0i8; 2];
        for axis in 0..2 {
            let [d1, d2] = decode_skip(cur, MVD_TABLE)?;
            let pred = predictor[axis];
            // Exactly one of the pair keeps the vector within +/-15; a
            // codeword where neither does is damaged data.
            mv[axis] = if (pred + d1).abs() <= 15 {
                pred + d1
            } else if (pred + d2).abs() <= 15 {
                pred + d2
            } else {
                return Err(ParseError::Invalid);
            };
        }
        mb.mvx = mv[0];
        mb.mvy = mv[1];
    }

    // CBP (4.2.3.5, Table 4); INTRA macroblocks imply all six blocks.
    let mut cbp = 0x3fu8;
    if mb.mtype.contains(Mtype::CBP) {
        cbp = decode_skip(cur, CBP_TABLE)?;
    }

    // Block layer (4.2.4): one TCOEFF run per block flagged by CBP, in
    // block order.
    if mb.mtype.contains(Mtype::TCOEFF) {
        for block in 0..6 {
            if cbp & (1 << (5 - block)) != 0 {
                parse_tcoeff(cur, mb.mtype)?;
            }
        }
    }

    Ok(MbResult::Parsed(mb))
}

/// Decode (skip) one block's transform coefficients (4.2.4.1, Tables 5
/// and 6). Coefficient values are not needed, only the codeword lengths.
fn parse_tcoeff(cur: &mut BitCursor, mtype: Mtype) -> Result<(), ParseError> {
    if mtype.contains(Mtype::INTER) {
        // The first INTER codeword uses a table where EOB is absent: a
        // leading 1 is the 2-bit codeword for (run 0, level 1).
        if cur.peek16() & 0x8000 != 0 {
            cur.skip(2)?;
        }
    } else {
        // The first INTRA coefficient is fixed-length (8 bits).
        cur.skip(8)?;
    }

    loop {
        match decode_skip(cur, TCOEFF_TABLE)? {
            TCOEFF_EOB => break,
            TCOEFF_ESC => {
                // Escape: 6-bit run + 8-bit level follow the codeword.
                cur.skip(14)?;
            }
            _ => (),
        }
    }

    Ok(())
}

const MBA_STUFFING: u8 = 34;
const MBA_START_CODE: u8 = 35;

// (code, mask, length, value)
static MBA_TABLE: &[(u16, u16, u32, u8)] = &[
    (0x8000, 0x8000, 1, 1),
    (0x6000, 0xe000, 3, 2),
    (0x4000, 0xe000, 3, 3),
    (0x3000, 0xf000, 4, 4),
    (0x2000, 0xf000, 4, 5),
    (0x1800, 0xf800, 5, 6),
    (0x1000, 0xf800, 5, 7),
    (0x0e00, 0xfe00, 7, 8),
    (0x0c00, 0xfe00, 7, 9),
    (0x0b00, 0xff00, 8, 10),
    (0x0a00, 0xff00, 8, 11),
    (0x0900, 0xff00, 8, 12),
    (0x0800, 0xff00, 8, 13),
    (0x0700, 0xff00, 8, 14),
    (0x0600, 0xff00, 8, 15),
    (0x05c0, 0xffc0, 10, 16),
    (0x0580, 0xffc0, 10, 17),
    (0x0540, 0xffc0, 10, 18),
    (0x0500, 0xffc0, 10, 19),
    (0x04c0, 0xffc0, 10, 20),
    (0x0480, 0xffc0, 10, 21),
    (0x0460, 0xffe0, 11, 22),
    (0x0440, 0xffe0, 11, 23),
    (0x0420, 0xffe0, 11, 24),
    (0x0400, 0xffe0, 11, 25),
    (0x03e0, 0xffe0, 11, 26),
    (0x03c0, 0xffe0, 11, 27),
    (0x03a0, 0xffe0, 11, 28),
    (0x0380, 0xffe0, 11, 29),
    (0x0360, 0xffe0, 11, 30),
    (0x0340, 0xffe0, 11, 31),
    (0x0320, 0xffe0, 11, 32),
    (0x0300, 0xffe0, 11, 33),
    (0x01e0, 0xffe0, 11, MBA_STUFFING),
    (0x0001, 0xffff, 16, MBA_START_CODE),
];

// (code, mask, length, MTYPE flags)
static MTYPE_TABLE: &[(u16, u16, u32, u16)] = &[
    (
        0x8000,
        0x8000,
        1,
        Mtype::INTER.bits() | Mtype::CBP.bits() | Mtype::TCOEFF.bits(),
    ),
    (
        0x4000,
        0xc000,
        2,
        Mtype::INTER.bits()
            | Mtype::MC.bits()
            | Mtype::MVD.bits()
            | Mtype::CBP.bits()
            | Mtype::TCOEFF.bits(),
    ),
    (
        0x2000,
        0xe000,
        3,
        Mtype::INTER.bits() | Mtype::MC.bits() | Mtype::MVD.bits(),
    ),
    (0x1000, 0xf000, 4, Mtype::TCOEFF.bits()),
    (
        0x0800,
        0xf800,
        5,
        Mtype::INTER.bits() | Mtype::MQUANT.bits() | Mtype::CBP.bits() | Mtype::TCOEFF.bits(),
    ),
    (
        0x0400,
        0xfc00,
        6,
        Mtype::INTER.bits()
            | Mtype::MC.bits()
            | Mtype::MQUANT.bits()
            | Mtype::MVD.bits()
            | Mtype::CBP.bits()
            | Mtype::TCOEFF.bits(),
    ),
    (
        0x0200,
        0xfe00,
        7,
        Mtype::MQUANT.bits() | Mtype::TCOEFF.bits(),
    ),
    (
        0x0100,
        0xff00,
        8,
        Mtype::INTER.bits()
            | Mtype::MC.bits()
            | Mtype::MVD.bits()
            | Mtype::CBP.bits()
            | Mtype::TCOEFF.bits(),
    ),
    (
        0x0080,
        0xff80,
        9,
        Mtype::INTER.bits() | Mtype::MC.bits() | Mtype::MVD.bits(),
    ),
    (
        0x0040,
        0xffc0,
        10,
        Mtype::INTER.bits()
            | Mtype::MC.bits()
            | Mtype::MQUANT.bits()
            | Mtype::MVD.bits()
            | Mtype::CBP.bits()
            | Mtype::TCOEFF.bits(),
    ),
];

// (code, mask, length, difference candidate pair)
// Codewords are prefix-free, so the linear table scans are order-independent.
static MVD_TABLE: &[(u16, u16, u32, [i8; 2])] = &[
    (0x8000, 0x8000, 1, [0, 0]),
    (0x6000, 0xe000, 3, [-1, -1]),
    (0x4000, 0xe000, 3, [1, 1]),
    (0x3000, 0xf000, 4, [-2, 30]),
    (0x2000, 0xf000, 4, [2, -30]),
    (0x1800, 0xf800, 5, [-3, 29]),
    (0x1000, 0xf800, 5, [3, -29]),
    (0x0e00, 0xfe00, 7, [-4, 28]),
    (0x0c00, 0xfe00, 7, [4, -28]),
    (0x0700, 0xff00, 8, [-7, 25]),
    (0x0900, 0xff00, 8, [-6, 26]),
    (0x0b00, 0xff00, 8, [-5, 27]),
    (0x0a00, 0xff00, 8, [5, -27]),
    (0x0800, 0xff00, 8, [6, -26]),
    (0x0600, 0xff00, 8, [7, -25]),
    (0x04c0, 0xffc0, 10, [-10, 22]),
    (0x0540, 0xffc0, 10, [-9, 23]),
    (0x05c0, 0xffc0, 10, [-8, 24]),
    (0x0580, 0xffc0, 10, [8, -24]),
    (0x0500, 0xffc0, 10, [9, -23]),
    (0x0480, 0xffc0, 10, [10, -22]),
    // Table 3 defines +16 only as the pair partner of '-16 & 16'
    // (00000011001); 00000011000 is an unused code. FFmpeg's encoder
    // emits it for +16, so accept it for interoperability.
    (0x0300, 0xffe0, 11, [16, -16]),
    (0x0320, 0xffe0, 11, [-16, 16]),
    (0x0360, 0xffe0, 11, [-15, 17]),
    (0x03a0, 0xffe0, 11, [-14, 18]),
    (0x03e0, 0xffe0, 11, [-13, 19]),
    (0x0420, 0xffe0, 11, [-12, 20]),
    (0x0460, 0xffe0, 11, [-11, 21]),
    (0x0440, 0xffe0, 11, [11, -21]),
    (0x0400, 0xffe0, 11, [12, -20]),
    (0x03c0, 0xffe0, 11, [13, -19]),
    (0x0380, 0xffe0, 11, [14, -18]),
    (0x0340, 0xffe0, 11, [15, -17]),
];

// (code, mask, length, coded block pattern)
static CBP_TABLE: &[(u16, u16, u32, u8)] = &[
    (0xe000, 0xe000, 3, 60),
    (0xd000, 0xf000, 4, 4),
    (0xc000, 0xf000, 4, 8),
    (0xb000, 0xf000, 4, 16),
    (0xa000, 0xf000, 4, 32),
    (0x9800, 0xf800, 5, 12),
    (0x9000, 0xf800, 5, 48),
    (0x8800, 0xf800, 5, 20),
    (0x8000, 0xf800, 5, 40),
    (0x7800, 0xf800, 5, 28),
    (0x7000, 0xf800, 5, 44),
    (0x6800, 0xf800, 5, 52),
    (0x6000, 0xf800, 5, 56),
    (0x5800, 0xf800, 5, 1),
    (0x5000, 0xf800, 5, 61),
    (0x4800, 0xf800, 5, 2),
    (0x4000, 0xf800, 5, 62),
    (0x3c00, 0xfc00, 6, 24),
    (0x3800, 0xfc00, 6, 36),
    (0x3400, 0xfc00, 6, 3),
    (0x3000, 0xfc00, 6, 63),
    (0x2e00, 0xfe00, 7, 5),
    (0x2c00, 0xfe00, 7, 9),
    (0x2a00, 0xfe00, 7, 17),
    (0x2800, 0xfe00, 7, 33),
    (0x2600, 0xfe00, 7, 6),
    (0x2400, 0xfe00, 7, 10),
    (0x2200, 0xfe00, 7, 18),
    (0x2000, 0xfe00, 7, 34),
    (0x1f00, 0xff00, 8, 7),
    (0x1e00, 0xff00, 8, 11),
    (0x1d00, 0xff00, 8, 19),
    (0x1c00, 0xff00, 8, 35),
    (0x1b00, 0xff00, 8, 13),
    (0x1a00, 0xff00, 8, 49),
    (0x1900, 0xff00, 8, 21),
    (0x1800, 0xff00, 8, 41),
    (0x1700, 0xff00, 8, 14),
    (0x1600, 0xff00, 8, 50),
    (0x1500, 0xff00, 8, 22),
    (0x1400, 0xff00, 8, 42),
    (0x1300, 0xff00, 8, 15),
    (0x1200, 0xff00, 8, 51),
    (0x1100, 0xff00, 8, 23),
    (0x1000, 0xff00, 8, 43),
    (0x0f00, 0xff00, 8, 25),
    (0x0e00, 0xff00, 8, 37),
    (0x0d00, 0xff00, 8, 26),
    (0x0c00, 0xff00, 8, 38),
    (0x0b00, 0xff00, 8, 29),
    (0x0a00, 0xff00, 8, 45),
    (0x0900, 0xff00, 8, 53),
    (0x0800, 0xff00, 8, 57),
    (0x0700, 0xff00, 8, 30),
    (0x0600, 0xff00, 8, 46),
    (0x0500, 0xff00, 8, 54),
    (0x0400, 0xff00, 8, 58),
    (0x0380, 0xff80, 9, 31),
    (0x0300, 0xff80, 9, 47),
    (0x0280, 0xff80, 9, 55),
    (0x0200, 0xff80, 9, 59),
    (0x0180, 0xff80, 9, 27),
    (0x0100, 0xff80, 9, 39),
];

const TCOEFF_EOB: i16 = -1;
const TCOEFF_ESC: i16 = -2;

// (code, mask, length, run; TCOEFF_EOB/TCOEFF_ESC are sentinels)
static TCOEFF_TABLE: &[(u16, u16, u32, i16)] = &[
    (0x8000, 0xc000, 2, TCOEFF_EOB),
    (0xc000, 0xc000, 3, 0),
    (0x6000, 0xe000, 4, 1),
    (0x4000, 0xf000, 5, 0),
    (0x5000, 0xf000, 5, 2),
    (0x2800, 0xf800, 6, 0),
    (0x3800, 0xf800, 6, 3),
    (0x3000, 0xf800, 6, 4),
    (0x0400, 0xfc00, 6, TCOEFF_ESC),
    (0x1800, 0xfc00, 7, 1),
    (0x1c00, 0xfc00, 7, 5),
    (0x1400, 0xfc00, 7, 6),
    (0x1000, 0xfc00, 7, 7),
    (0x0c00, 0xfe00, 8, 0),
    (0x0800, 0xfe00, 8, 2),
    (0x0e00, 0xfe00, 8, 8),
    (0x0a00, 0xfe00, 8, 9),
    (0x2600, 0xff00, 9, 0),
    (0x2100, 0xff00, 9, 0),
    (0x2500, 0xff00, 9, 1),
    (0x2400, 0xff00, 9, 3),
    (0x2700, 0xff00, 9, 10),
    (0x2300, 0xff00, 9, 11),
    (0x2200, 0xff00, 9, 12),
    (0x2000, 0xff00, 9, 13),
    (0x0280, 0xffc0, 11, 0),
    (0x0300, 0xffc0, 11, 1),
    (0x02c0, 0xffc0, 11, 2),
    (0x03c0, 0xffc0, 11, 4),
    (0x0240, 0xffc0, 11, 5),
    (0x0380, 0xffc0, 11, 14),
    (0x0340, 0xffc0, 11, 15),
    (0x0200, 0xffc0, 11, 16),
    (0x01d0, 0xfff0, 13, 0),
    (0x0180, 0xfff0, 13, 0),
    (0x0130, 0xfff0, 13, 0),
    (0x0100, 0xfff0, 13, 0),
    (0x01b0, 0xfff0, 13, 1),
    (0x0140, 0xfff0, 13, 2),
    (0x01c0, 0xfff0, 13, 3),
    (0x0120, 0xfff0, 13, 4),
    (0x01e0, 0xfff0, 13, 6),
    (0x0150, 0xfff0, 13, 7),
    (0x0110, 0xfff0, 13, 8),
    (0x01f0, 0xfff0, 13, 17),
    (0x01a0, 0xfff0, 13, 18),
    (0x0190, 0xfff0, 13, 19),
    (0x0170, 0xfff0, 13, 20),
    (0x0160, 0xfff0, 13, 21),
    (0x00d0, 0xfff8, 14, 0),
    (0x00c8, 0xfff8, 14, 0),
    (0x00c0, 0xfff8, 14, 0),
    (0x00b8, 0xfff8, 14, 0),
    (0x00b0, 0xfff8, 14, 1),
    (0x00a8, 0xfff8, 14, 1),
    (0x00a0, 0xfff8, 14, 2),
    (0x0098, 0xfff8, 14, 3),
    (0x0090, 0xfff8, 14, 5),
    (0x0088, 0xfff8, 14, 9),
    (0x0080, 0xfff8, 14, 10),
    (0x00f8, 0xfff8, 14, 22),
    (0x00f0, 0xfff8, 14, 23),
    (0x00e8, 0xfff8, 14, 24),
    (0x00e0, 0xfff8, 14, 25),
    (0x00d8, 0xfff8, 14, 26),
];

#[cfg(test)]
pub mod test_util {
    use super::*;

    /// Minimal MSB-first bit writer for test vectors.
    pub struct BitWriter {
        pub bytes: Vec<u8>,
        bit: usize,
    }

    impl BitWriter {
        pub fn new() -> Self {
            Self {
                bytes: Vec::new(),
                bit: 0,
            }
        }

        pub fn write(&mut self, n: u32, value: u32) {
            for i in (0..n).rev() {
                if self.bit.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if (value >> i) & 1 != 0 {
                    let last = self.bytes.len() - 1;
                    self.bytes[last] |= 1 << (7 - self.bit % 8);
                }
                self.bit += 1;
            }
        }

        /// Finish the picture: terminate with zero bits up to the byte
        /// boundary (the trailing zeros mark the end of the picture).
        pub fn finish(mut self) -> Vec<u8> {
            while !self.bit.is_multiple_of(8) {
                self.write(1, 0);
            }
            self.bytes
        }
    }

    /// One QCIF picture header: PSC + TR=1 + PTYPE (QCIF, 3 GOBs).
    pub fn picture_header(w: &mut BitWriter) {
        w.write(20, PSC);
        w.write(5, 1); // TR
        w.write(6, 0x02); // PTYPE: still image off, QCIF
    }

    /// One GOB header plus `num_mbs` inter macroblocks with 4 coded blocks
    /// each (21 bits per macroblock).
    pub fn gob(w: &mut BitWriter, gn: u8, gquant: u8, num_mbs: u8) {
        w.write(16, GBSC as u32);
        w.write(4, gn as u32);
        w.write(5, gquant as u32);
        w.write(1, 0); // GEI
        for _ in 0..num_mbs {
            w.write(1, 1); // MBA: difference 1
            w.write(1, 1); // MTYPE: INTER | CBP | TCOEFF
            w.write(3, 0b111); // CBP: 60 = blocks 0..3
            for _ in 0..4 {
                w.write(2, 0b10); // first INTER coefficient (run 0, level 1)
                w.write(2, 0b10); // EOB
            }
        }
    }

    /// A complete QCIF picture, without the parser padding.
    pub fn qcif_frame(gobs: &[(u8, u8, u8)]) -> Vec<u8> {
        let mut w = BitWriter::new();
        picture_header(&mut w);
        for &(gn, gq, n) in gobs {
            gob(&mut w, gn, gq, n);
        }
        w.finish()
    }

    /// Append the zero padding the parser requires.
    pub fn pad_bits(mut data: Vec<u8>) -> Vec<u8> {
        data.resize(data.len() + PAD_BYTES, 0);
        data
    }

    /// A complete QCIF picture padded for direct parser use.
    pub fn qcif_frame_padded(gobs: &[(u8, u8, u8)]) -> Vec<u8> {
        pad_bits(qcif_frame(gobs))
    }

    pub fn cursor(data: &[u8]) -> BitCursor<'_> {
        BitCursor::new(data, (data.len() - PAD_BYTES) * 8)
    }
}

#[cfg(test)]
mod unit_tests {
    use super::test_util::*;
    use super::*;

    #[test]
    fn picture_header_offset_finds_unaligned_psc() {
        for off in 0..8u32 {
            let mut w = BitWriter::new();
            w.write(off, 0); // leading zero bits
            w.write(20, PSC);
            w.write(5, 1);
            w.write(6, 0x02);
            let data = pad_bits(w.finish());
            assert_eq!(picture_header_offset(&data), Some(off as usize));
        }
    }

    #[test]
    fn picture_header_offset_rejects_missing_psc() {
        assert_eq!(picture_header_offset(&[0x00, 0x00, 0x00, 0x02]), None);
        assert_eq!(picture_header_offset(&[0x00, 0x00]), None);
    }

    #[test]
    fn parse_picture_header_qcif_vs_cif() {
        for (ptype_bit, expected) in [(false, 3), (true, 12)] {
            let mut w = BitWriter::new();
            w.write(20, PSC);
            w.write(5, 7); // TR
            w.write(6, if ptype_bit { 0x06 } else { 0x02 });
            let data = pad_bits(w.finish());
            let mut cur = cursor(&data);
            assert_eq!(parse_picture_header(&mut cur), Ok(expected));
            assert_eq!(cur.pos(), 31);
        }
    }

    #[test]
    fn parse_picture_header_rejects_bad_psc() {
        let data = [0x00u8; 12];
        let mut cur = BitCursor::new(&data, 64);
        assert_eq!(parse_picture_header(&mut cur), Err(ParseError::Invalid));
    }

    #[test]
    fn parse_gob_header_with_and_without_gspare() {
        let mut w = BitWriter::new();
        w.write(16, GBSC as u32);
        w.write(4, 5); // GN
        w.write(5, 12); // GQUANT
        w.write(1, 1); // GEI
        w.write(8, 0xAB); // GSPARE
        w.write(1, 0); // GEI
        let data = pad_bits(w.finish());
        let mut cur = cursor(&data);
        assert_eq!(
            parse_gob_header(&mut cur),
            Ok(GobHeader { gn: 5, gquant: 12 })
        );
        assert_eq!(cur.pos(), 16 + 4 + 5 + 1 + 8 + 1);
    }

    #[test]
    fn scan_gobs_finds_all_gobs() {
        let data = qcif_frame_padded(&[(1, 10, 2), (2, 11, 0), (3, 12, 1)]);
        let mut cur = cursor(&data);
        let num_gobs = parse_picture_header(&mut cur).unwrap();
        let gobs = scan_gobs(&data, (data.len() - PAD_BYTES) * 8, num_gobs, cur.pos()).unwrap();
        assert_eq!(gobs.len(), 3);
        assert!(gobs[0].start < gobs[1].start && gobs[1].start < gobs[2].start);
        assert_eq!(gobs[2].end, (data.len() - PAD_BYTES) * 8);
    }

    #[test]
    fn scan_gobs_fails_when_missing() {
        let data = qcif_frame_padded(&[(1, 10, 2)]); // 2 GOBs missing
        let mut cur = cursor(&data);
        let num_gobs = parse_picture_header(&mut cur).unwrap();
        assert!(scan_gobs(&data, (data.len() - PAD_BYTES) * 8, num_gobs, cur.pos()).is_err());
    }

    #[test]
    fn parse_inter_macroblocks() {
        let data = qcif_frame_padded(&[(1, 10, 3)]);
        let mut cur = cursor(&data);
        parse_picture_header(&mut cur).unwrap();
        let GobHeader { gn, gquant } = parse_gob_header(&mut cur).unwrap();
        assert_eq!((gn, gquant), (1, 10));

        let mut prev = Macroblock {
            mba: 0,
            gobn: gn,
            quant: gquant,
            ..Default::default()
        };
        for mba in 1..=3 {
            match parse_mb(&mut cur, &prev) {
                Ok(MbResult::Parsed(mb)) => {
                    assert_eq!(mb.mba, mba);
                    assert_eq!(mb.gobn, 1);
                    assert_eq!(mb.quant, 10); // inherited from GQUANT
                    assert!(mb.mtype.contains(Mtype::INTER | Mtype::CBP | Mtype::TCOEFF));
                    prev = mb;
                }
                other => panic!("expected macroblock {mba}, got {other:?}"),
            }
        }
        // The next group start code (here: frame padding zeros) ends the
        // picture.
        assert_eq!(parse_mb(&mut cur, &prev), Ok(MbResult::EndOfFrame));
    }

    #[test]
    fn parse_intra_macroblock() {
        let mut w = BitWriter::new();
        picture_header(&mut w);
        w.write(16, GBSC as u32);
        w.write(4, 1);
        w.write(5, 8); // GQUANT
        w.write(1, 0); // GEI
        w.write(1, 1); // MBA: difference 1
        w.write(4, 0b0001); // MTYPE: INTRA | TCOEFF
        for _ in 0..6 {
            w.write(8, 0x2a); // fixed-length first INTRA coefficient
            w.write(2, 0b10); // EOB
        }
        let data = pad_bits(w.finish());

        let mut cur = cursor(&data);
        parse_picture_header(&mut cur).unwrap();
        let GobHeader { gn, gquant } = parse_gob_header(&mut cur).unwrap();
        let prev = Macroblock {
            mba: 0,
            gobn: gn,
            quant: gquant,
            ..Default::default()
        };
        match parse_mb(&mut cur, &prev) {
            Ok(MbResult::Parsed(mb)) => {
                assert_eq!(mb.mba, 1);
                assert!(mb.mtype.contains(Mtype::TCOEFF));
                assert!(!mb.mtype.contains(Mtype::INTER));
            }
            other => panic!("expected macroblock, got {other:?}"),
        }
    }

    #[test]
    fn parse_motion_compensated_macroblock() {
        let mut w = BitWriter::new();
        picture_header(&mut w);
        w.write(16, GBSC as u32);
        w.write(4, 1);
        w.write(5, 8);
        w.write(1, 0);
        w.write(1, 1); // MBA: difference 1
        w.write(3, 0b001); // MTYPE: INTER | MC | FIL | MVD (no CBP/TCOFF)
        w.write(1, 1); // HMVD: 0
        w.write(1, 1); // VMVD: 0
        let data = pad_bits(w.finish());

        let mut cur = cursor(&data);
        parse_picture_header(&mut cur).unwrap();
        let GobHeader { gn, gquant } = parse_gob_header(&mut cur).unwrap();
        let prev = Macroblock {
            mba: 0,
            gobn: gn,
            quant: gquant,
            ..Default::default()
        };
        match parse_mb(&mut cur, &prev) {
            Ok(MbResult::Parsed(mb)) => {
                assert!(mb.mtype.contains(Mtype::MC | Mtype::MVD));
                assert_eq!((mb.mvx, mb.mvy), (0, 0));
            }
            other => panic!("expected macroblock, got {other:?}"),
        }
    }

    #[test]
    fn mba_stuffing_is_skipped() {
        let mut w = BitWriter::new();
        picture_header(&mut w);
        w.write(16, GBSC as u32);
        w.write(4, 1);
        w.write(5, 8);
        w.write(1, 0);
        w.write(11, 0x01e0 >> 5); // MBA stuffing codeword '00000001111'
        w.write(1, 1); // MBA: difference 1
        w.write(1, 1); // MTYPE
        w.write(3, 0b111); // CBP 60
        for _ in 0..4 {
            w.write(2, 0b10);
            w.write(2, 0b10);
        }
        let data = pad_bits(w.finish());

        let mut cur = cursor(&data);
        parse_picture_header(&mut cur).unwrap();
        let GobHeader { gn, gquant } = parse_gob_header(&mut cur).unwrap();
        let prev = Macroblock {
            mba: 0,
            gobn: gn,
            quant: gquant,
            ..Default::default()
        };
        match parse_mb(&mut cur, &prev) {
            Ok(MbResult::Parsed(mb)) => assert_eq!(mb.mba, 1),
            other => panic!("expected macroblock, got {other:?}"),
        }
    }

    #[test]
    fn next_gob_start_code_ends_the_gob() {
        let data = qcif_frame_padded(&[(1, 10, 1), (2, 11, 0)]);
        let mut cur = cursor(&data);
        parse_picture_header(&mut cur).unwrap();
        let GobHeader { gn, gquant } = parse_gob_header(&mut cur).unwrap();
        let prev = Macroblock {
            mba: 0,
            gobn: gn,
            quant: gquant,
            ..Default::default()
        };
        parse_mb(&mut cur, &prev).unwrap();
        assert_eq!(parse_mb(&mut cur, &prev), Ok(MbResult::EndOfGob));
    }

    /// Every codeword of ITU-T H.261 Table 3, transcribed from the
    /// published table, must decode with the exact length and value pair.
    #[test]
    fn mvd_table_matches_spec() {
        const SPEC: &[(u16, u16, u32, [i8; 2])] = &[
            (0x8000, 0x8000, 1, [0, 0]),
            (0x4000, 0xE000, 3, [1, 1]),
            (0x6000, 0xE000, 3, [-1, -1]),
            (0x2000, 0xF000, 4, [2, -30]),
            (0x3000, 0xF000, 4, [-2, 30]),
            (0x1000, 0xF800, 5, [3, -29]),
            (0x1800, 0xF800, 5, [-3, 29]),
            (0x0C00, 0xFE00, 7, [4, -28]),
            (0x0E00, 0xFE00, 7, [-4, 28]),
            (0x0A00, 0xFF00, 8, [5, -27]),
            (0x0800, 0xFF00, 8, [6, -26]),
            (0x0600, 0xFF00, 8, [7, -25]),
            (0x0700, 0xFF00, 8, [-7, 25]),
            (0x0900, 0xFF00, 8, [-6, 26]),
            (0x0B00, 0xFF00, 8, [-5, 27]),
            (0x0580, 0xFFC0, 10, [8, -24]),
            (0x0500, 0xFFC0, 10, [9, -23]),
            (0x0480, 0xFFC0, 10, [10, -22]),
            (0x04C0, 0xFFC0, 10, [-10, 22]),
            (0x0540, 0xFFC0, 10, [-9, 23]),
            (0x05C0, 0xFFC0, 10, [-8, 24]),
            (0x0400, 0xFFE0, 11, [12, -20]),
            (0x0440, 0xFFE0, 11, [11, -21]),
            (0x03C0, 0xFFE0, 11, [13, -19]),
            (0x0380, 0xFFE0, 11, [14, -18]),
            (0x0340, 0xFFE0, 11, [15, -17]),
            (0x0360, 0xFFE0, 11, [-15, 17]),
            (0x03A0, 0xFFE0, 11, [-14, 18]),
            (0x03E0, 0xFFE0, 11, [-13, 19]),
            (0x0420, 0xFFE0, 11, [-12, 20]),
            (0x0460, 0xFFE0, 11, [-11, 21]),
            (0x0320, 0xFFE0, 11, [-16, 16]),
        ];
        let mut got: Vec<(u16, u16, u32, [i8; 2])> = MVD_TABLE.to_vec();
        let mut want: Vec<(u16, u16, u32, [i8; 2])> = SPEC.to_vec();
        // The table additionally accepts 00000011000 (+16), undefined by
        // Table 3 but emitted by FFmpeg's encoder; see MVD_TABLE.
        want.push((0x0300, 0xFFE0, 11, [16, -16]));
        got.sort();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn parse_mc_macroblock_uses_vertical_predictor() {
        let mut w = BitWriter::new();
        picture_header(&mut w);
        w.write(16, GBSC as u32);
        w.write(4, 1);
        w.write(5, 8);
        w.write(1, 0);
        w.write(1, 1); // MBA: difference 1
        w.write(3, 0b001); // MTYPE: INTER | MC | FIL | MVD
        w.write(1, 1); // HMVD: 0
        w.write(4, 0b0010); // VMVD: +2 & -30
        let data = pad_bits(w.finish());

        let mut cur = cursor(&data);
        parse_picture_header(&mut cur).unwrap();
        let GobHeader { gn, gquant } = parse_gob_header(&mut cur).unwrap();
        let prev = Macroblock {
            mba: 2,
            mtype: Mtype::MC | Mtype::MVD,
            quant: gquant,
            gobn: gn,
            mvx: -3,
            mvy: 5,
        };
        match parse_mb(&mut cur, &prev) {
            Ok(MbResult::Parsed(mb)) => {
                assert_eq!(mb.mba, 3);
                // The vertical component predicts from the previous
                // vertical component (5 + 2), not the horizontal one.
                assert_eq!((mb.mvx, mb.mvy), (-3, 7));
            }
            other => panic!("expected macroblock, got {other:?}"),
        }
    }

    #[test]
    fn truncated_stream_reports_end_of_buffer() {
        // A GOB header cut short by the data end.
        let mut w = BitWriter::new();
        picture_header(&mut w);
        w.write(16, GBSC as u32);
        w.write(4, 1);
        let mut data = w.bytes;
        data.resize(data.len() + PAD_BYTES, 0);
        let mut cur = BitCursor::new(&data, 31 + 20);
        parse_picture_header(&mut cur).unwrap();
        assert_eq!(parse_gob_header(&mut cur), Err(ParseError::EndOfBuffer));
    }
}
