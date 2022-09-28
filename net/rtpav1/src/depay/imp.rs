//
// Copyright (C) 2022 Vivienne Watermeier <vwatermeier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, subclass::prelude::*};
use gst_rtp::subclass::prelude::*;
use std::{
    cmp::Ordering,
    io::{Cursor, Read, Seek, SeekFrom},
    sync::Mutex,
};

use bitstream_io::{BitReader, BitWriter};
use once_cell::sync::Lazy;

use crate::common::{
    err_opt, leb128_size, parse_leb128, write_leb128, AggregationHeader, ObuType, SizedObu,
    UnsizedObu, CLOCK_RATE, ENDIANNESS,
};

// TODO: handle internal size fields in RTP OBUs

#[derive(Debug, Default)]
struct State {
    /// used to store outgoing OBUs until the TU is complete
    adapter: gst_base::UniqueAdapter,

    last_timestamp: Option<u32>,
    /// if true, the last packet of a temporal unit has been received
    marked_packet: bool,
    /// holds data for a fragment
    obu_fragment: Option<(UnsizedObu, Vec<u8>)>,
}

#[derive(Debug, Default)]
pub struct RTPAv1Depay {
    state: Mutex<State>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rtpav1depay",
        gst::DebugColorFlags::empty(),
        Some("RTP AV1 Depayloader"),
    )
});

static TEMPORAL_DELIMITER: Lazy<gst::Memory> =
    Lazy::new(|| gst::Memory::from_slice(&[0b0001_0010, 0]));

impl RTPAv1Depay {
    fn reset(&self, element: &<Self as ObjectSubclass>::Type, state: &mut State) {
        gst::debug!(CAT, obj: element, "resetting state");

        *state = State::default()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RTPAv1Depay {
    const NAME: &'static str = "GstRtpAv1Depay";
    type Type = super::RTPAv1Depay;
    type ParentType = gst_rtp::RTPBaseDepayload;
}

impl ObjectImpl for RTPAv1Depay {}

impl GstObjectImpl for RTPAv1Depay {}

impl ElementImpl for RTPAv1Depay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP AV1 Depayloader",
                "Codec/Depayloader/Network/RTP",
                "Depayload AV1 from RTP packets",
                "Vivienne Watermeier <vwatermeier@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("payload", gst::IntRange::new(96, 127))
                    .field("clock-rate", CLOCK_RATE as i32)
                    .field("encoding-name", "AV1")
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/x-av1")
                    .field("parsed", true)
                    .field("stream-format", "obu-stream")
                    .field("alignment", "tu")
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::debug!(CAT, obj: element, "changing state: {}", transition);

        if matches!(transition, gst::StateChange::ReadyToPaused) {
            let mut state = self.state.lock().unwrap();
            self.reset(element, &mut state);
        }

        let ret = self.parent_change_state(element, transition);

        if matches!(transition, gst::StateChange::PausedToReady) {
            let mut state = self.state.lock().unwrap();
            self.reset(element, &mut state);
        }

        ret
    }
}

impl RTPBaseDepayloadImpl for RTPAv1Depay {
    fn handle_event(&self, element: &Self::Type, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::Eos(_) | gst::EventView::FlushStop(_) => {
                let mut state = self.state.lock().unwrap();
                self.reset(element, &mut state);
            }
            _ => (),
        }

        self.parent_handle_event(element, event)
    }

    fn process_rtp_packet(
        &self,
        element: &Self::Type,
        rtp: &gst_rtp::RTPBuffer<gst_rtp::rtp_buffer::Readable>,
    ) -> Option<gst::Buffer> {
        gst::log!(
            CAT,
            obj: element,
            "processing RTP packet with payload type {} and size {}",
            rtp.payload_type(),
            rtp.buffer().size(),
        );

        let payload = rtp.payload().map_err(err_opt!(element, payload_buf)).ok()?;

        let mut state = self.state.lock().unwrap();

        if rtp.buffer().flags().contains(gst::BufferFlags::DISCONT) {
            gst::debug!(CAT, obj: element, "buffer discontinuity");
            self.reset(element, &mut state);
        }

        // number of bytes that can be used in the next outgoing buffer
        let mut bytes_ready = 0;
        let mut reader = Cursor::new(payload);
        let mut ready_obus = gst::Buffer::new();

        let aggr_header = {
            let mut byte = [0; 1];
            reader
                .read_exact(&mut byte)
                .map_err(err_opt!(element, aggr_header_read))
                .ok()?;
            AggregationHeader::from(&byte)
        };

        // handle new temporal units
        if state.marked_packet || state.last_timestamp != Some(rtp.timestamp()) {
            if state.last_timestamp.is_some() && state.obu_fragment.is_some() {
                gst::error!(
                    CAT,
                    obj: element,
                    concat!(
                        "invalid packet: packet is part of a new TU but ",
                        "the previous TU still has an incomplete OBU",
                        "marked_packet: {}, last_timestamp: {:?}"
                    ),
                    state.marked_packet,
                    state.last_timestamp
                );
                self.reset(element, &mut state);
                return None;
            }

            // all the currently stored bytes can be packed into the next outgoing buffer
            bytes_ready = state.adapter.available();

            // the next temporal unit starts with a temporal delimiter OBU
            ready_obus
                .get_mut()
                .unwrap()
                .insert_memory(None, TEMPORAL_DELIMITER.clone());
            state.marked_packet = false;
        }
        state.marked_packet = rtp.is_marker();
        state.last_timestamp = Some(rtp.timestamp());

        // parse and prepare the received OBUs
        let mut idx = 0;

        // handle leading OBU fragment
        if let Some((obu, ref mut bytes)) = &mut state.obu_fragment {
            if !aggr_header.leading_fragment {
                gst::error!(
                    CAT,
                    obj: element,
                    "invalid packet: ignores unclosed OBU fragment"
                );
                return None;
            }

            let (element_size, is_last_obu) =
                find_element_info(element, rtp, &mut reader, &aggr_header, idx)?;

            let bytes_end = bytes.len();
            bytes.resize(bytes_end + element_size as usize, 0);
            reader
                .read_exact(&mut bytes[bytes_end..])
                .map_err(err_opt!(element, buf_read))
                .ok()?;

            // if this OBU is complete, it can be appended to the adapter
            if !(is_last_obu && aggr_header.trailing_fragment) {
                let full_obu = {
                    let size = bytes.len() as u32 - obu.header_len;
                    let leb_size = leb128_size(size) as u32;
                    obu.as_sized(size, leb_size)
                };

                let buffer = translate_obu(element, &mut Cursor::new(bytes.as_slice()), &full_obu)?;

                state.adapter.push(buffer);
                state.obu_fragment = None;
            }
        }

        // handle other OBUs, including trailing fragments
        while reader.position() < rtp.payload_size() as u64 {
            let (element_size, is_last_obu) =
                find_element_info(element, rtp, &mut reader, &aggr_header, idx)?;

            let header_pos = reader.position();
            let mut bitreader = BitReader::endian(&mut reader, ENDIANNESS);
            let obu = UnsizedObu::parse(&mut bitreader)
                .map_err(err_opt!(element, obu_read))
                .ok()?;

            reader
                .seek(SeekFrom::Start(header_pos))
                .map_err(err_opt!(element, buf_read))
                .ok()?;

            // ignore these OBU types
            if matches!(obu.obu_type, ObuType::TemporalDelimiter | ObuType::TileList) {
                reader
                    .seek(SeekFrom::Current(element_size as i64))
                    .map_err(err_opt!(element, buf_read))
                    .ok()?;
            }
            // trailing OBU fragments are stored in the state
            if is_last_obu && aggr_header.trailing_fragment {
                let bytes_left = rtp.payload_size() - (reader.position() as u32);
                let mut bytes = vec![0; bytes_left as usize];
                reader
                    .read_exact(bytes.as_mut_slice())
                    .map_err(err_opt!(element, buf_read))
                    .ok()?;

                state.obu_fragment = Some((obu, bytes));
            }
            // full OBUs elements are translated and appended to the adapter
            else {
                let full_obu = {
                    let size = element_size - obu.header_len;
                    let leb_size = leb128_size(size) as u32;
                    obu.as_sized(size, leb_size)
                };

                ready_obus.append(translate_obu(element, &mut reader, &full_obu)?);
            }

            idx += 1;
        }

        state.adapter.push(ready_obus);

        if state.marked_packet {
            if state.obu_fragment.is_some() {
                gst::error!(
                    CAT,
                    obj: element,
                    concat!(
                        "invalid packet: has marker bit set, but ",
                        "last OBU is not yet complete"
                    )
                );
                self.reset(element, &mut state);
                return None;
            }

            bytes_ready = state.adapter.available();
        }

        // now push all the complete temporal units
        if bytes_ready > 0 {
            gst::log!(
                CAT,
                obj: element,
                "creating buffer containing {} bytes of data...",
                bytes_ready
            );
            Some(
                state
                    .adapter
                    .take_buffer(bytes_ready)
                    .map_err(err_opt!(element, buf_take))
                    .ok()?,
            )
        } else {
            None
        }
    }
}

/// Find out the next OBU element's size, and if it is the last OBU in the packet.
/// The reader is expected to be at the first byte of the element,
/// or its preceding size field if present,
/// and will be at the first byte past the element's size field afterwards.
fn find_element_info(
    element: &<RTPAv1Depay as ObjectSubclass>::Type,
    rtp: &gst_rtp::RTPBuffer<gst_rtp::rtp_buffer::Readable>,
    reader: &mut Cursor<&[u8]>,
    aggr_header: &AggregationHeader,
    index: u32,
) -> Option<(u32, bool)> {
    let element_size: u32;
    let is_last_obu: bool;

    if let Some(count) = aggr_header.obu_count {
        is_last_obu = index + 1 == count as u32;
        element_size = if is_last_obu {
            rtp.payload_size() - (reader.position() as u32)
        } else {
            let mut bitreader = BitReader::endian(reader, ENDIANNESS);
            parse_leb128(&mut bitreader)
                .map_err(err_opt!(element, leb_read))
                .ok()? as u32
        }
    } else {
        element_size = parse_leb128(&mut BitReader::endian(&mut *reader, ENDIANNESS))
            .map_err(err_opt!(element, leb_read))
            .ok()? as u32;
        is_last_obu = match rtp
            .payload_size()
            .cmp(&(reader.position() as u32 + element_size))
        {
            Ordering::Greater => false,
            Ordering::Equal => true,
            Ordering::Less => {
                gst::error!(
                    CAT,
                    obj: element,
                    "invalid packet: size field gives impossibly large OBU size"
                );
                return None;
            }
        };
    }

    Some((element_size, is_last_obu))
}

/// Using OBU data from an RTP packet, construct a buffer containing that OBU in AV1 bitstream format
fn translate_obu(
    element: &<RTPAv1Depay as ObjectSubclass>::Type,
    reader: &mut Cursor<&[u8]>,
    obu: &SizedObu,
) -> Option<gst::Buffer> {
    let mut bytes = gst::Buffer::with_size(obu.full_size() as usize)
        .map_err(err_opt!(element, buf_alloc))
        .ok()?
        .into_mapped_buffer_writable()
        .unwrap();

    // write OBU header
    reader
        .read_exact(&mut bytes[..obu.header_len as usize])
        .map_err(err_opt!(element, buf_read))
        .ok()?;

    // set `has_size_field`
    bytes[0] |= 1 << 1;

    // skip internal size field if present
    if obu.has_size_field {
        parse_leb128(&mut BitReader::endian(&mut *reader, ENDIANNESS))
            .map_err(err_opt!(element, leb_read))
            .ok()?;
    }

    // write size field
    write_leb128(
        &mut BitWriter::endian(
            Cursor::new(&mut bytes[obu.header_len as usize..]),
            ENDIANNESS,
        ),
        obu.size,
    )
    .map_err(err_opt!(element, leb_write))
    .ok()?;

    // write OBU payload
    reader
        .read_exact(&mut bytes[(obu.header_len + obu.leb_size) as usize..])
        .map_err(err_opt!(element, buf_read))
        .ok()?;

    Some(bytes.into_buffer())
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use super::*;
    use gst_rtp::prelude::*;
    use std::io::Cursor;

    #[test]
    fn test_translate_obu() {
        gst::init().unwrap();

        let test_data = [
            (
                SizedObu {
                    obu_type: ObuType::TemporalDelimiter,
                    has_extension: false,
                    has_size_field: false,
                    temporal_id: 0,
                    spatial_id: 0,
                    size: 0,
                    leb_size: 1,
                    header_len: 1,
                    is_fragment: false,
                },
                vec![0b0001_0000],
                vec![0b0001_0010, 0],
            ), (
                SizedObu {
                    obu_type: ObuType::Frame,
                    has_extension: true,
                    has_size_field: false,
                    temporal_id: 3,
                    spatial_id: 2,
                    size: 5,
                    leb_size: 1,
                    header_len: 2,
                    is_fragment: false,
                },
                vec![0b0011_0100, 0b0111_0000, 1, 2, 3, 4, 5],
                vec![0b0011_0110, 0b0111_0000, 0b0000_0101, 1, 2, 3, 4, 5],
            ), (
                SizedObu {
                    obu_type: ObuType::Frame,
                    has_extension: true,
                    has_size_field: true,
                    temporal_id: 3,
                    spatial_id: 2,
                    size: 5,
                    leb_size: 1,
                    header_len: 2,
                    is_fragment: false,
                },
                vec![0b0011_0100, 0b0111_0000, 0b0000_0101, 1, 2, 3, 4, 5],
                vec![0b0011_0110, 0b0111_0000, 0b0000_0101, 1, 2, 3, 4, 5],
            )
        ];

        let element = <RTPAv1Depay as ObjectSubclass>::Type::new();
        for (idx, (obu, rtp_bytes, out_bytes)) in test_data.into_iter().enumerate() {
            println!("running test {}...", idx);
            let mut reader = Cursor::new(rtp_bytes.as_slice());

            let actual = translate_obu(&element, &mut reader, &obu);
            assert_eq!(reader.position(), rtp_bytes.len() as u64);
            assert!(actual.is_some());

            let actual = actual
                .unwrap()
                .into_mapped_buffer_readable()
                .unwrap();
            assert_eq!(actual.as_slice(), out_bytes.as_slice());
        }
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn test_find_element_info() {
        gst::init().unwrap();

        let test_data: [(Vec<(u32, bool)>, u32, Vec<u8>, AggregationHeader); 4] = [
            (
                vec![(1, false)],   // expected results
                100,                // RTP payload size
                vec![0b0000_0001, 0b0001_0000],
                AggregationHeader { obu_count: None, ..AggregationHeader::default() },
            ), (
                vec![(5, true)],
                5,
                vec![0b0111_1000, 0, 0, 0, 0],
                AggregationHeader { obu_count: Some(1), ..AggregationHeader::default() },
            ), (
                vec![(7, true)],
                8,
                vec![0b0000_0111, 0b0011_0110, 0b0010_1000, 0b0000_1010, 1, 2, 3, 4],
                AggregationHeader { obu_count: None, ..AggregationHeader::default() },
            ), (
                vec![(6, false), (4, true)],
                11,
                vec![0b0000_0110, 0b0111_1000, 1, 2, 3, 4, 5, 0b0011_0000, 1, 2, 3],
                AggregationHeader { obu_count: Some(2), ..AggregationHeader::default() },
            )
        ];

        let element = <RTPAv1Depay as ObjectSubclass>::Type::new();
        for (idx, (
            info,
            payload_size,
            rtp_bytes,
            aggr_header,
        )) in test_data.into_iter().enumerate() {
            println!("running test {}...", idx);
            let buffer = gst::Buffer::new_rtp_with_sizes(payload_size, 0, 0).unwrap();
            let rtp = gst_rtp::RTPBuffer::from_buffer_readable(&buffer).unwrap();
            let mut reader = Cursor::new(rtp_bytes.as_slice());

            let mut element_size = 0;
            for (obu_idx, expected) in info.into_iter().enumerate() {
                if element_size != 0 {
                    reader.seek(SeekFrom::Current(element_size as i64)).unwrap();
                }

                println!("testing element {} with reader position {}...", obu_idx, reader.position());

                let actual = find_element_info(&element, &rtp, &mut reader, &aggr_header, obu_idx as u32);
                assert_eq!(actual, Some(expected));
                element_size = actual.unwrap().0;
            }
        }
    }
}
