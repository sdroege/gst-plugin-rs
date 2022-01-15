// Copyright (C) 2020 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use byteorder::{BigEndian, ByteOrder};
use std::fmt;

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy)]
pub enum ParseErrorCode {
    WrongLength,
    WrongMagicSequence,
    WrongLayout,
}

#[derive(Debug, Clone)]
pub struct ParseError {
    pub code: ParseErrorCode,
    pub byte: usize,
    pub msg: String,
}

pub fn extract_cdp(mut data: &[u8]) -> Result<&[u8], ParseError> {
    /* logic from ccconverter */
    let data_len = data.len();

    if data.len() < 11 {
        return Err(ParseError {
            code: ParseErrorCode::WrongLength,
            byte: data_len - data.len(),
            msg: format!(
                "cdp packet too short {}. expected at least {}",
                data.len(),
                11
            ),
        });
    }

    if 0x9669 != BigEndian::read_u16(&data[..2]) {
        return Err(ParseError {
            code: ParseErrorCode::WrongMagicSequence,
            byte: data_len - data.len(),
            msg: String::from("cdp packet does not have initial magic bytes of 0x9669"),
        });
    }
    data = &data[2..];

    if (data[0] as usize) != data_len {
        return Err(ParseError {
            code: ParseErrorCode::WrongLength,
            byte: data_len - data.len(),
            msg: format!(
                "advertised cdp packet length {} does not match length of data {}",
                data[0], data_len
            ),
        });
    }
    data = &data[1..];

    /* skip framerate value */
    data = &data[1..];

    let flags = data[0];
    data = &data[1..];

    if flags & 0x40 == 0 {
        /* no cc_data */
        return Ok(&[]);
    }

    /* skip sequence counter */
    data = &data[2..];

    /* timecode present? */
    if flags & 0x80 == 0x80 {
        if data.len() < 5 {
            return Err(ParseError {
                code: ParseErrorCode::WrongLength,
                byte: data_len - data.len(),
                msg: String::from(
                    "cdp packet signals a timecode but is not large enough to contain a timecode",
                ),
            });
        }
        data = &data[5..];
    }

    /* cc_data */
    if data.len() < 2 {
        return Err(ParseError {
            code: ParseErrorCode::WrongLength,
            byte: data_len - data.len(),
            msg: String::from(
                "cdp packet signals cc_data but is not large enough to contain cc_data",
            ),
        });
    }

    if data[0] != 0x72 {
        return Err(ParseError {
            code: ParseErrorCode::WrongMagicSequence,
            byte: data_len - data.len(),
            msg: String::from("ccp is missing start code 0x72"),
        });
    }
    data = &data[1..];

    let cc_count = data[0];
    data = &data[1..];
    if cc_count & 0xe0 != 0xe0 {
        return Err(ParseError {
            code: ParseErrorCode::WrongMagicSequence,
            byte: data_len - data.len(),
            msg: format!("reserved bits are not 0xe0, found {:02x}", cc_count & 0xe0),
        });
    }
    let cc_count = cc_count & 0x1f;
    let len = 3 * cc_count as usize;

    if len > data.len() {
        return Err(ParseError {
            code: ParseErrorCode::WrongLength,
            byte: data_len - data.len(),
            msg: String::from("cc_data length extends past the end of the cdp packet"),
        });
    }

    /* TODO: validate checksum */

    Ok(&data[..len])
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?} at byte {}: {}", self.code, self.byte, self.msg)
    }
}

impl std::error::Error for ParseError {}
