// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use nom::IResult;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeCode {
    pub hours: u32,
    pub minutes: u32,
    pub seconds: u32,
    pub frames: u32,
    pub drop_frame: bool,
}

/// Parser for parsing a run of ASCII, decimal digits and converting them into a `u32`
pub fn digits(s: &[u8]) -> IResult<&[u8], u32> {
    use nom::bytes::complete::take_while;
    use nom::character::is_digit;
    use nom::combinator::map_res;

    map_res(
        map_res(take_while(is_digit), std::str::from_utf8),
        |s: &str| s.parse::<u32>(),
    )(s)
}

/// Parser for a run of decimal digits, that converts them into a `u32` and checks if the result is
/// in the allowed range.
pub fn digits_range<R: std::ops::RangeBounds<u32>>(
    range: R,
) -> impl FnMut(&[u8]) -> IResult<&[u8], u32> {
    use nom::combinator::verify;
    use nom::error::context;

    move |s: &[u8]| context("digits out of range", verify(digits, |v| range.contains(v)))(s)
}

/// Parser for a timecode in the form `hh:mm:ss:fs`
pub fn timecode(s: &[u8]) -> IResult<&[u8], TimeCode> {
    use nom::character::complete::{char, one_of};
    use nom::combinator::map;
    use nom::error::context;
    use nom::sequence::tuple;

    context(
        "invalid timecode",
        map(
            tuple((
                digits,
                char(':'),
                digits_range(0..60),
                char(':'),
                digits_range(0..60),
                one_of(":.;,"),
                digits,
            )),
            |(hours, _, minutes, _, seconds, sep, frames)| TimeCode {
                hours,
                minutes,
                seconds,
                frames,
                drop_frame: sep == ';' || sep == ',',
            },
        ),
    )(s)
}

/// Parser that checks for EOF and optionally `\n` or `\r\n` before EOF
pub fn end_of_line(s: &[u8]) -> IResult<&[u8], ()> {
    use nom::branch::alt;
    use nom::bytes::complete::tag;
    use nom::combinator::{eof, map, opt};
    use nom::sequence::pair;

    map(pair(opt(alt((tag("\r\n"), tag("\n")))), eof), |_| ())(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timecode() {
        assert_eq!(
            timecode(b"11:12:13;14".as_ref()),
            Ok((
                b"".as_ref(),
                TimeCode {
                    hours: 11,
                    minutes: 12,
                    seconds: 13,
                    frames: 14,
                    drop_frame: true
                },
            ))
        );

        assert_eq!(
            timecode(b"11:12:13:14".as_ref()),
            Ok((
                b"".as_ref(),
                TimeCode {
                    hours: 11,
                    minutes: 12,
                    seconds: 13,
                    frames: 14,
                    drop_frame: false
                },
            ))
        );

        assert_eq!(
            timecode(b"11:12:13:14abcd".as_ref()),
            Ok((
                b"abcd".as_ref(),
                TimeCode {
                    hours: 11,
                    minutes: 12,
                    seconds: 13,
                    frames: 14,
                    drop_frame: false
                },
            ))
        );

        assert_eq!(
            timecode(b"abcd11:12:13:14".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b"abcd11:12:13:14".as_ref(),
                nom::error::ErrorKind::MapRes
            ))),
        );
    }
}
