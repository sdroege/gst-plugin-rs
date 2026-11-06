//
// Copyright (C) 2026 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/// Re-packs a full group of G.726 codewords between the native MSB-first
/// (AAL2 / GStreamer `audio/x-adpcm layout=g726`) and the RFC 3551 LSB-first
/// bit ordering.
///
/// `width` is the codeword size in bits (3 for 24 kbit/s, 5 for 40 kbit/s).
/// `to_lsb` selects the direction: `true` converts native to RFC (payloader),
/// `false` converts RFC to native (depayload). The group must contain an
/// integer number of codewords (`group.len() * 8 % width == 0`) and at most
/// eight of them.
fn repack_g726(group: &mut [u8], width: usize, to_lsb: bool) {
    debug_assert_eq!(group.len() * 8 % width, 0);
    let num_codes = (group.len() * 8) / width;
    debug_assert!(num_codes <= 8);

    // Work in a view where each octet is LSB-first: a codeword then occupies
    // a contiguous `width`-bit run starting at bit `rem` of octet `i`, where
    // `i = idx * width / 8` and `rem = idx * width % 8`.
    //
    // Native packing is MSB-first in both its octets and its codeword bit
    // order, so its octets must be bit-reversed to enter this view and its
    // runs read back reversed; RFC 3551 packing is LSB-first in both, so it
    // is already in the view.
    //
    // The six-octet buffers leave room for the two-octet window of the last
    // codeword.
    let rev_code = |v: u16| u16::from(u8::reverse_bits(v as u8) >> (8 - width));
    let mut src = [0u8; 6];
    for (d, &b) in src.iter_mut().zip(group.iter()) {
        *d = if to_lsb { u8::reverse_bits(b) } else { b };
    }
    let mut dst = [0u8; 6];
    for idx in 0..num_codes {
        let base = idx * width;
        let (i, rem) = (base / 8, base % 8);
        let win = u16::from(src[i]) | u16::from(src[i + 1]) << 8;
        let run = (win >> rem) & ((1u16 << width) - 1);
        // The codeword value with its LSB at bit 0; native codewords are
        // MSB-first within their run, so the run is bit-reversed.
        let v = if to_lsb { rev_code(run) } else { run };
        let mut out = u16::from(dst[i]) | u16::from(dst[i + 1]) << 8;
        out |= (if to_lsb { v } else { rev_code(v) }) << rem;
        dst[i] = out as u8;
        dst[i + 1] = (out >> 8) as u8;
    }
    // RFC 3551 output is already LSB-first; native output needs its
    // MSB-first octets restored.
    let len = group.len();
    for (d, &s) in group.iter_mut().zip(&dst[..len]) {
        *d = if to_lsb { s } else { u8::reverse_bits(s) };
    }
}

pub fn g726_code_swap(data: &mut [u8], is_pay: bool, bitrate: i32) {
    // We need to reshuffle the input bytes to the required output
    //
    //   A stream of G726 codewords MUST be packed into
    //   octets as follows:  the first codeword is placed into the first octet
    //   such that the least significant bit of the codeword aligns with the
    //   least significant bit in the octet, the second codeword is then
    //   packed so that its least significant bit coincides with the least
    //   significant unoccupied bit in the octet.  When a complete codeword
    //   cannot be placed into an octet, the bits overlapping the octet
    //   boundary are placed into the least significant bits of the next
    //   octet.  Packing MUST end with a completely packed final octet.
    //
    // This is effectively an endian swap of the code points (not bits).
    //
    // Output is of the form A B C D ...
    //
    // Octet and codeword bit numbers in the diagrams below are MSB-0: bit 0
    // is the most significant bit (octet bit 0 is the 2^7 bit).
    match bitrate {
        // 0 1 2 3 4 5 6 7
        // ---------------
        // D D|C C|B B|A A ...
        // 0 1|0 1|0 1|0 1
        // ---------------
        // A A|B B|C C|D D ...
        // 0 1|0 1|0 1|0 1
        16000 => {
            for byte in data.iter_mut() {
                let tmp = *byte;
                *byte =
                    (tmp & 0xc0) >> 6 | (tmp & 0x30) >> 2 | (tmp & 0x0c) << 2 | (tmp & 0x03) << 6;
            }
        }
        // 0 1 2 3 4 5 6 7|8 9 0 1 2 3 4 5|6 7 8 9 0 1 2 3
        // -----------------------------------------------
        // C C|B B B|A A A|F|E E E|D D D|C|H H H|G G G|F F  ...
        // 1 2|0 1 2|0 1 2|2|0 1 2|0 1 2|0|0 1 2|0 1 2|0 1
        // -----------------------------------------------
        // A A A|B B B|C C C|D D D|E E E|F F F|G G G|H H H  ...
        // 0 1 2|0 1 2|0 1 2|0 1 2|0 1 2|0 1 2|0 1 2|0 1 2
        24000 => {
            for chunk in data.as_chunks_mut::<3>().0 {
                repack_g726(chunk, 3, is_pay);
            }
        }
        // 0 1 2 3 4 5 6 7|8 9 0 1 2 3 4 5
        // -------------------------------
        // B B B B|A A A A|D D D D|C C C C ...
        // 0 1 2 3|0 1 2 3|0 1 2 3|0 1 2 3
        // -------------------------------
        // A A A A|B B B B|C C C C|D D D D ...
        // 0 1 2 3|0 1 2 3|0 1 2 3|0 1 2 3
        32000 => {
            for byte in data.iter_mut() {
                let tmp = *byte;
                *byte = (tmp & 0xf0) >> 4 | (tmp & 0x0f) << 4;
            }
        }
        // 0 1 2 3 4 5 6 7|8 9 0 1 2 3 4 5|6 7 8 9 0 1 2 3|4 5 6 7 8 9 0 1|2 3 4 5 6 7 8 9
        // -------------------------------------------------------------------------------
        // B B B|A A A A A|D|C C C C C|B B|E E E E|D D D D|G G|F F F F F|E|H H H H H|G G G ...
        // 2 3 4|0 1 2 3 4|4|0 1 2 3 4|0 1|1 2 3 4|0 1 2 3|3 4|0 1 2 3 4|0|0 1 2 3 4|0 1 2
        // -------------------------------------------------------------------------------
        // A A A A A|B B B B B|C C C C C|D D D D D|E E E E E|F F F F F|G G G G G|H H H H H ...
        // 0 1 2 3 4|0 1 2 3 4|0 1 2 3 4|0 1 2 3 4|0 1 2 3 4|0 1 2 3 4|0 1 2 3 4|0 1 2 3 4
        40000 => {
            for chunk in data.as_chunks_mut::<5>().0 {
                repack_g726(chunk, 5, is_pay);
            }
        }
        _ => unreachable!(),
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum G723Frame {
    HighRate,
    LowRate,
    Silence,
}

impl G723Frame {
    pub fn from_first_byte(byte: u8) -> Option<Self> {
        match byte & 0x03 {
            0x00 => Some(Self::HighRate),
            0x01 => Some(Self::LowRate),
            0x02 => Some(Self::Silence),
            _ => None,
        }
    }

    pub fn octets(self) -> usize {
        match self {
            Self::HighRate => 24,
            Self::LowRate => 20,
            Self::Silence => 4,
        }
    }
}

pub fn g723_bytes_to_frames(data: &[u8]) -> impl Iterator<Item = (usize, Option<G723Frame>)> + '_ {
    let mut idx = 0;
    core::iter::from_fn(move || {
        if idx >= data.len() {
            return None;
        }
        let on_parse_error = move |idx: &mut usize| {
            let ret_idx = *idx;
            *idx = data.len();
            Some((ret_idx, None))
        };
        let Some(frame) = G723Frame::from_first_byte(data[idx]) else {
            return on_parse_error(&mut idx);
        };
        if data.len() - idx < frame.octets() {
            return on_parse_error(&mut idx);
        }
        let ret_idx = idx;
        idx += frame.octets();
        Some((ret_idx, Some(frame)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A deliberately simple, per-bit reference used only as a differential
    // oracle for `repack_g726`. It reads each stream bit from the format
    // definition directly and places it at its significance, so any deviation
    // in the optimised window implementation surfaces as a mismatch.
    fn repack_ref(group: &mut [u8], width: usize, to_lsb: bool) {
        let num_codes = (group.len() * 8) / width;
        let mut scratch = [0u32; 8];
        for p in 0..group.len() * 8 {
            let (byte_i, rem) = (p / 8, p % 8);
            let msb0 = if to_lsb { rem } else { 7 - rem };
            let b = ((group[byte_i] >> (7 - msb0)) & 1) as u32;
            let (idx, j) = (p / width, p % width);
            let sig = if to_lsb { width - 1 - j } else { j };
            scratch[idx] |= b << sig;
        }
        let mut out = [0u8; 5];
        for (idx, &v) in scratch[..num_codes].iter().enumerate() {
            let base = idx * width;
            for s in 0..width {
                let b = (v >> s) & 1;
                if b == 0 {
                    continue;
                }
                let g = base + if to_lsb { s } else { width - 1 - s };
                let (byte_i, rem) = (g / 8, g % 8);
                let msb0 = if to_lsb { 7 - rem } else { rem };
                out[byte_i] |= (b as u8) << (7 - msb0);
            }
        }
        group.copy_from_slice(&out[..group.len()]);
    }

    #[test]
    fn g726_repack_diff_fuzz() {
        let mut state = 0x1234_5678u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        };
        for width in [3usize, 5] {
            for to_lsb in [true, false] {
                let check = |g: [u8; 5]| {
                    let mut a = g;
                    let mut b = g;
                    repack_g726(&mut a[..width], width, to_lsb);
                    repack_ref(&mut b[..width], width, to_lsb);
                    assert_eq!(
                        &a[..width],
                        &b[..width],
                        "width={width} to_lsb={to_lsb} in={g:?}"
                    );
                };
                // Edge cases first: constant and alternating patterns.
                for g in [
                    [0x00; 5],
                    [0xff; 5],
                    [0xaa; 5],
                    [0x55; 5],
                    [0x0f, 0xf0, 0x0f, 0xf0, 0x0f],
                    [0x01, 0x80, 0x01, 0x80, 0x01],
                ] {
                    check(g);
                }
                // Then a large batch of random groups.
                for _ in 0..100_000 {
                    let mut g = [0u8; 5];
                    for byte in g.iter_mut().take(width) {
                        *byte = next();
                    }
                    check(g);
                }
            }
        }
    }

    #[test]
    fn g726_code_swap_16000() {
        let bytes = [0x12, 0x34, 0x45, 0x56, 0x78, 0x9a, 0xab, 0xcd, 0xef];
        let mut data = bytes;
        g726_code_swap(&mut data, true, 16000);
        g726_code_swap(&mut data, false, 16000);
        assert_eq!(bytes, data);

        let bytes = bytes.map(|b| !b);
        let mut data = bytes;
        g726_code_swap(&mut data, true, 16000);
        g726_code_swap(&mut data, false, 16000);
        assert_eq!(bytes, data);
    }

    #[test]
    fn g726_code_swap_24000() {
        // Reference values for the RFC 3551 LSB-first wire format, computed
        // independently from the native (AAL2) MSB-first input.
        let input = [0x12, 0x34, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01];
        let rfc = [0x20, 0xa7, 0xa0, 0xcb, 0x41, 0x77, 0xde, 0x7c, 0x22];

        let mut data = input;
        g726_code_swap(&mut data, true, 24000);
        assert_eq!(rfc, data, "pay (native -> RFC)");

        let mut data = rfc;
        g726_code_swap(&mut data, false, 24000);
        assert_eq!(input, data, "depay (RFC -> native)");

        // Round-trip on another vector.
        let bytes = [0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0xa5, 0x5a, 0xaa];
        let mut data = bytes;
        g726_code_swap(&mut data, true, 24000);
        g726_code_swap(&mut data, false, 24000);
        assert_eq!(bytes, data);
    }

    #[test]
    fn g726_code_swap_32000() {
        let bytes = [0x12, 0x34, 0x45, 0x56, 0x78, 0x9a, 0xab, 0xcd, 0xef];
        let mut data = bytes;
        g726_code_swap(&mut data, true, 32000);
        g726_code_swap(&mut data, false, 32000);
        assert_eq!(bytes, data);

        let bytes = bytes.map(|b| !b);
        let mut data = bytes;
        g726_code_swap(&mut data, true, 32000);
        g726_code_swap(&mut data, false, 32000);
        assert_eq!(bytes, data);
    }

    #[test]
    fn g726_code_swap_40000() {
        // Reference values for the RFC 3551 LSB-first wire format, computed
        // independently from the native (AAL2) MSB-first input.
        let input = [0x12, 0x34, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23];
        let rfc = [0x02, 0x69, 0xa2, 0x32, 0x4f, 0xf5, 0x19, 0xef, 0x41, 0x1a];

        let mut data = input;
        g726_code_swap(&mut data, true, 40000);
        assert_eq!(rfc, data, "pay (native -> RFC)");

        let mut data = rfc;
        g726_code_swap(&mut data, false, 40000);
        assert_eq!(input, data, "depay (RFC -> native)");

        // Round-trip on another vector.
        let bytes = [0xa5, 0x5a, 0xaa, 0x55, 0xff, 0x10, 0x20, 0x30, 0x40, 0x50];
        let mut data = bytes;
        g726_code_swap(&mut data, true, 40000);
        g726_code_swap(&mut data, false, 40000);
        assert_eq!(bytes, data);
    }

    #[test]
    fn g723_frame_parse_header() {
        for i in 0..64 {
            assert!(matches!(
                G723Frame::from_first_byte(i * 4),
                Some(G723Frame::HighRate)
            ));
            assert!(matches!(
                G723Frame::from_first_byte(i * 4 + 1),
                Some(G723Frame::LowRate)
            ));
            assert!(matches!(
                G723Frame::from_first_byte(i * 4 + 2),
                Some(G723Frame::Silence)
            ));
            assert!(G723Frame::from_first_byte(i * 4 + 3).is_none());
        }
    }

    #[test]
    fn g723_parse_high_rate() {
        let data = [0x00; 24];
        let mut it = g723_bytes_to_frames(&data);
        assert_eq!(it.next(), Some((0, Some(G723Frame::HighRate))));
        assert_eq!(it.next(), None);

        // short read
        let mut it = g723_bytes_to_frames(&data[..23]);
        assert_eq!(it.next(), Some((0, None)));
        assert_eq!(it.next(), None);
    }

    #[test]
    fn g723_parse_low_rate() {
        let data = [0x01; 20];
        let mut it = g723_bytes_to_frames(&data);
        assert_eq!(it.next(), Some((0, Some(G723Frame::LowRate))));
        assert_eq!(it.next(), None);

        // short read
        let mut it = g723_bytes_to_frames(&data[..19]);
        assert_eq!(it.next(), Some((0, None)));
        assert_eq!(it.next(), None);
    }

    #[test]
    fn g723_parse_silence() {
        let data = [0x02; 4];
        let mut it = g723_bytes_to_frames(&data);
        assert_eq!(it.next(), Some((0, Some(G723Frame::Silence))));
        assert_eq!(it.next(), None);

        // short read
        let mut it = g723_bytes_to_frames(&data[..3]);
        assert_eq!(it.next(), Some((0, None)));
        assert_eq!(it.next(), None);
    }

    #[test]
    fn g723_parse_multiple() {
        let mut data = [0x00; 24 + 20 + 4];
        data[24] = 0x01;
        data[44] = 0x02;
        let mut it = g723_bytes_to_frames(&data);
        assert_eq!(it.next(), Some((0, Some(G723Frame::HighRate))));
        assert_eq!(it.next(), Some((24, Some(G723Frame::LowRate))));
        assert_eq!(it.next(), Some((44, Some(G723Frame::Silence))));
        assert_eq!(it.next(), None);
    }

    #[test]
    fn g723_parse_failure_ends_iter() {
        let mut data = [0x00; 24 + 20 + 4];
        data[24] = 0x03;
        data[25..].fill(0x02);
        let mut it = g723_bytes_to_frames(&data);
        assert_eq!(it.next(), Some((0, Some(G723Frame::HighRate))));
        assert_eq!(it.next(), Some((24, None)));
        assert_eq!(it.next(), None);
    }
}
