// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
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

use combine;
use combine::parser::byte::hex_digit;
use combine::parser::range::{range, take_while1};
use combine::{choice, eof, from_str, many1, one_of, optional, token, unexpected_any, value};
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
pub enum SccLine {
    Header,
    Empty,
    Caption(TimeCode, Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Init,
    Header,
    Empty,
    Captions,
}

#[derive(Debug)]
pub struct SccParser {
    state: State,
}

/// Parser for parsing a run of ASCII, decimal digits and converting them into a `u32`
fn digits<'a, I: 'a>() -> impl Parser<Input = I, Output = u32>
where
    I: RangeStream<Item = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Item, I::Range, I::Position>,
{
    from_str(take_while1(|c: u8| c >= b'0' && c <= b'9').message("while parsing digits"))
}

/// Copy from std::ops::RangeBounds as it's not stabilized yet.
///
/// Checks if `item` is in the range `range`.
fn contains<R: std::ops::RangeBounds<U>, U>(range: &R, item: &U) -> bool
where
    U: ?Sized + PartialOrd<U>,
{
    (match range.start_bound() {
        std::ops::Bound::Included(ref start) => *start <= item,
        std::ops::Bound::Excluded(ref start) => *start < item,
        std::ops::Bound::Unbounded => true,
    }) && (match range.end_bound() {
        std::ops::Bound::Included(ref end) => item <= *end,
        std::ops::Bound::Excluded(ref end) => item < *end,
        std::ops::Bound::Unbounded => true,
    })
}

/// Parser for a run of decimal digits, that converts them into a `u32` and checks if the result is
/// in the allowed range.
fn digits_range<'a, I: 'a, R: std::ops::RangeBounds<u32>>(
    range: R,
) -> impl Parser<Input = I, Output = u32>
where
    I: RangeStream<Item = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Item, I::Range, I::Position>,
{
    digits().then(move |v| {
        if contains(&range, &v) {
            value(v).left()
        } else {
            unexpected_any("digits out of range").right()
        }
    })
}

/// Parser for a timecode in the form `hh:mm:ss:fs`
fn timecode<'a, I: 'a>() -> impl Parser<Input = I, Output = TimeCode>
where
    I: RangeStream<Item = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Item, I::Range, I::Position>,
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
fn end_of_line<'a, I: 'a>() -> impl Parser<Input = I, Output = ()> + 'a
where
    I: RangeStream<Item = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Item, I::Range, I::Position>,
{
    (
        optional(choice((range(b"\n".as_ref()), range(b"\r\n".as_ref())))),
        eof(),
    )
        .map(|_| ())
        .message("while parsing end of line")
}

/// Parser for the SCC header
fn header<'a, I: 'a>() -> impl Parser<Input = I, Output = SccLine> + 'a
where
    I: RangeStream<Item = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Item, I::Range, I::Position>,
{
    (range(b"Scenarist_SCC V1.0".as_ref()), end_of_line())
        .map(|_| SccLine::Header)
        .message("while parsing header")
}

/// Parser that accepts only an empty line
fn empty_line<'a, I: 'a>() -> impl Parser<Input = I, Output = SccLine> + 'a
where
    I: RangeStream<Item = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Item, I::Range, I::Position>,
{
    end_of_line()
        .map(|_| SccLine::Empty)
        .message("while parsing empty line")
}

/// A single SCC payload item. This is ASCII hex encoded bytes.
/// It returns an tuple of `(u8, u8)` of the hex encoded bytes.
fn scc_payload_item<'a, I: 'a>() -> impl Parser<Input = I, Output = (u8, u8)>
where
    I: RangeStream<Item = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Item, I::Range, I::Position>,
{
    ((hex_digit(), hex_digit(), hex_digit(), hex_digit()).map(|(u, l, m, n)| {
        let hex_to_u8 = |v: u8| match v {
            v if v >= b'0' && v <= b'9' => v - b'0',
            v if v >= b'A' && v <= b'F' => 10 + v - b'A',
            v if v >= b'a' && v <= b'f' => 10 + v - b'a',
            _ => unreachable!(),
        };
        let val = (hex_to_u8(u) << 4) | hex_to_u8(l);
        let val2 = (hex_to_u8(m) << 4) | hex_to_u8(n);
        (val, val2)
    }))
    .message("while parsing SCC payload")
}

/// A wrapper around `Vec<u8>` that implements `Extend` in a special way. It can be
/// extended from a `(u8, u8)` while the default `Extend` implementation for
/// `Vec` only allows to extend over vector items.
struct VecExtend(Vec<u8>);

impl Default for VecExtend {
    fn default() -> Self {
        VecExtend(Vec::with_capacity(256))
    }
}

impl Extend<(u8, u8)> for VecExtend {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = (u8, u8)>,
    {
        for (item, item2) in iter {
            self.0.extend_from_slice(&[item, item2]);
        }
    }
}

/// Parser for the whole SCC payload with conversion to the underlying byte values.
fn scc_payload<'a, I: 'a>() -> impl Parser<Input = I, Output = Vec<u8>> + 'a
where
    I: RangeStream<Item = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Item, I::Range, I::Position>,
{
    many1(
        (
            scc_payload_item(),
            choice!(token(b' ').map(|_| ()), end_of_line()),
        )
            .map(|v| v.0),
    )
    .map(|v: VecExtend| v.0)
    .message("while parsing SCC payloads")
}

/// Parser for a SCC caption line in the form `timecode\tpayload`.
fn caption<'a, I: 'a>() -> impl Parser<Input = I, Output = SccLine> + 'a
where
    I: RangeStream<Item = u8, Range = &'a [u8]>,
    I::Error: ParseError<I::Item, I::Range, I::Position>,
{
    (timecode(), token(b'\t'), scc_payload(), end_of_line())
        .map(|(tc, _, value, _)| SccLine::Caption(tc, value))
        .message("while parsing caption")
}

/// SCC parser the parses line-by-line and keeps track of the current state in the file.
impl SccParser {
    pub fn new() -> Self {
        Self { state: State::Init }
    }

    pub fn reset(&mut self) {
        self.state = State::Init;
    }

    pub fn parse_line<'a>(
        &mut self,
        line: &'a [u8],
    ) -> Result<SccLine, combine::easy::Errors<u8, &'a [u8], combine::stream::PointerOffset>> {
        match self.state {
            State::Init => header()
                .message("while in Init state")
                .easy_parse(line)
                .map(|v| {
                    self.state = State::Header;
                    v.0
                }),
            State::Header => empty_line()
                .message("while in Header state")
                .easy_parse(line)
                .map(|v| {
                    self.state = State::Empty;
                    v.0
                }),
            State::Empty => caption()
                .message("while in Empty state")
                .easy_parse(line)
                .map(|v| {
                    self.state = State::Captions;
                    v.0
                }),
            State::Captions => empty_line()
                .message("while in Captions state")
                .easy_parse(line)
                .map(|v| {
                    self.state = State::Empty;
                    v.0
                }),
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
            parser.parse(b"Scenarist_SCC V1.0".as_ref()),
            Ok((SccLine::Header, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"Scenarist_SCC V1.0\n".as_ref()),
            Ok((SccLine::Header, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"Scenarist_SCC V1.0\r\n".as_ref()),
            Ok((SccLine::Header, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"Scenarist_SCC V1.1".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );
    }

    #[test]
    fn test_empty_line() {
        let mut parser = empty_line();
        assert_eq!(
            parser.parse(b"".as_ref()),
            Ok((SccLine::Empty, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"\n".as_ref()),
            Ok((SccLine::Empty, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b"\r\n".as_ref()),
            Ok((SccLine::Empty, b"".as_ref()))
        );

        assert_eq!(
            parser.parse(b" \r\n".as_ref()),
            Err(UnexpectedParse::Unexpected)
        );
    }

    #[test]
    fn test_caption() {
        let mut parser = caption();
        assert_eq!(
            parser.parse(b"01:02:53:14\t94ae 94ae 9420 9420 947a 947a 97a2 97a2 a820 68ef f26e 2068 ef6e 6be9 6e67 2029 942c 942c 8080 8080 942f 942f".as_ref()),
            Ok((
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
                b"".as_ref()
            ))
        );

        assert_eq!(
            parser.parse(b"01:02:55;14	942c 942c".as_ref()),
            Ok((
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
                b"".as_ref()
            ))
        );

        assert_eq!(
            parser.parse(b"01:03:27:29	94ae 94ae 9420 9420 94f2 94f2 c845 d92c 2054 c845 5245 ae80 942c 942c 8080 8080 942f 942f".as_ref()),
            Ok((
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
                b"".as_ref()
            ))
        );

        assert_eq!(
            parser.parse(b"00:00:00;00\t942c 942c".as_ref()),
            Ok((
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
                b"".as_ref()
            ))
        );

        assert_eq!(
            parser.parse(b"00:00:00;00\t942c 942c\r\n".as_ref()),
            Ok((
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
                b"".as_ref()
            ))
        );
    }

    #[test]
    fn test_parser() {
        let scc_file = include_bytes!("../tests/dn2018-1217.scc");
        let mut reader = crate::line_reader::LineReader::new();
        let mut parser = SccParser::new();
        let mut line_cnt = 0;

        reader.push(Vec::from(scc_file.as_ref()));

        while let Some(line) = reader.get_line() {
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
