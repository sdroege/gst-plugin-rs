// Copyright (C) 2019,2020 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2019 Jordan Petridis <jordan@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use crate::parser_utils::{end_of_line, timecode, TimeCode};
use nom::IResult;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SccLine {
    Header,
    Empty,
    Caption(TimeCode, Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Header,
    Empty,
    CaptionOrEmpty,
}

#[derive(Debug)]
pub struct SccParser {
    state: State,
}

/// Parser for the SCC header
fn header(s: &[u8]) -> IResult<&[u8], SccLine> {
    use nom::bytes::complete::tag;
    use nom::combinator::{map, opt};
    use nom::error::context;
    use nom::sequence::tuple;

    context(
        "invalid header",
        map(
            tuple((
                opt(tag(&[0xEFu8, 0xBBu8, 0xBFu8][..])),
                tag("Scenarist_SCC V1.0"),
                end_of_line,
            )),
            |_| SccLine::Header,
        ),
    )(s)
}

/// Parser that accepts only an empty line
fn empty_line(s: &[u8]) -> IResult<&[u8], SccLine> {
    use nom::combinator::map;
    use nom::error::context;

    context("invalid empty line", map(end_of_line, |_| SccLine::Empty))(s)
}

/// A single SCC payload item. This is ASCII hex encoded bytes.
/// It returns an tuple of `(u8, u8)` of the hex encoded bytes.
fn scc_payload_item(s: &[u8]) -> IResult<&[u8], (u8, u8)> {
    use nom::bytes::complete::take_while_m_n;
    use nom::character::is_hex_digit;
    use nom::combinator::map;
    use nom::error::context;

    context(
        "invalid SCC payload item",
        map(take_while_m_n(4, 4, is_hex_digit), |s: &[u8]| {
            let hex_to_u8 = |v: u8| match v {
                v if v >= b'0' && v <= b'9' => v - b'0',
                v if v >= b'A' && v <= b'F' => 10 + v - b'A',
                v if v >= b'a' && v <= b'f' => 10 + v - b'a',
                _ => unreachable!(),
            };

            let val1 = (hex_to_u8(s[0]) << 4) | hex_to_u8(s[1]);
            let val2 = (hex_to_u8(s[2]) << 4) | hex_to_u8(s[3]);

            (val1, val2)
        }),
    )(s)
}

/// Parser for the whole SCC payload with conversion to the underlying byte values.
fn scc_payload(s: &[u8]) -> IResult<&[u8], Vec<u8>> {
    use nom::branch::alt;
    use nom::bytes::complete::tag;
    use nom::combinator::map;
    use nom::error::context;
    use nom::multi::fold_many1;
    use nom::sequence::pair;
    use nom::Parser;

    let parse_item = map(
        pair(scc_payload_item, alt((tag(" ").map(|_| ()), end_of_line))),
        |(item, _)| item,
    );

    context(
        "invalid SCC payload",
        fold_many1(parse_item, Vec::new(), |mut acc: Vec<_>, item| {
            acc.push(item.0);
            acc.push(item.1);
            acc
        }),
    )(s)
}

/// Parser for a SCC caption line in the form `timecode\tpayload`.
fn caption(s: &[u8]) -> IResult<&[u8], SccLine> {
    use nom::bytes::complete::tag;
    use nom::combinator::map;
    use nom::error::context;
    use nom::sequence::tuple;

    context(
        "invalid SCC caption line",
        map(
            tuple((timecode, tag("\t"), scc_payload, end_of_line)),
            |(tc, _, value, _)| SccLine::Caption(tc, value),
        ),
    )(s)
}

/// SCC parser the parses line-by-line and keeps track of the current state in the file.
impl SccParser {
    pub fn new() -> Self {
        Self {
            state: State::Header,
        }
    }

    pub fn new_scan_captions() -> Self {
        Self {
            state: State::CaptionOrEmpty,
        }
    }

    pub fn reset(&mut self) {
        self.state = State::Header;
    }

    pub fn parse_line<'a>(
        &mut self,
        line: &'a [u8],
    ) -> Result<SccLine, nom::error::Error<&'a [u8]>> {
        use nom::branch::alt;

        match self.state {
            State::Header => header(line)
                .map(|v| {
                    self.state = State::Empty;
                    v.1
                })
                .map_err(|err| match err {
                    nom::Err::Incomplete(_) => unreachable!(),
                    nom::Err::Error(e) | nom::Err::Failure(e) => e,
                }),
            State::Empty => empty_line(line)
                .map(|v| {
                    self.state = State::CaptionOrEmpty;
                    v.1
                })
                .map_err(|err| match err {
                    nom::Err::Incomplete(_) => unreachable!(),
                    nom::Err::Error(e) | nom::Err::Failure(e) => e,
                }),
            State::CaptionOrEmpty => alt((caption, empty_line))(line)
                .map(|v| v.1)
                .map_err(|err| match err {
                    nom::Err::Incomplete(_) => unreachable!(),
                    nom::Err::Error(e) | nom::Err::Failure(e) => e,
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header() {
        assert_eq!(
            header(b"Scenarist_SCC V1.0".as_ref()),
            Ok((b"".as_ref(), SccLine::Header,))
        );

        assert_eq!(
            header(b"Scenarist_SCC V1.0\n".as_ref()),
            Ok((b"".as_ref(), SccLine::Header))
        );

        assert_eq!(
            header(b"Scenarist_SCC V1.0\r\n".as_ref()),
            Ok((b"".as_ref(), SccLine::Header))
        );

        assert_eq!(
            header(b"Scenarist_SCC V1.1".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b"Scenarist_SCC V1.1".as_ref(),
                nom::error::ErrorKind::Tag
            ))),
        );
    }

    #[test]
    fn test_empty_line() {
        assert_eq!(empty_line(b"".as_ref()), Ok((b"".as_ref(), SccLine::Empty)));

        assert_eq!(
            empty_line(b"\n".as_ref()),
            Ok((b"".as_ref(), SccLine::Empty))
        );

        assert_eq!(
            empty_line(b"\r\n".as_ref()),
            Ok((b"".as_ref(), SccLine::Empty))
        );

        assert_eq!(
            empty_line(b" \r\n".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b" \r\n".as_ref(),
                nom::error::ErrorKind::Eof
            ))),
        );
    }

    #[test]
    fn test_caption() {
        assert_eq!(
            caption(b"01:02:53:14\t94ae 94ae 9420 9420 947a 947a 97a2 97a2 a820 68ef f26e 2068 ef6e 6be9 6e67 2029 942c 942c 8080 8080 942f 942f".as_ref()),
            Ok((
                b"".as_ref(),
                SccLine::Caption(
                    TimeCode {
                        hours: 1,
                        minutes: 2,
                        seconds: 53,
                        frames: 14,
                        drop_frame: false
                    },

                    vec![
                        0x94, 0xae, 0x94, 0xae, 0x94, 0x20, 0x94, 0x20, 0x94, 0x7a, 0x94, 0x7a, 0x97, 0xa2,
                        0x97, 0xa2, 0xa8, 0x20, 0x68, 0xef, 0xf2, 0x6e, 0x20, 0x68, 0xef, 0x6e, 0x6b, 0xe9,
                        0x6e, 0x67, 0x20, 0x29, 0x94, 0x2c, 0x94, 0x2c, 0x80, 0x80, 0x80, 0x80, 0x94, 0x2f,
                        0x94, 0x2f,
                    ]
                ),
            ))
        );

        assert_eq!(
            caption(b"01:02:55;14	942c 942c".as_ref()),
            Ok((
                b"".as_ref(),
                SccLine::Caption(
                    TimeCode {
                        hours: 1,
                        minutes: 2,
                        seconds: 55,
                        frames: 14,
                        drop_frame: true
                    },
                    vec![0x94, 0x2c, 0x94, 0x2c]
                ),
            ))
        );

        assert_eq!(
            caption(b"01:03:27:29	94ae 94ae 9420 9420 94f2 94f2 c845 d92c 2054 c845 5245 ae80 942c 942c 8080 8080 942f 942f".as_ref()),
            Ok((
                b"".as_ref(),
                SccLine::Caption(
                    TimeCode {
                        hours: 1,
                        minutes: 3,
                        seconds: 27,
                        frames: 29,
                        drop_frame: false
                    },
                    vec![
                        0x94, 0xae, 0x94, 0xae, 0x94, 0x20, 0x94, 0x20, 0x94, 0xf2, 0x94, 0xf2, 0xc8, 0x45,
                        0xd9, 0x2c, 0x20, 0x54, 0xc8, 0x45, 0x52, 0x45, 0xae, 0x80, 0x94, 0x2c, 0x94, 0x2c,
                        0x80, 0x80, 0x80, 0x80, 0x94, 0x2f, 0x94, 0x2f,
                    ]
                ),
            ))
        );

        assert_eq!(
            caption(b"00:00:00;00\t942c 942c".as_ref()),
            Ok((
                b"".as_ref(),
                SccLine::Caption(
                    TimeCode {
                        hours: 0,
                        minutes: 0,
                        seconds: 00,
                        frames: 00,
                        drop_frame: true,
                    },
                    vec![0x94, 0x2c, 0x94, 0x2c],
                ),
            ))
        );

        assert_eq!(
            caption(b"00:00:00;00\t942c 942c\r\n".as_ref()),
            Ok((
                b"".as_ref(),
                SccLine::Caption(
                    TimeCode {
                        hours: 0,
                        minutes: 0,
                        seconds: 00,
                        frames: 00,
                        drop_frame: true,
                    },
                    vec![0x94, 0x2c, 0x94, 0x2c],
                ),
            ))
        );
    }

    #[test]
    fn test_parser() {
        let scc_file = include_bytes!("../../tests/dn2018-1217.scc");
        let mut reader = crate::line_reader::LineReader::new();
        let mut parser = SccParser::new();
        let mut line_cnt = 0;

        reader.push(Vec::from(scc_file.as_ref()));

        while let Some(line) = reader.line() {
            let res = match parser.parse_line(line) {
                Ok(res) => res,
                Err(err) => panic!("Couldn't parse line {}: {:?}", line_cnt, err),
            };

            match line_cnt {
                0 => assert_eq!(res, SccLine::Header),
                x if x % 2 != 0 => assert_eq!(res, SccLine::Empty),
                _ => match res {
                    SccLine::Caption(_, _) => (),
                    res => panic!("Expected caption at line {}, got {:?}", line_cnt, res),
                },
            }

            line_cnt += 1;
        }
    }
}
