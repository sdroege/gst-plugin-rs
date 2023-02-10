//
// Copyright (C) 2022 Vivienne Watermeier <vwatermeier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, subclass::prelude::*};
use gst_rtp::prelude::*;
use gst_rtp::subclass::prelude::*;
use std::{
    cmp::Ordering,
    io::{Cursor, Read, Seek, SeekFrom},
    sync::Mutex,
};

use bitstream_io::{BitReader, BitWriter};
use once_cell::sync::Lazy;

use crate::av1::common::{
    err_flow, leb128_size, parse_leb128, write_leb128, AggregationHeader, ObuType, SizedObu,
    UnsizedObu, CLOCK_RATE, ENDIANNESS,
};

// TODO: handle internal size fields in RTP OBUs

#[derive(Debug)]
struct State {
    last_timestamp: Option<u32>,
    /// if true, the last packet of a temporal unit has been received
    marked_packet: bool,
    /// if the next output buffer needs the DISCONT flag set
    needs_discont: bool,
    /// holds data for a fragment
    obu_fragment: Option<(UnsizedObu, Vec<u8>)>,
}

impl Default for State {
    fn default() -> Self {
        State {
            last_timestamp: None,
            marked_packet: false,
            needs_discont: true,
            obu_fragment: None,
        }
    }
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

static TEMPORAL_DELIMITER: [u8; 2] = [0b0001_0010, 0];

impl RTPAv1Depay {
    fn reset(&self, state: &mut State) {
        gst::debug!(CAT, imp: self, "resetting state");

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
                    .field("alignment", "obu")
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::debug!(CAT, imp: self, "changing state: {}", transition);

        if matches!(transition, gst::StateChange::ReadyToPaused) {
            let mut state = self.state.lock().unwrap();
            self.reset(&mut state);
        }

        let ret = self.parent_change_state(transition);

        if matches!(transition, gst::StateChange::PausedToReady) {
            let mut state = self.state.lock().unwrap();
            self.reset(&mut state);
        }

        ret
    }
}

impl RTPBaseDepayloadImpl for RTPAv1Depay {
    fn set_caps(&self, _caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let element = self.obj();
        let src_pad = element.src_pad();
        let src_caps = src_pad.pad_template_caps();
        src_pad.push_event(gst::event::Caps::builder(&src_caps).build());

        Ok(())
    }

    fn handle_event(&self, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::Eos(_) | gst::EventView::FlushStop(_) => {
                let mut state = self.state.lock().unwrap();
                self.reset(&mut state);
            }
            _ => (),
        }

        self.parent_handle_event(event)
    }

    fn process_rtp_packet(
        &self,
        rtp: &gst_rtp::RTPBuffer<gst_rtp::rtp_buffer::Readable>,
    ) -> Option<gst::Buffer> {
        if let Err(err) = self.handle_rtp_packet(rtp) {
            gst::warning!(CAT, imp: self, "Failed to handle RTP packet: {err:?}");
            self.reset(&mut self.state.lock().unwrap());
        }

        None
    }
}

impl RTPAv1Depay {
    fn handle_rtp_packet(
        &self,
        rtp: &gst_rtp::RTPBuffer<gst_rtp::rtp_buffer::Readable>,
    ) -> Result<(), gst::FlowError> {
        gst::log!(
            CAT,
            imp: self,
            "processing RTP packet with payload type {} and size {}",
            rtp.payload_type(),
            rtp.buffer().size(),
        );

        let payload = rtp.payload().map_err(err_flow!(self, payload_buf))?;

        let mut state = self.state.lock().unwrap();

        if rtp.buffer().flags().contains(gst::BufferFlags::DISCONT) {
            gst::debug!(CAT, imp: self, "buffer discontinuity");
            self.reset(&mut state);
        }

        let mut reader = Cursor::new(payload);
        let mut ready_obus = Vec::new();

        let aggr_header = {
            let mut byte = [0; 1];
            reader
                .read_exact(&mut byte)
                .map_err(err_flow!(self, aggr_header_read))?;
            AggregationHeader::from(&byte)
        };

        // handle new temporal units
        if state.marked_packet || state.last_timestamp != Some(rtp.timestamp()) {
            if state.last_timestamp.is_some() && state.obu_fragment.is_some() {
                gst::error!(
                    CAT,
                    imp: self,
                    concat!(
                        "invalid packet: packet is part of a new TU but ",
                        "the previous TU still has an incomplete OBU",
                        "marked_packet: {}, last_timestamp: {:?}"
                    ),
                    state.marked_packet,
                    state.last_timestamp
                );
                self.reset(&mut state);
            }

            // the next temporal unit starts with a temporal delimiter OBU
            ready_obus.extend_from_slice(&TEMPORAL_DELIMITER);
        }
        state.marked_packet = rtp.is_marker();
        state.last_timestamp = Some(rtp.timestamp());

        // parse and prepare the received OBUs
        let mut idx = 0;

        // handle leading OBU fragment
        if state.obu_fragment.is_some() && !aggr_header.leading_fragment {
            gst::error!(
                CAT,
                imp: self,
                "invalid packet: dropping unclosed OBU fragment"
            );
            self.reset(&mut state);
        }

        if let Some((obu, ref mut bytes)) = &mut state.obu_fragment {
            assert!(aggr_header.leading_fragment);
            let (element_size, is_last_obu) = self
                .find_element_info(rtp, &mut reader, &aggr_header, idx)
                .map_err(err_flow!(self, find_element))?;

            let bytes_end = bytes.len();
            bytes.resize(bytes_end + element_size as usize, 0);
            reader
                .read_exact(&mut bytes[bytes_end..])
                .map_err(err_flow!(self, buf_read))?;

            // if this OBU is complete, it can be appended to the adapter
            if !(is_last_obu && aggr_header.trailing_fragment) {
                let full_obu = {
                    let size = bytes.len() as u32 - obu.header_len;
                    let leb_size = leb128_size(size) as u32;
                    obu.as_sized(size, leb_size)
                };

                self.translate_obu(
                    &mut Cursor::new(bytes.as_slice()),
                    &full_obu,
                    &mut ready_obus,
                )?;
                state.obu_fragment = None;
            }

            idx += 1;
        }

        // handle other OBUs, including trailing fragments
        while reader.position() < rtp.payload_size() as u64 {
            let (element_size, is_last_obu) =
                self.find_element_info(rtp, &mut reader, &aggr_header, idx)?;

            let header_pos = reader.position();
            let mut bitreader = BitReader::endian(&mut reader, ENDIANNESS);
            let obu = UnsizedObu::parse(&mut bitreader).map_err(err_flow!(self, obu_read))?;

            reader
                .seek(SeekFrom::Start(header_pos))
                .map_err(err_flow!(self, buf_read))?;

            // ignore these OBU types
            if matches!(obu.obu_type, ObuType::TemporalDelimiter | ObuType::TileList) {
                reader
                    .seek(SeekFrom::Current(element_size as i64))
                    .map_err(err_flow!(self, buf_read))?;
                idx += 1;
                continue;
            }

            // trailing OBU fragments are stored in the state
            if is_last_obu && aggr_header.trailing_fragment {
                let bytes_left = rtp.payload_size() - (reader.position() as u32);
                let mut bytes = vec![0; bytes_left as usize];
                reader
                    .read_exact(bytes.as_mut_slice())
                    .map_err(err_flow!(self, buf_read))?;

                state.obu_fragment = Some((obu, bytes));
            }
            // full OBUs elements are translated and appended to the ready OBUs
            else {
                let full_obu = {
                    let size = element_size - obu.header_len;
                    let leb_size = leb128_size(size) as u32;
                    obu.as_sized(size, leb_size)
                };

                self.translate_obu(&mut reader, &full_obu, &mut ready_obus)?;
            }

            idx += 1;
        }

        // now push all the complete OBUs
        let buffer = if !ready_obus.is_empty() {
            gst::log!(
                CAT,
                imp: self,
                "Creating buffer containing {} bytes of data (marker {}, discont {})...",
                ready_obus.len(),
                state.marked_packet,
                state.needs_discont,
            );

            let mut buffer = gst::Buffer::from_mut_slice(ready_obus);
            {
                let buffer = buffer.get_mut().unwrap();
                if state.marked_packet {
                    buffer.set_flags(gst::BufferFlags::MARKER);
                }
                if state.needs_discont {
                    buffer.set_flags(gst::BufferFlags::DISCONT);
                    state.needs_discont = false;
                }
            }

            Some(buffer)
        } else {
            None
        };

        // It's important to check this after the packet was created as otherwise
        // the discont flag is already before the missing data.
        if state.marked_packet && state.obu_fragment.is_some() {
            gst::error!(
                CAT,
                imp: self,
                concat!(
                    "invalid packet: has marker bit set, but ",
                    "last OBU is not yet complete. Dropping incomplete OBU."
                )
            );
            self.reset(&mut state);
        }
        drop(state);

        if let Some(buffer) = buffer {
            self.obj().push(buffer)?;
        }

        Ok(())
    }

    /// Find out the next OBU element's size, and if it is the last OBU in the packet.
    /// The reader is expected to be at the first byte of the element,
    /// or its preceding size field if present,
    /// and will be at the first byte past the element's size field afterwards.
    fn find_element_info(
        &self,
        rtp: &gst_rtp::RTPBuffer<gst_rtp::rtp_buffer::Readable>,
        reader: &mut Cursor<&[u8]>,
        aggr_header: &AggregationHeader,
        index: u32,
    ) -> Result<(u32, bool), gst::FlowError> {
        let is_last_obu: bool;

        let element_size = if let Some(count) = aggr_header.obu_count {
            is_last_obu = index + 1 == count as u32;
            if is_last_obu {
                rtp.payload_size() - (reader.position() as u32)
            } else {
                let mut bitreader = BitReader::endian(reader, ENDIANNESS);
                let (size, _) = parse_leb128(&mut bitreader).map_err(err_flow!(self, leb_read))?;
                size
            }
        } else {
            let (size, _) = parse_leb128(&mut BitReader::endian(&mut *reader, ENDIANNESS))
                .map_err(err_flow!(self, leb_read))?;
            is_last_obu = match rtp.payload_size().cmp(&(reader.position() as u32 + size)) {
                Ordering::Greater => false,
                Ordering::Equal => true,
                Ordering::Less => {
                    gst::error!(
                        CAT,
                        imp: self,
                        "invalid packet: size field gives impossibly large OBU size"
                    );
                    return Err(gst::FlowError::Error);
                }
            };
            size
        };

        Ok((element_size, is_last_obu))
    }

    /// Using OBU data from an RTP packet, construct a buffer containing that OBU in AV1 bitstream format
    fn translate_obu(
        &self,
        reader: &mut Cursor<&[u8]>,
        obu: &SizedObu,
        w: &mut Vec<u8>,
    ) -> Result<(), gst::FlowError> {
        let pos = w.len();
        w.resize(pos + obu.full_size() as usize, 0);
        let bytes = &mut w[pos..];

        // write OBU header
        reader
            .read_exact(&mut bytes[..obu.header_len as usize])
            .map_err(err_flow!(self, buf_read))?;

        // set `has_size_field`
        bytes[0] |= 1 << 1;

        // skip internal size field if present
        if obu.has_size_field {
            parse_leb128(&mut BitReader::endian(&mut *reader, ENDIANNESS))
                .map_err(err_flow!(self, leb_read))?;
        }

        // write size field
        write_leb128(
            &mut BitWriter::endian(
                Cursor::new(&mut bytes[obu.header_len as usize..]),
                ENDIANNESS,
            ),
            obu.size,
        )
        .map_err(err_flow!(self, leb_write))?;

        // write OBU payload
        reader
            .read_exact(&mut bytes[(obu.header_len + obu.leb_size) as usize..])
            .map_err(err_flow!(self, buf_read))?;

        Ok(())
    }
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use super::*;
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
            println!("running test {idx}...");
            let mut reader = Cursor::new(rtp_bytes.as_slice());

            let mut actual = Vec::new();
            element.imp().translate_obu(&mut reader, &obu, &mut actual).unwrap();
            assert_eq!(reader.position(), rtp_bytes.len() as u64);

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
            println!("running test {idx}...");
            let buffer = gst::Buffer::new_rtp_with_sizes(payload_size, 0, 0).unwrap();
            let rtp = gst_rtp::RTPBuffer::from_buffer_readable(&buffer).unwrap();
            let mut reader = Cursor::new(rtp_bytes.as_slice());

            let mut element_size = 0;
            for (obu_idx, expected) in info.into_iter().enumerate() {
                if element_size != 0 {
                    reader.seek(SeekFrom::Current(element_size as i64)).unwrap();
                }

                println!("testing element {} with reader position {}...", obu_idx, reader.position());

                let actual = element.imp().find_element_info(&rtp, &mut reader, &aggr_header, obu_idx as u32);
                assert_eq!(actual, Ok(expected));
                element_size = actual.unwrap().0;
            }
        }
    }
}
