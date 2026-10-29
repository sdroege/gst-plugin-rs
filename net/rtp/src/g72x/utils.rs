//
// Copyright (C) 2026 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

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
            for bytes in data.as_chunks_mut::<3>().0 {
                // XXX: This does not match the above bit fiddling but does match rtpg726pay/depay
                if is_pay {
                    let tmp = bytes[0];
                    bytes[0] = (tmp & 0xc0) >> 6 | (tmp & 0x38) >> 1 | (tmp & 0x07) << 5;
                    let tmp = bytes[1];
                    bytes[1] = (tmp & 0x80) >> 7
                        | (tmp & 0x70) >> 3
                        | (tmp & 0x0e) << 3
                        | (tmp & 0x01) << 7;
                    let tmp = bytes[2];
                    bytes[2] = (tmp & 0xe0) >> 5 | (tmp & 0x1c) << 1 | (tmp & 0x03) << 6;
                } else {
                    let tmp = bytes[0];
                    bytes[0] = (tmp & 0xe0) >> 5 | (tmp & 0x1c) << 1 | (tmp & 0x03) << 6;
                    let tmp = bytes[1];
                    bytes[1] = (tmp & 0x80) >> 7
                        | (tmp & 0x70) >> 3
                        | (tmp & 0x0e) << 3
                        | (tmp & 0x01) << 7;
                    let tmp = bytes[2];
                    bytes[2] = (tmp & 0xc0) >> 6 | (tmp & 0x38) >> 1 | (tmp & 0x07) << 5;
                }
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
            for bytes in data.as_chunks_mut::<5>().0 {
                // XXX: This does not match the above bit fiddling but does match rtpg726pay/depay
                if is_pay {
                    let tmp = bytes[0];
                    bytes[0] = (tmp & 0xe0) >> 5 | (tmp & 0x1f) << 3;
                    let tmp = bytes[1];
                    bytes[1] = (tmp & 0x80) >> 7 | (tmp & 0x7c) >> 1 | (tmp & 0x03) << 6;
                    let tmp = bytes[2];
                    bytes[2] = (tmp & 0xf0) >> 4 | (tmp & 0x0f) << 4;
                    let tmp = bytes[3];
                    bytes[3] = (tmp & 0xc0) >> 6 | (tmp & 0x3e) << 1 | (tmp & 0x01) << 7;
                    let tmp = bytes[4];
                    bytes[4] = (tmp & 0xf8) >> 3 | (tmp & 0x07) << 5;
                } else {
                    let tmp = bytes[0];
                    bytes[0] = (tmp & 0xf8) >> 3 | (tmp & 0x07) << 5;
                    let tmp = bytes[1];
                    bytes[1] = (tmp & 0xc0) >> 6 | (tmp & 0x3e) << 1 | (tmp & 0x01) << 7;
                    let tmp = bytes[2];
                    bytes[2] = (tmp & 0xf0) >> 4 | (tmp & 0x0f) << 4;
                    let tmp = bytes[3];
                    bytes[3] = (tmp & 0x80) >> 7 | (tmp & 0x7c) >> 1 | (tmp & 0x03) << 6;
                    let tmp = bytes[4];
                    bytes[4] = (tmp & 0xe0) >> 5 | (tmp & 0x1f) << 3;
                }
            }
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let bytes = [0x12, 0x34, 0x45, 0x56, 0x78, 0x9a, 0xab, 0xcd, 0xef];
        let mut data = bytes;
        g726_code_swap(&mut data, true, 24000);
        g726_code_swap(&mut data, false, 24000);
        assert_eq!(bytes, data);

        let bytes = bytes.map(|b| !b);
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
        let bytes = [0x12, 0x34, 0x45, 0x56, 0x78, 0x9a, 0xab, 0xcd, 0xef];
        let mut data = bytes;
        g726_code_swap(&mut data, true, 40000);
        g726_code_swap(&mut data, false, 40000);
        assert_eq!(bytes, data);

        let bytes = bytes.map(|b| !b);
        let mut data = bytes;
        g726_code_swap(&mut data, true, 40000);
        g726_code_swap(&mut data, false, 40000);
        assert_eq!(bytes, data);
    }
}
