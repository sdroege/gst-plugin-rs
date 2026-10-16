//
// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! H.263 start code scanning.
//!
//! All H.263+ start codes (picture, GOB, slice, EOS, EOSBS) consist of 16
//! zero bits followed by a '1' bit (RFC 4629 section 3.2, H.263 5.1.1,
//! 5.1.27, 5.2.2, Annexes C.4.1 and K.2.2). When byte aligned they therefore
//! appear as the byte pattern `00 00 8x`. H.263 has no emulation prevention
//! mechanism, so the pattern can also occur inside macroblock data; see
//! segment_offsets.

/// Find the next start code `00 00 8x` at or after `from`.
///
/// Returns the byte offset of the first `0x00` of the start code.
fn find_startcode(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(3)
        .enumerate()
        .find(|(_, w)| w[0] == 0 && w[1] == 0 && w[2] & 0x80 != 0)
        .map(|(i, _)| from + i)
}

/// True if `data` starts with a picture start code (`00 00 8x`).
pub fn starts_with_startcode(data: &[u8]) -> bool {
    matches!(data, [0, 0, x, ..] if x & 0x80 != 0)
}

/// True if the start code at `offset` is an EOS or EOSBS code.
///
/// Start codes here are byte aligned. EOS is group number 31
/// (`1 11111`, third byte 0xFC..0xFF) and EOSBS group number 30
/// (`1 11110 0`, third byte 0xF8/0xF9) per H.263 5.2.3, 5.1.27 and
/// Annex C.4.1, so the EOS/EOSBS class is third byte >= 0xF8. Slice codes
/// with certain MBA bit patterns share this range; treating them as
/// sequence endings only isolates a packet, the stream stays byte-exact.
pub fn is_sequence_end(data: &[u8], offset: usize) -> bool {
    data.get(offset + 2).is_some_and(|b| *b >= 0xF8)
}

/// Split `data` into segments that each start with a start code.
///
/// `data` must start with a start code. The returned offsets are the start
/// positions of each segment; the last segment ends at `data.len()`.
///
/// Every byte-aligned `00 00 8x` pattern is a valid start code prefix
/// (picture, GOB group numbers 1..=29, slice and EOS/EOSBS all overlap in
/// that range), so no further classification is possible at byte level;
/// only bit-level parsing could reject a false pattern from macroblock
/// data. Such splits stay byte-exact through a matching depayer (the P-bit
/// zero-byte omission is symmetric).
///
/// The scan for the next start code starts at offset 3: the first start code
/// occupies bytes 0..=2 and a start code cannot start at offset 1 or 2 because
/// byte 2 of the first start code is non-zero.
pub fn segment_offsets(data: &[u8]) -> Vec<usize> {
    assert!(starts_with_startcode(data));

    let mut offsets = vec![0];
    let mut pos = 3;
    while let Some(offset) = find_startcode(data, pos) {
        offsets.push(offset);
        pos = offset + 3;
    }

    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_startcode_basic() {
        let data = [0x00, 0x00, 0x80, 0x21, 0xFF, 0x00, 0x00, 0x80, 0xA1];
        assert_eq!(find_startcode(&data, 0), Some(0));
        assert_eq!(find_startcode(&data, 1), Some(5));
        assert_eq!(find_startcode(&data, 6), None);
        assert_eq!(find_startcode(&data, 100), None);
    }

    #[test]
    fn find_startcode_rejects_non_startcodes() {
        // 0x80 in the third byte is required
        let data = [0x00, 0x00, 0x7F, 0x00];
        assert_eq!(find_startcode(&data, 0), None);
        // A single zero byte is not enough
        let data = [0x00, 0x11, 0x80, 0x00];
        assert_eq!(find_startcode(&data, 0), None);
    }

    #[test]
    fn is_sequence_end_detects_eos_and_eosbs() {
        // EOS (group number 31, third byte 0xFC..0xFF) and EOSBS (group
        // number 30, 0xF8/0xF9) are sequence endings.
        assert!(is_sequence_end(&[0x00, 0x00, 0xFC, 0x00], 0));
        assert!(is_sequence_end(&[0x00, 0x00, 0xFE, 0x00], 0));
        assert!(is_sequence_end(&[0x00, 0x00, 0xF8, 0x00], 0));
        // Picture (0x80..0x83), GOB GN 28 (0xB8) and slice (0xC0..) are not.
        assert!(!is_sequence_end(&[0x00, 0x00, 0x80, 0x00], 0));
        assert!(!is_sequence_end(&[0x00, 0x00, 0xB8, 0x00], 0));
        assert!(!is_sequence_end(&[0x00, 0x00, 0xC0, 0x00], 0));
    }

    #[test]
    fn segment_offsets_splits_at_every_startcode() {
        let mut data = vec![0x00, 0x00, 0x80, 0x21];
        data.extend_from_slice(&[0x11, 0x22]);
        data.extend_from_slice(&[0x00, 0x00, 0x80, 0xA1]);
        data.extend_from_slice(&[0x33]);
        data.extend_from_slice(&[0x00, 0x00, 0x80, 0x00]);

        assert_eq!(segment_offsets(&data), vec![0, 6, 11]);
    }
}
