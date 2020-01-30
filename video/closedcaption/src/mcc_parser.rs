// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
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

use combine;
use combine::parser::byte::hex_digit;
use combine::parser::range::{range, take_while1};
use combine::parser::repeat::skip_many;
use combine::parser::EasyParser;
use combine::{any, choice, eof, from_str, many1, one_of, optional, token, unexpected_any, value};
use combine::{ParseError, Parser, RangeStream};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeCode {
    pub hours: u32,
    pub minutes: u32,
    pub seconds: u32,
    pub frames: u32,
    pub drop_frame: bool,
}

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

/// Parser for parsing a run of ASCII, decimal digits and converting them into a `u32`
fn digits<'a, I: 'a>() -> impl Parser<I, Output = u32>
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    from_str(take_while1(|c: u8| c >= b'0' && c <= b'9').message("while parsing digits"))
}

/// Parser for a run of decimal digits, that converts them into a `u32` and checks if the result is
/// in the allowed range.
fn digits_range<'a, I: 'a, R: std::ops::RangeBounds<u32>>(range: R) -> impl Parser<I, Output = u32>
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    digits().then(move |v| {
        if range.contains(&v) {
            value(v).left()
        } else {
            unexpected_any("digits out of range").right()
        }
    })
}

/// Parser for a timecode in the form `hh:mm:ss:fs`
fn timecode<'a, I: 'a>() -> impl Parser<I, Output = TimeCode>
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    (
        digits(),
        token(b':'),
        digits_range(0..60),
        token(b':'),
        digits_range(0..60),
        one_of([b':', b'.', b';', b','].iter().cloned()),
        digits(),
    )
        .map(|(hours, _, minutes, _, seconds, sep, frames)| TimeCode {
            hours,
            minutes,
            seconds,
            frames,
            drop_frame: sep == b';' || sep == b',',
        })
        .message("while parsing timecode")
}

/// Parser that checks for EOF and optionally `\n` or `\r\n` before EOF
fn end_of_line<'a, I: 'a>() -> impl Parser<I, Output = ()> + 'a
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    (
        optional(choice((range(b"\n".as_ref()), range(b"\r\n".as_ref())))),
        eof(),
    )
        .map(|_| ())
        .message("while parsing end of line")
}

/// Parser for the MCC header
fn header<'a, I: 'a>() -> impl Parser<I, Output = MccLine<'a>>
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    (
        range(b"File Format=MacCaption_MCC V".as_ref()),
        choice!(range(b"1.0".as_ref()), range(b"2.0".as_ref())),
        end_of_line(),
    )
        .map(|_| MccLine::Header)
        .message("while parsing header")
}

/// Parser that accepts only an empty line
fn empty_line<'a, I: 'a>() -> impl Parser<I, Output = MccLine<'a>>
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    end_of_line()
        .map(|_| MccLine::Empty)
        .message("while parsing empty line")
}

/// Parser for an MCC comment, i.e. a line starting with `//`. We don't return the actual comment
/// text as it's irrelevant for us.
fn comment<'a, I: 'a>() -> impl Parser<I, Output = MccLine<'a>>
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    (token(b'/'), token(b'/'), skip_many(any()))
        .map(|_| MccLine::Comment)
        .message("while parsing comment")
}

/// Parser for the MCC UUID line.
fn uuid<'a, I: 'a>() -> impl Parser<I, Output = MccLine<'a>>
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    (
        range(b"UUID=".as_ref()),
        take_while1(|b| b != b'\n' && b != b'\r'),
        end_of_line(),
    )
        .map(|(_, uuid, _)| MccLine::UUID(uuid))
        .message("while parsing UUID")
}

/// Parser for the MCC Time Code Rate line.
fn time_code_rate<'a, I: 'a>() -> impl Parser<I, Output = MccLine<'a>>
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    (
        range(b"Time Code Rate=".as_ref()),
        digits_range(1..256)
            .and(optional(range(b"DF".as_ref())))
            .map(|(v, df)| MccLine::TimeCodeRate(v as u8, df.is_some())),
        end_of_line(),
    )
        .map(|(_, v, _)| v)
        .message("while parsing time code rate")
}

/// Parser for generic MCC metadata lines in the form `key=value`.
fn metadata<'a, I: 'a>() -> impl Parser<I, Output = MccLine<'a>>
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    (
        take_while1(|b| b != b'='),
        token(b'='),
        take_while1(|b| b != b'\n' && b != b'\r'),
        end_of_line(),
    )
        .map(|(name, _, value, _)| MccLine::Metadata(name, value))
        .message("while parsing metadata")
}

/// A single MCC payload item. This is ASCII hex encoded bytes plus some single-character
/// short-cuts for common byte sequences.
///
/// It returns an `Either` of the single hex encoded byte or the short-cut byte sequence as a
/// static byte slice.
fn mcc_payload_item<'a, I: 'a>() -> impl Parser<I, Output = Either<u8, &'static [u8]>>
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    // FIXME: Switch back to the choice! macro once https://github.com/rust-lang/rust/issues/68666
    // is fixed and we depend on a new enough Rust version.
    choice((
        token(b'G').map(|_| Either::Right([0xfau8, 0x00, 0x00].as_ref())),
        token(b'H').map(|_| Either::Right([0xfau8, 0x00, 0x00, 0xfa, 0x00, 0x00].as_ref())),
        token(b'I').map(|_| {
            Either::Right([0xfau8, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00].as_ref())
        }),
        token(b'J').map(|_| {
            Either::Right(
                [
                    0xfau8, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                ]
                .as_ref(),
            )
        }),
        token(b'K').map(|_| {
            Either::Right(
                [
                    0xfau8, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                    0x00, 0x00,
                ]
                .as_ref(),
            )
        }),
        token(b'L').map(|_| {
            Either::Right(
                [
                    0xfau8, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                    0x00, 0x00, 0xfa, 0x00, 0x00,
                ]
                .as_ref(),
            )
        }),
        token(b'M').map(|_| {
            Either::Right(
                [
                    0xfau8, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                    0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                ]
                .as_ref(),
            )
        }),
        token(b'N').map(|_| {
            Either::Right(
                [
                    0xfau8, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                    0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00,
                ]
                .as_ref(),
            )
        }),
        token(b'O').map(|_| {
            Either::Right(
                [
                    0xfau8, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa,
                    0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00, 0x00, 0xfa, 0x00,
                    0x00,
                ]
                .as_ref(),
            )
        }),
        token(b'P').map(|_| Either::Right([0xfbu8, 0x80, 0x80].as_ref())),
        token(b'Q').map(|_| Either::Right([0xfcu8, 0x80, 0x80].as_ref())),
        token(b'R').map(|_| Either::Right([0xfdu8, 0x80, 0x80].as_ref())),
        token(b'S').map(|_| Either::Right([0x96u8, 0x69].as_ref())),
        token(b'T').map(|_| Either::Right([0x61u8, 0x01].as_ref())),
        token(b'U').map(|_| Either::Right([0xe1u8, 0x00, 0x00].as_ref())),
        token(b'Z').map(|_| Either::Left(0x00u8)),
        (hex_digit(), hex_digit()).map(|(u, l)| {
            let hex_to_u8 = |v: u8| match v {
                v if v >= b'0' && v <= b'9' => v - b'0',
                v if v >= b'A' && v <= b'F' => 10 + v - b'A',
                v if v >= b'a' && v <= b'f' => 10 + v - b'a',
                _ => unreachable!(),
            };
            let val = (hex_to_u8(u) << 4) | hex_to_u8(l);
            Either::Left(val)
        }),
    ))
    .message("while parsing MCC payload")
}

/// A wrapper around `Vec<u8>` that implements `Extend` in a special way. It can be
/// extended from an iterator of `Either<u8, &[u8]>` while the default `Extend` implementation for
/// `Vec` only allows to extend over vector items.
struct VecExtend(Vec<u8>);

impl Default for VecExtend {
    fn default() -> Self {
        VecExtend(Vec::with_capacity(256))
    }
}

impl<'a> Extend<Either<u8, &'a [u8]>> for VecExtend {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = Either<u8, &'a [u8]>>,
    {
        for item in iter {
            match item {
                Either::Left(v) => self.0.push(v),
                Either::Right(v) => self.0.extend_from_slice(v),
            }
        }
    }
}

/// Parser for the whole MCC payload with conversion to the underlying byte values.
fn mcc_payload<'a, I: 'a>() -> impl Parser<I, Output = Vec<u8>>
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    many1(mcc_payload_item())
        .map(|v: VecExtend| v.0)
        .message("while parsing MCC payloads")
}

/// Parser for a MCC caption line in the form `timecode\tpayload`.
fn caption<'a, I: 'a>(parse_payload: bool) -> impl Parser<I, Output = MccLine<'a>>
where
    I: RangeStream<Token = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Token, I::Range, I::Position>,
{
    (
        timecode(),
        optional((
            token(b'.'),
            one_of([b'0', b'1'].iter().cloned()),
            optional((token(b','), digits())),
        )),
        token(b'\t'),
        if parse_payload {
            mcc_payload().map(Some).right()
        } else {
            skip_many(any()).map(|_| None).left()
        },
        end_of_line(),
    )
        .map(|(tc, _, _, value, _)| MccLine::Caption(tc, value))
        .message("while parsing caption")
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
    ) -> Result<MccLine<'a>, combine::easy::ParseError<&'a [u8]>> {
        match self.state {
            State::Header => header()
                .message("while in Header state")
                .easy_parse(line)
                .map(|v| {
                    self.state = State::EmptyAfterHeader;
                    v.0
                }),
            State::EmptyAfterHeader => empty_line()
                .message("while in EmptyAfterHeader state")
                .easy_parse(line)
                .map(|v| {
                    self.state = State::Comments;
                    v.0
                }),
            State::Comments => choice!(empty_line(), comment())
                .message("while in Comments state")
                .easy_parse(line)
                .map(|v| {
                    if v.0 == MccLine::Empty {
                        self.state = State::Metadata;
                    }

                    v.0
                }),
            State::Metadata => choice!(empty_line(), uuid(), time_code_rate(), metadata())
                .message("while in Metadata state")
                .easy_parse(line)
                .map(|v| {
                    if v.0 == MccLine::Empty {
                        self.state = State::Captions;
                    }

                    v.0
                }),
            State::Captions => caption(parse_payload)
                .message("while in Captions state")
                .easy_parse(line)
                .map(|v| v.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use combine::error::UnexpectedParse;

    #[test]
    fn test_timecode() {
        let mut parser = timecode();
        assert_eq!(
            parser.parse(b"11:12:13;14".as_ref()),
            Ok((
                TimeCode {
                    hours: 11,
                    minutes: 12,
                    seconds: 13,
                    frames: 14,
                    drop_frame: true
                },
                b"".as_ref()
            ))
        );

        assert_eq!(
            parser.parse(b"11:12:13:14".as_ref()),
            Ok((
                TimeCode {
                    hours: 11,
                    minutes: 12,
                    seconds: 13,
                    frames: 14,
                    drop_frame: false
                },
                b"".as_ref()
            ))
        );

        assert_eq!(
            parser.parse(b"11:12:13:14abcd".as_ref()),
            Ok((
                TimeCode {
                    hours: 11,
                    minutes: 12,
                    seconds: 13,
                    frames: 14,
                    drop_frame: false
                },
                b"abcd".as_ref()
            ))
        );

        assert_eq!(
            parser.parse(b"abcd11:12:13:14".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );
    }

    #[test]
    fn test_header() {
        let mut parser = header();
        assert_eq!(
            parser.parse(b"File Format=MacCaption_MCC V1.0".as_ref()),
            Ok((MccLine::Header, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"File Format=MacCaption_MCC V1.0\n".as_ref()),
            Ok((MccLine::Header, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"File Format=MacCaption_MCC V1.0\r\n".as_ref()),
            Ok((MccLine::Header, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"File Format=MacCaption_MCC V2.0\r\n".as_ref()),
            Ok((MccLine::Header, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"File Format=MacCaption_MCC V1.1".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );
    }

    #[test]
    fn test_empty_line() {
        let mut parser = empty_line();
        assert_eq!(
            parser.parse(b"".as_ref()),
            Ok((MccLine::Empty, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"\n".as_ref()),
            Ok((MccLine::Empty, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"\r\n".as_ref()),
            Ok((MccLine::Empty, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b" \r\n".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );
    }

    #[test]
    fn test_comment() {
        let mut parser = comment();
        assert_eq!(
            parser.parse(b"// blabla".as_ref()),
            Ok((MccLine::Comment, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"//\n".as_ref()),
            Ok((MccLine::Comment, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"//".as_ref()),
            Ok((MccLine::Comment, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b" //".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );
    }

    #[test]
    fn test_uuid() {
        let mut parser = uuid();
        assert_eq!(
            parser.parse(b"UUID=1234".as_ref()),
            Ok((MccLine::UUID(b"1234".as_ref()), b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"UUID=1234\n".as_ref()),
            Ok((MccLine::UUID(b"1234".as_ref()), b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"UUID=1234\r\n".as_ref()),
            Ok((MccLine::UUID(b"1234".as_ref()), b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"UUID=".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );

        assert_eq!(
            parser.parse(b"uUID=1234".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );
    }

    #[test]
    fn test_time_code_rate() {
        let mut parser = time_code_rate();
        assert_eq!(
            parser.parse(b"Time Code Rate=30".as_ref()),
            Ok((MccLine::TimeCodeRate(30, false), b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"Time Code Rate=30DF".as_ref()),
            Ok((MccLine::TimeCodeRate(30, true), b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"Time Code Rate=60".as_ref()),
            Ok((MccLine::TimeCodeRate(60, false), b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"Time Code Rate=17F".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );

        assert_eq!(
            parser.parse(b"Time Code Rate=256".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );
    }

    #[test]
    fn test_metadata() {
        let mut parser = metadata();
        assert_eq!(
            parser.parse(b"Creation Date=Thursday, June 04, 2015".as_ref()),
            Ok((
                MccLine::Metadata(
                    b"Creation Date".as_ref(),
                    b"Thursday, June 04, 2015".as_ref()
                ),
                b"".as_ref()
            ))
        );

        assert_eq!(
            parser.parse(b"Creation Date= ".as_ref()),
            Ok((
                MccLine::Metadata(b"Creation Date".as_ref(), b" ".as_ref()),
                b"".as_ref()
            ))
        );

        assert_eq!(
            parser.parse(b"Creation Date".as_ref()),
            Err(UnexpectedParse::Eoi)
        );

        assert_eq!(
            parser.parse(b"Creation Date\n".as_ref()),
            Err(UnexpectedParse::Eoi)
        );

        assert_eq!(
            parser.parse(b"Creation Date=".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );

        assert_eq!(
            parser.parse(b"Creation Date=\n".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );
    }

    #[test]
    fn test_caption() {
        let mut parser = caption(true);
        assert_eq!(
            parser.parse(b"00:00:00:00\tT52S524F67ZZ72F4QROO7391UC13FFF74ZZAEB4".as_ref()),
            Ok((
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
                b"".as_ref()
            ))
        );

        assert_eq!(
            parser.parse(b"00:00:00:00.0\tT52S524F67ZZ72F4QROO7391UC13FFF74ZZAEB4".as_ref()),
            Ok((
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
                b"".as_ref()
            ))
        );

        assert_eq!(
            parser.parse(b"00:00:00:00.0,9\tT52S524F67ZZ72F4QROO7391UC13FFF74ZZAEB4".as_ref()),
            Ok((
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
                b"".as_ref()
            ))
        );

        assert_eq!(
            parser.parse(b"Creation Date=\n".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );
    }

    #[test]
    fn test_parser() {
        let mcc_file = include_bytes!("../tests/captions-test_708.mcc");
        let mut reader = crate::line_reader::LineReader::new();
        let mut parser = MccParser::new();
        let mut line_cnt = 0;

        reader.push(Vec::from(mcc_file.as_ref()));

        while let Some(line) = reader.get_line() {
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
