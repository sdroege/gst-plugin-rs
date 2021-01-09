// Copyright (C) 2020 Jordan Petridis <jordan@centricular.com>
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

use byteorder::{BigEndian, WriteBytesExt};
use crc::crc32;
use nom::{
    self, bytes::complete::take, combinator::map_res, multi::many0, number::complete::be_u16,
    number::complete::be_u32, number::complete::be_u8, sequence::tuple, IResult,
};
use std::borrow::Cow;
use std::io::{self, Write};

#[derive(Debug)]
struct Prelude {
    total_bytes: u32,
    header_bytes: u32,
    prelude_crc: u32,
}

#[derive(Debug)]
pub struct Header {
    pub name: Cow<'static, str>,
    pub value_type: u8,
    pub value: Cow<'static, str>,
}

#[derive(Debug)]
pub struct Packet<'a> {
    prelude: Prelude,
    headers: Vec<Header>,
    pub payload: &'a [u8],
    msg_crc: u32,
}

fn write_header<W: Write>(w: &mut W, header: &Header) -> Result<(), io::Error> {
    w.write_u8(header.name.len() as u8)?;
    w.write_all(header.name.as_bytes())?;
    w.write_u8(header.value_type)?;
    w.write_u16::<BigEndian>(header.value.len() as u16)?;
    w.write_all(header.value.as_bytes())?;
    Ok(())
}

fn write_headers<W: Write>(w: &mut W, headers: &[Header]) -> Result<(), io::Error> {
    for header in headers {
        write_header(w, header)?;
    }
    Ok(())
}

pub fn encode_packet(payload: &[u8], headers: &[Header]) -> Result<Vec<u8>, io::Error> {
    let mut res = Vec::with_capacity(1024);

    // Total length
    res.write_u32::<BigEndian>(0)?;
    // Header length
    res.write_u32::<BigEndian>(0)?;
    // Prelude CRC32 placeholder
    res.write_u32::<BigEndian>(0)?;

    // Write all headers
    write_headers(&mut res, headers)?;

    // Rewrite header length
    let header_length = res.len() - 12;
    (&mut res[4..8]).write_u32::<BigEndian>(header_length as u32)?;

    // Write payload
    res.write_all(payload)?;

    // Rewrite total length
    let total_length = res.len() + 4;
    (&mut res[0..4]).write_u32::<BigEndian>(total_length as u32)?;

    // Rewrite the prelude crc since we replaced the lengths
    let prelude_crc = crc32::checksum_ieee(&res[0..8]);
    (&mut res[8..12]).write_u32::<BigEndian>(prelude_crc)?;

    // Message CRC
    let message_crc = crc32::checksum_ieee(&res);
    res.write_u32::<BigEndian>(message_crc)?;

    Ok(res)
}

fn parse_prelude(input: &[u8]) -> IResult<&[u8], Prelude> {
    map_res(
        tuple((be_u32, be_u32, be_u32)),
        |(total_bytes, header_bytes, prelude_crc)| {
            let sum = crc32::checksum_ieee(&input[0..8]);
            if prelude_crc != sum {
                return Err(nom::Err::Error((
                    "Prelude CRC doesn't match",
                    nom::error::ErrorKind::MapRes,
                )));
            }

            Ok(Prelude {
                total_bytes,
                header_bytes,
                prelude_crc,
            })
        },
    )(input)
}

fn parse_header(input: &[u8]) -> IResult<&[u8], Header> {
    let (input, header_length) = be_u8(input)?;
    let (input, name) = map_res(take(header_length), std::str::from_utf8)(input)?;
    let (input, value_type) = be_u8(input)?;
    let (input, value_length) = be_u16(input)?;
    let (input, value) = map_res(take(value_length), std::str::from_utf8)(input)?;

    let header = Header {
        name: name.to_string().into(),
        value_type,
        value: value.to_string().into(),
    };

    Ok((input, header))
}

pub fn packet_is_exception(packet: &Packet) -> bool {
    for header in &packet.headers {
        if header.name == ":message-type" && header.value == "exception" {
            return true;
        }
    }

    false
}

pub fn parse_packet(input: &[u8]) -> IResult<&[u8], Packet> {
    let (remainder, prelude) = parse_prelude(input)?;

    // Check the crc of the whole input
    let sum = crc32::checksum_ieee(&input[..input.len() - 4]);
    let (_, msg_crc) = be_u32(&input[input.len() - 4..])?;

    if msg_crc != sum {
        return Err(nom::Err::Error(nom::error::Error::new(
            b"Prelude CRC doesn't match",
            nom::error::ErrorKind::MapRes,
        )));
    }

    let (remainder, header_input) = take(prelude.header_bytes)(remainder)?;
    let (_, headers) = many0(parse_header)(header_input)?;

    let payload_length = prelude.total_bytes - prelude.header_bytes - 4 - 12;
    let (remainder, payload) = take(payload_length)(remainder)?;

    // only the message_crc we check before should be remaining now
    assert_eq!(remainder.len(), 4);

    Ok((
        input,
        Packet {
            prelude,
            headers,
            payload,
            msg_crc,
        },
    ))
}
