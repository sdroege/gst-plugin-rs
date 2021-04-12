// Copyright (C) 2018,2020 Sebastian Dröge <sebastian@centricular.com>
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

use either::Either;

use crate::parser_utils::{digits, digits_range, end_of_line, timecode, TimeCode};
use nom::IResult;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MccLine<'a> {
    Header,
    Comment,
    Empty,
    UUID(&'a [u8]),
    Metadata(&'a [u8], &'a [u8]),
    TimeCodeRate(u8, bool),
    Caption(TimeCode, Option<Vec<u8>>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Header,
    EmptyAfterHeader,
    Comments,
    Metadata,
    Captions,
}

#[derive(Debug)]
pub struct MccParser {
    state: State,
}

/// Parser for the MCC header
fn header(s: &[u8]) -> IResult<&[u8], MccLine> {
    use nom::branch::alt;
    use nom::bytes::complete::tag;
    use nom::combinator::{map, opt};
    use nom::error::context;
    use nom::sequence::tuple;

    context(
        "invalid header",
        map(
            tuple((
                opt(tag(&[0xEFu8, 0xBBu8, 0xBFu8][..])),
                tag("File Format=MacCaption_MCC V"),
                alt((tag("1.0"), tag("2.0"))),
                end_of_line,
            )),
            |_| MccLine::Header,
        ),
    )(s)
}

/// Parser for an MCC comment, i.e. a line starting with `//`. We don't return the actual comment
/// text as it's irrelevant for us.
fn comment(s: &[u8]) -> IResult<&[u8], MccLine> {
    use nom::bytes::complete::tag;
    use nom::combinator::{map, rest};
    use nom::error::context;
    use nom::sequence::tuple;

    context(
        "invalid comment",
        map(tuple((tag("//"), rest)), |_| MccLine::Comment),
    )(s)
}

/// Parser for the MCC UUID line.
fn uuid(s: &[u8]) -> IResult<&[u8], MccLine> {
    use nom::bytes::complete::{tag, take_while1};
    use nom::combinator::map;
    use nom::error::context;
    use nom::sequence::tuple;

    context(
        "invalid uuid",
        map(
            tuple((
                tag("UUID="),
                take_while1(|b| b != b'\n' && b != b'\r'),
                end_of_line,
            )),
            |(_, uuid, _)| MccLine::UUID(uuid),
        ),
    )(s)
}

/// Parser for the MCC Time Code Rate line.
fn time_code_rate(s: &[u8]) -> IResult<&[u8], MccLine> {
    use nom::bytes::complete::tag;
    use nom::combinator::{map, opt};
    use nom::error::context;
    use nom::sequence::tuple;

    context(
        "invalid timecode rate",
        map(
            tuple((
                tag("Time Code Rate="),
                digits_range(1..256),
                opt(tag("DF")),
                end_of_line,
            )),
            |(_, v, df, _)| MccLine::TimeCodeRate(v as u8, df.is_some()),
        ),
    )(s)
}

/// Parser for generic MCC metadata lines in the form `key=value`.
fn metadata(s: &[u8]) -> IResult<&[u8], MccLine> {
    use nom::bytes::complete::take_while1;
    use nom::character::complete::char;
    use nom::combinator::map;
    use nom::error::context;
    use nom::sequence::tuple;

    context(
        "invalid metadata",
        map(
            tuple((
                take_while1(|b| b != b'='),
                char('='),
                take_while1(|b| b != b'\n' && b != b'\r'),
                end_of_line,
            )),
            |(name, _, value, _)| MccLine::Metadata(name, value),
        ),
    )(s)
}

/// Parser that accepts only an empty line
fn empty_line(s: &[u8]) -> IResult<&[u8], MccLine> {
    use nom::combinator::map;
    use nom::error::context;

    context("invalid empty line", map(end_of_line, |_| MccLine::Empty))(s)
}

/// A single MCC payload item. This is ASCII hex encoded bytes plus some single-character
/// short-cuts for common byte sequences.
///
/// It returns an `Either` of the single hex encoded byte or the short-cut byte sequence as a
/// static byte slice.
fn mcc_payload_item(s: &[u8]) -> IResult<&[u8], Either<u8, &'static [u8]>> {
    use nom::branch::alt;
    use nom::bytes::complete::tag;
    use nom::bytes::complete::take_while_m_n;
    use nom::character::is_hex_digit;
    use nom::combinator::map;
    use nom::error::context;

    context(
        "invalid payload item",
        alt((
            map(tag("G"), |_| Either::Right([0xfa, 0x00, 0x00].as_ref())),
            map(tag("H"), |_| {
                Either::Right([0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00].as_ref())
            }),
            map(tag("I"), |_| {
                Either::Right([0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00].as_ref())
            }),
            map(tag("J"), |_| {
                Either::Right(
                    [
                        0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                    ]
                    .as_ref(),
                )
            }),
            map(tag("K"), |_| {
                Either::Right(
                    [
                        0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                        0xfa, 0x00, 0x00,
                    ]
                    .as_ref(),
                )
            }),
            map(tag("L"), |_| {
                Either::Right(
                    [
                        0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                        0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                    ]
                    .as_ref(),
                )
            }),
            map(tag("M"), |_| {
                Either::Right(
                    [
                        0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                        0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                    ]
                    .as_ref(),
                )
            }),
            map(tag("N"), |_| {
                Either::Right(
                    [
                        0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                        0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                    ]
                    .as_ref(),
                )
            }),
            map(tag("O"), |_| {
                Either::Right(
                    [
                        0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                        0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                        0xfa, 0x00, 0x00,
                    ]
                    .as_ref(),
                )
            }),
            map(tag("P"), |_| Either::Right([0xfb, 0x80, 0x80].as_ref())),
            map(tag("Q"), |_| Either::Right([0xfc, 0x80, 0x80].as_ref())),
            map(tag("R"), |_| Either::Right([0xfd, 0x80, 0x80].as_ref())),
            map(tag("S"), |_| Either::Right([0x96, 0x69].as_ref())),
            map(tag("T"), |_| Either::Right([0x61, 0x01].as_ref())),
            map(tag("U"), |_| Either::Right([0xe1, 0x00, 0x00].as_ref())),
            map(tag("Z"), |_| Either::Right([0x00].as_ref())),
            map(take_while_m_n(2, 2, is_hex_digit), |s: &[u8]| {
                let hex_to_u8 = |v: u8| match v {
                    v if v >= b'0' && v <= b'9' => v - b'0',
                    v if v >= b'A' && v <= b'F' => 10 + v - b'A',
                    v if v >= b'a' && v <= b'f' => 10 + v - b'a',
                    _ => unreachable!(),
                };
                let val = (hex_to_u8(s[0]) << 4) | hex_to_u8(s[1]);
                Either::Left(val)
            }),
        )),
    )(s)
}

/// Parser for the whole MCC payload with conversion to the underlying byte values.
fn mcc_payload(s: &[u8]) -> IResult<&[u8], Vec<u8>> {
    use nom::error::context;
    use nom::multi::fold_many1;

    context(
        "invalid MCC payload",
        fold_many1(mcc_payload_item, Vec::new(), |mut acc: Vec<_>, item| {
            match item {
                Either::Left(val) => acc.push(val),
                Either::Right(vals) => acc.extend_from_slice(vals),
            }
            acc
        }),
    )(s)
}

/// Parser for a MCC caption line in the form `timecode\tpayload`.
fn caption(parse_payload: bool) -> impl FnMut(&[u8]) -> IResult<&[u8], MccLine> {
    use nom::bytes::complete::take_while;
    use nom::character::complete::{char, one_of};
    use nom::combinator::{map, opt};
    use nom::error::context;
    use nom::sequence::tuple;

    fn parse(parse_payload: bool) -> impl FnMut(&[u8]) -> IResult<&[u8], Option<Vec<u8>>> {
        move |s| {
            if parse_payload {
                map(mcc_payload, Some)(s)
            } else {
                map(take_while(|b| b != b'\n' && b != b'\r'), |_| None)(s)
            }
        }
    }

    move |s: &[u8]| {
        context(
            "invalid MCC caption",
            map(
                tuple((
                    timecode,
                    opt(tuple((
                        char('.'),
                        one_of("01"),
                        opt(tuple((char(','), digits))),
                    ))),
                    char('\t'),
                    parse(parse_payload),
                    end_of_line,
                )),
                |(tc, _, _, value, _)| MccLine::Caption(tc, value),
            ),
        )(s)
    }
}

/// MCC parser the parses line-by-line and keeps track of the current state in the file.
impl MccParser {
    pub fn new() -> Self {
        Self {
            state: State::Header,
        }
    }

    pub fn new_scan_captions() -> Self {
        Self {
            state: State::Captions,
        }
    }

    pub fn reset(&mut self) {
        self.state = State::Header;
    }

    pub fn parse_line<'a>(
        &mut self,
        line: &'a [u8],
        parse_payload: bool,
    ) -> Result<MccLine<'a>, nom::error::Error<&'a [u8]>> {
        use nom::branch::alt;

        match self.state {
            State::Header => header(line)
                .map(|v| {
                    self.state = State::EmptyAfterHeader;
                    v.1
                })
                .map_err(|err| match err {
                    nom::Err::Incomplete(_) => unreachable!(),
                    nom::Err::Error(e) | nom::Err::Failure(e) => e,
                }),
            State::EmptyAfterHeader => empty_line(line)
                .map(|v| {
                    self.state = State::Comments;
                    v.1
                })
                .map_err(|err| match err {
                    nom::Err::Incomplete(_) => unreachable!(),
                    nom::Err::Error(e) | nom::Err::Failure(e) => e,
                }),
            State::Comments => alt((empty_line, comment))(line)
                .map(|v| {
                    if v.1 == MccLine::Empty {
                        self.state = State::Metadata;
                    }

                    v.1
                })
                .map_err(|err| match err {
                    nom::Err::Incomplete(_) => unreachable!(),
                    nom::Err::Error(e) | nom::Err::Failure(e) => e,
                }),
            State::Metadata => alt((empty_line, uuid, time_code_rate, metadata))(line)
                .map(|v| {
                    if v.1 == MccLine::Empty {
                        self.state = State::Captions;
                    }

                    v.1
                })
                .map_err(|err| match err {
                    nom::Err::Incomplete(_) => unreachable!(),
                    nom::Err::Error(e) | nom::Err::Failure(e) => e,
                }),
            State::Captions => caption(parse_payload)(line)
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
            header(b"File Format=MacCaption_MCC V1.0".as_ref()),
            Ok((b"".as_ref(), MccLine::Header))
        );

        assert_eq!(
            header(b"File Format=MacCaption_MCC V1.0\n".as_ref()),
            Ok((b"".as_ref(), MccLine::Header))
        );

        assert_eq!(
            header(b"File Format=MacCaption_MCC V1.0\r\n".as_ref()),
            Ok((b"".as_ref(), MccLine::Header))
        );

        assert_eq!(
            header(b"File Format=MacCaption_MCC V2.0\r\n".as_ref()),
            Ok((b"".as_ref(), MccLine::Header))
        );

        assert_eq!(
            header(b"File Format=MacCaption_MCC V1.1".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b"1.1".as_ref(),
                nom::error::ErrorKind::Tag
            ))),
        );
    }

    #[test]
    fn test_empty_line() {
        assert_eq!(empty_line(b"".as_ref()), Ok((b"".as_ref(), MccLine::Empty)));

        assert_eq!(
            empty_line(b"\n".as_ref()),
            Ok((b"".as_ref(), MccLine::Empty))
        );

        assert_eq!(
            empty_line(b"\r\n".as_ref()),
            Ok((b"".as_ref(), MccLine::Empty))
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
    fn test_comment() {
        assert_eq!(
            comment(b"// blabla".as_ref()),
            Ok((b"".as_ref(), MccLine::Comment))
        );

        assert_eq!(
            comment(b"//\n".as_ref()),
            Ok((b"".as_ref(), MccLine::Comment))
        );

        assert_eq!(
            comment(b"//".as_ref()),
            Ok((b"".as_ref(), MccLine::Comment))
        );

        assert_eq!(
            comment(b" //".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b" //".as_ref(),
                nom::error::ErrorKind::Tag
            ))),
        );
    }

    #[test]
    fn test_uuid() {
        assert_eq!(
            uuid(b"UUID=1234".as_ref()),
            Ok((b"".as_ref(), MccLine::UUID(b"1234".as_ref())))
        );

        assert_eq!(
            uuid(b"UUID=1234\n".as_ref()),
            Ok((b"".as_ref(), MccLine::UUID(b"1234".as_ref())))
        );

        assert_eq!(
            uuid(b"UUID=1234\r\n".as_ref()),
            Ok((b"".as_ref(), MccLine::UUID(b"1234".as_ref())))
        );

        assert_eq!(
            uuid(b"UUID=".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b"".as_ref(),
                nom::error::ErrorKind::TakeWhile1
            ))),
        );

        assert_eq!(
            uuid(b"uUID=1234".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b"uUID=1234".as_ref(),
                nom::error::ErrorKind::Tag,
            ))),
        );
    }

    #[test]
    fn test_time_code_rate() {
        assert_eq!(
            time_code_rate(b"Time Code Rate=30".as_ref()),
            Ok((b"".as_ref(), MccLine::TimeCodeRate(30, false)))
        );

        assert_eq!(
            time_code_rate(b"Time Code Rate=30DF".as_ref()),
            Ok((b"".as_ref(), MccLine::TimeCodeRate(30, true)))
        );

        assert_eq!(
            time_code_rate(b"Time Code Rate=60".as_ref()),
            Ok((b"".as_ref(), MccLine::TimeCodeRate(60, false)))
        );

        assert_eq!(
            time_code_rate(b"Time Code Rate=17F".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b"F".as_ref(),
                nom::error::ErrorKind::Eof
            ))),
        );

        assert_eq!(
            time_code_rate(b"Time Code Rate=256".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b"256".as_ref(),
                nom::error::ErrorKind::Verify
            ))),
        );
    }

    #[test]
    fn test_metadata() {
        assert_eq!(
            metadata(b"Creation Date=Thursday, June 04, 2015".as_ref()),
            Ok((
                b"".as_ref(),
                MccLine::Metadata(
                    b"Creation Date".as_ref(),
                    b"Thursday, June 04, 2015".as_ref()
                ),
            ))
        );

        assert_eq!(
            metadata(b"Creation Date= ".as_ref()),
            Ok((
                b"".as_ref(),
                MccLine::Metadata(b"Creation Date".as_ref(), b" ".as_ref()),
            ))
        );

        assert_eq!(
            metadata(b"Creation Date".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b"".as_ref(),
                nom::error::ErrorKind::Char
            ))),
        );

        assert_eq!(
            metadata(b"Creation Date\n".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b"".as_ref(),
                nom::error::ErrorKind::Char,
            ))),
        );

        assert_eq!(
            metadata(b"Creation Date=".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b"".as_ref(),
                nom::error::ErrorKind::TakeWhile1
            ))),
        );

        assert_eq!(
            metadata(b"Creation Date=\n".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b"\n".as_ref(),
                nom::error::ErrorKind::TakeWhile1
            ))),
        );
    }

    #[test]
    fn test_caption() {
        assert_eq!(
            caption(true)(b"00:00:00:00\tT52S524F67ZZ72F4QROO7391UC13FFF74ZZAEB4".as_ref()),
            Ok((
                b"".as_ref(),
                MccLine::Caption(
                    TimeCode {
                        hours: 0,
                        minutes: 0,
                        seconds: 0,
                        frames: 0,
                        drop_frame: false
                    },
                    Some(vec![
                        0x61, 0x01, 0x52, 0x96, 0x69, 0x52, 0x4F, 0x67, 0x00, 0x00, 0x72, 0xF4,
                        0xFC, 0x80, 0x80, 0xFD, 0x80, 0x80, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0x73, 0x91, 0xE1, 0x00, 0x00, 0xC1, 0x3F, 0xFF, 0x74, 0x00, 0x00, 0xAE,
                        0xB4
                    ])
                ),
            ))
        );

        assert_eq!(
            caption(true)(b"00:00:00:00.0\tT52S524F67ZZ72F4QROO7391UC13FFF74ZZAEB4".as_ref()),
            Ok((
                b"".as_ref(),
                MccLine::Caption(
                    TimeCode {
                        hours: 0,
                        minutes: 0,
                        seconds: 0,
                        frames: 0,
                        drop_frame: false
                    },
                    Some(vec![
                        0x61, 0x01, 0x52, 0x96, 0x69, 0x52, 0x4F, 0x67, 0x00, 0x00, 0x72, 0xF4,
                        0xFC, 0x80, 0x80, 0xFD, 0x80, 0x80, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0x73, 0x91, 0xE1, 0x00, 0x00, 0xC1, 0x3F, 0xFF, 0x74, 0x00, 0x00, 0xAE,
                        0xB4
                    ])
                ),
            ))
        );

        assert_eq!(
            caption(true)(b"00:00:00:00.0,9\tT52S524F67ZZ72F4QROO7391UC13FFF74ZZAEB4".as_ref()),
            Ok((
                b"".as_ref(),
                MccLine::Caption(
                    TimeCode {
                        hours: 0,
                        minutes: 0,
                        seconds: 0,
                        frames: 0,
                        drop_frame: false
                    },
                    Some(vec![
                        0x61, 0x01, 0x52, 0x96, 0x69, 0x52, 0x4F, 0x67, 0x00, 0x00, 0x72, 0xF4,
                        0xFC, 0x80, 0x80, 0xFD, 0x80, 0x80, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00, 0xFA, 0x00, 0x00,
                        0x73, 0x91, 0xE1, 0x00, 0x00, 0xC1, 0x3F, 0xFF, 0x74, 0x00, 0x00, 0xAE,
                        0xB4
                    ])
                ),
            ))
        );

        assert_eq!(
            caption(true)(b"Creation Date=\n".as_ref()),
            Err(nom::Err::Error(nom::error::Error::new(
                b"Creation Date=\n".as_ref(),
                nom::error::ErrorKind::MapRes
            ))),
        );
    }

    #[test]
    fn test_parser() {
        let mcc_file = include_bytes!("../../tests/captions-test_708.mcc");
        let mut reader = crate::line_reader::LineReader::new();
        let mut parser = MccParser::new();
        let mut line_cnt = 0;

        reader.push(Vec::from(mcc_file.as_ref()));

        while let Some(line) = reader.line() {
            let res = match parser.parse_line(line, true) {
                Ok(res) => res,
                Err(err) => panic!("Couldn't parse line {}: {:?}", line_cnt, err),
            };

            match line_cnt {
                0 => assert_eq!(res, MccLine::Header),
                1 | 37 | 43 => assert_eq!(res, MccLine::Empty),
                x if x >= 2 && x <= 36 => assert_eq!(res, MccLine::Comment),
                38 => assert_eq!(
                    res,
                    MccLine::UUID(b"CA8BC94D-9931-4EEE-812F-2D68FA74F287".as_ref())
                ),
                39 => assert_eq!(
                    res,
                    MccLine::Metadata(
                        b"Creation Program".as_ref(),
                        b"Adobe Premiere Pro CC (Windows)".as_ref()
                    )
                ),
                40 => assert_eq!(
                    res,
                    MccLine::Metadata(
                        b"Creation Date".as_ref(),
                        b"Thursday, June 04, 2015".as_ref()
                    )
                ),
                41 => assert_eq!(
                    res,
                    MccLine::Metadata(b"Creation Time".as_ref(), b"13:48:25".as_ref())
                ),
                42 => assert_eq!(res, MccLine::TimeCodeRate(30, true)),
                _ => match res {
                    MccLine::Caption(_, _) => (),
                    res => panic!("Expected caption at line {}, got {:?}", line_cnt, res),
                },
            }

            line_cnt += 1;
        }
    }
}
