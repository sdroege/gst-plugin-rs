// GStreamer Data Protocol (GDP) reader
//
// Copyright (C) 2025 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use anyhow::Context;
use bitstream_io::{BigEndian, ByteRead, ByteReader, FromByteStream};

use std::str::FromStr;

#[derive(Clone, Debug)]
#[allow(unused)]
pub(crate) enum PayloadType {
    Buffer,
    Caps,
    Event(u16),
}

#[derive(Clone, Debug)]
struct ClockTime {
    t: Option<gst::ClockTime>,
}

impl FromByteStream for ClockTime {
    type Error = anyhow::Error;

    fn from_reader<R: ByteRead + ?Sized>(r: &mut R) -> anyhow::Result<Self> {
        let t = match r.read::<u64>()? {
            u64::MAX => gst::ClockTime::NONE,
            t => Some(gst::ClockTime::from_nseconds(t)),
        };

        Ok(ClockTime { t })
    }
}

#[derive(Clone, Debug)]
#[allow(unused)]
struct Header {
    version: (u8, u8),
    flags: u8,
    payload_type: PayloadType,
    payload_len: u32,
    pts: Option<gst::ClockTime>,
    dts: Option<gst::ClockTime>,
    duration: Option<gst::ClockTime>,
    offset: u64,
    offset_end: u64,
    buffer_flags: u16,
    crc_header: u16,
    crc_payload: u16,
}

impl FromByteStream for Header {
    type Error = anyhow::Error;

    fn from_reader<R: ByteRead + ?Sized>(r: &mut R) -> anyhow::Result<Self> {
        let version = (
            r.read::<u8>().context("major version")?,
            r.read::<u8>().context("minor version")?,
        );

        let flags = r.read::<u8>().context("flags")?;

        let _ = r.read::<u8>().context("padding")?;

        let payload_type = match r.read::<u16>().context("payload type")? {
            1 => PayloadType::Buffer,
            2 => PayloadType::Caps,
            event_number @ 64.. => PayloadType::Event(event_number - 64),
            _ => anyhow::bail!("Invalid payload type"),
        };

        let payload_len = r.read::<u32>().context("payload length")?;

        let pts = r.parse::<ClockTime>().context("pts")?.t;
        let duration = r.parse::<ClockTime>().context("duration")?.t;

        let offset = r.read::<u64>().context("offset")?;
        let offset_end = r.read::<u64>().context("offset end")?;

        let buffer_flags = r.read::<u16>().context("buffer flags")?;
        let dts = r.parse::<ClockTime>().context("dts")?.t;

        r.skip(6).context("extension padding")?;

        let crc_header = r.read::<u16>().context("crc header")?;
        let crc_payload = r.read::<u16>().context("crc payload")?;

        Ok(Header {
            version,
            flags,
            payload_type,
            payload_len,
            pts,
            duration,
            offset,
            offset_end,
            buffer_flags,
            dts,
            crc_header,
            crc_payload,
        })
    }
}

#[derive(Clone, Debug)]
#[allow(unused)]
pub(crate) enum Item {
    Caps(gst::Caps),
    Buffer(gst::Buffer),
    Event(gst::Event),
}

impl FromByteStream for Item {
    type Error = anyhow::Error;

    fn from_reader<R: ByteRead + ?Sized>(r: &mut R) -> anyhow::Result<Self> {
        use PayloadType::*;

        let hdr = r.parse::<Header>().context("header")?;

        let payload = r.read_to_vec(hdr.payload_len as usize).context("payload")?;

        let item = match hdr.payload_type {
            Caps => {
                let mut payload_str = payload.as_slice();
                while payload_str.ends_with(&[0]) {
                    payload_str = payload_str.strip_suffix(&[0]).unwrap();
                }

                let caps_str = std::str::from_utf8(payload_str).context("caps string")?;
                let caps = gst::Caps::from_str(caps_str).context("caps")?;

                Item::Caps(caps)
            }
            Buffer => {
                let mut buf = gst::Buffer::from_mut_slice(payload);
                let buf_ref = buf.make_mut();

                buf_ref.set_pts(hdr.pts);
                buf_ref.set_dts(hdr.dts);
                buf_ref.set_duration(hdr.duration);

                buf_ref.set_flags(gst::BufferFlags::from_bits_retain(hdr.buffer_flags as u32));

                buf_ref.set_offset(hdr.offset);
                buf_ref.set_offset_end(hdr.offset_end);

                Item::Buffer(buf)
            }
            Event(n) => {
                let mut payload_str = payload.as_slice();
                while payload_str.ends_with(&[0]) {
                    payload_str = payload_str.strip_suffix(&[0]).unwrap();
                }

                let ev_str = std::str::from_utf8(payload_str).context("event string")?;
                let ev_struct = gst::Structure::from_str(ev_str).context("event structure")?;

                // Need to use unsafe and FFI API here because the high-level Rust bindings have
                // no API to create events generically like we need here (which is quite unusual).
                let event = unsafe {
                    use gst::glib::translate::ToGlibPtr;

                    let ev = gst::ffi::gst_event_new_custom(n.into(), ev_struct.to_glib_full());
                    gst::Event::from_glib_full(ev)
                };

                Item::Event(event)
            }
        };

        Ok(item)
    }
}

// Iterator

pub(crate) struct ItemIter<R>
where
    R: std::io::Read,
{
    br: ByteReader<R, BigEndian>,
}

impl<R: std::io::Read> ItemIter<R> {
    pub(crate) fn new(reader: R) -> Self {
        let br = ByteReader::endian(reader, BigEndian);

        Self { br }
    }
}

impl<R: std::io::Read> Iterator for ItemIter<R> {
    type Item = Item;

    fn next(&mut self) -> Option<Item> {
        self.br.parse::<Item>().context("GDP item").ok()
    }
}
