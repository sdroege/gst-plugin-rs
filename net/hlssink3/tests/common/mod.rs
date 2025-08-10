// Copyright (C) 2025 Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#[derive(Debug, PartialEq)]
pub struct ByteRange {
    length: u64,
    offset: u64,
}

#[allow(dead_code)]
pub fn extract_map_byterange(input: &str) -> Option<ByteRange> {
    input
        .lines()
        .find(|line| line.starts_with("#EXT-X-MAP:"))
        .and_then(|line| {
            line.find("BYTERANGE=\"")
                .and_then(|start| {
                    let content_start = start + "BYTERANGE=\"".len();
                    line[content_start..]
                        .find('"')
                        .map(|end| &line[content_start..content_start + end])
                })
                .and_then(|byterange_str| {
                    let mut parts = byterange_str.split('@');
                    match (parts.next(), parts.next()) {
                        (Some(len_str), Some(off_str)) => {
                            let length = len_str.parse::<u64>().ok()?;
                            let offset = off_str.parse::<u64>().ok()?;
                            Some(ByteRange { length, offset })
                        }
                        _ => None,
                    }
                })
        })
}

pub fn extract_byteranges(input: &str) -> Vec<ByteRange> {
    input
        .split('\n')
        .filter(|line| line.starts_with("#EXT-X-BYTERANGE:"))
        .filter_map(|line| {
            line.strip_prefix("#EXT-X-BYTERANGE:").and_then(|content| {
                let mut parts = content.split('@');
                match (parts.next(), parts.next()) {
                    (Some(len_str), Some(off_str)) => {
                        let length = len_str.parse::<u64>().ok()?;
                        let offset = off_str.parse::<u64>().ok()?;
                        Some(ByteRange { length, offset })
                    }
                    _ => None,
                }
            })
        })
        .collect()
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
