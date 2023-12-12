//
// Copyright (C) 2023 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use bitstream_io::{BigEndian, BitQueue, BitRead, Endianness};
use std::io;

/// Implementation of the bool decoder from RFC 6386:
/// https://datatracker.ietf.org/doc/html/rfc6386#section-7.3
///
/// See RFC for details.
pub struct BoolDecoder<R: io::Read> {
    reader: R,
    eof: bool,
    range: u32,
    value: u32,
    bit_count: u8,
}

impl<R: io::Read> BoolDecoder<R> {
    #[inline]
    pub fn new(mut reader: R) -> Result<Self, io::Error> {
        let mut input1 = [0u8; 1];
        let mut input2 = [0u8; 1];

        reader.read_exact(&mut input1)?;

        let bit_count = if let Err(err) = reader.read_exact(&mut input2) {
            if err.kind() == io::ErrorKind::UnexpectedEof {
                // no second byte so in a state as if 8 bits were shifted out already
                8
            } else {
                return Err(err);
            }
        } else {
            // have not yet shifted out any bits
            0
        };

        let value = ((input1[0] as u32) << 8) | (input2[0] as u32);

        Ok(BoolDecoder {
            reader,
            eof: false,
            range: 255, // initial range is full
            value,
            bit_count,
        })
    }

    #[inline(always)]
    fn next_bool(&mut self, prob: u16) -> Result<bool, io::Error> {
        assert!(prob <= 256);

        // range and split are identical to the corresponding values
        // used by the encoder when this bool was written

        let split = 1 + (((self.range - 1) * prob as u32) >> 8);
        let split_ = split << 8;

        let ret = if self.value >= split_ {
            self.range -= split; // reduce range
            self.value -= split_; // subtract off left endpoint of interval
            true
        } else {
            self.range = split; // reduce range, no change in left endpoint
            false
        };

        // shift out irrelevant bits
        while self.range < 128 {
            self.value <<= 1;
            self.range <<= 1;

            self.bit_count += 1;
            // shift in new bits 8 at a time
            if self.bit_count == 8 {
                if !self.eof {
                    let mut input = [0u8; 1];
                    if let Err(err) = self.reader.read_exact(&mut input) {
                        if err.kind() == io::ErrorKind::UnexpectedEof {
                            self.eof = true;
                        } else {
                            return Err(err);
                        }
                    } else {
                        self.bit_count = 0;
                        self.value |= input[0] as u32;
                    }
                } else if self.bit_count == 16 {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
            }
        }

        Ok(ret)
    }

    #[inline]
    pub fn next_bit(&mut self) -> Result<bool, io::Error> {
        self.next_bool(128)
    }
}

impl<R: io::Read> BitRead for BoolDecoder<R> {
    fn read_bit(&mut self) -> std::io::Result<bool> {
        self.next_bit()
    }

    fn read<U>(&mut self, mut bits: u32) -> std::io::Result<U>
    where
        U: bitstream_io::Numeric,
    {
        if bits > U::BITS_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "excessive bits for type read",
            ));
        }

        let mut queue = BitQueue::<BigEndian, U>::new();
        while bits > 0 {
            queue.push(1, U::from_u8(self.next_bit()? as u8));
            bits -= 1;
        }

        Ok(queue.value())
    }

    fn read_signed<S>(&mut self, bits: u32) -> std::io::Result<S>
    where
        S: bitstream_io::SignedNumeric,
    {
        BigEndian::read_signed(self, bits)
    }

    fn read_to<V>(&mut self) -> std::io::Result<V>
    where
        V: bitstream_io::Primitive,
    {
        BigEndian::read_primitive(self)
    }

    fn read_as_to<F, V>(&mut self) -> std::io::Result<V>
    where
        F: bitstream_io::Endianness,
        V: bitstream_io::Primitive,
    {
        F::read_primitive(self)
    }

    fn skip(&mut self, mut bits: u32) -> std::io::Result<()> {
        while bits > 0 {
            self.next_bit()?;
            bits -= 1;
        }

        Ok(())
    }

    fn byte_aligned(&self) -> bool {
        false
    }

    fn byte_align(&mut self) {
        unimplemented!()
    }
}
