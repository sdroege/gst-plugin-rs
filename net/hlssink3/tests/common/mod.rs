// Copyright (C) 2025 Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ByteRange {
    length: u64,
    offset: u64,
}

pub fn get_byte_ranges(s: &gst::StructureRef) -> Vec<ByteRange> {
    let mut ranges = Vec::new();

    if let Ok(br) = s.get::<gst::Structure>("initialization-segment-byte-range") {
        let length = br.get::<u64>("length").unwrap();
        let offset = br.get::<u64>("offset").unwrap();
        assert!(length != 0);
        ranges.push(ByteRange { length, offset })
    }

    if let Ok(br) = s.get::<gst::Structure>("segment-byte-range") {
        let length = br.get::<u64>("length").unwrap();
        let offset = br.get::<u64>("offset").unwrap();
        assert!(length != 0);
        ranges.push(ByteRange { length, offset })
    }

    ranges
}

pub fn validate_byterange_sequence(ranges: &[ByteRange]) -> bool {
    if ranges.is_empty() {
        return false; // Empty sequence is invalid
    }

    ranges.windows(2).all(|pair| {
        let current = &pair[0];
        let next = &pair[1];
        next.offset == current.offset + current.length
    })
}
