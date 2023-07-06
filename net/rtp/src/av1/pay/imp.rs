//
// Copyright (C) 2022 Vivienne Watermeier <vwatermeier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, subclass::prelude::*};
use gst_rtp::{prelude::*, subclass::prelude::*};
use std::{
    cmp,
    collections::VecDeque,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    sync::Mutex,
};

use bitstream_io::{BitReader, BitWriter};
use gst::glib::once_cell::sync::Lazy;

use crate::av1::common::{
    err_flow, leb128_size, write_leb128, ObuType, SizedObu, CLOCK_RATE, ENDIANNESS,
};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rtpav1pay",
        gst::DebugColorFlags::empty(),
        Some("RTP AV1 Payloader"),
    )
});

// TODO: properly handle `max_ptime` and `min_ptime`

/// Information about the OBUs intended to be grouped into one packet
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct PacketOBUData {
    obu_count: usize,
    payload_size: u32,
    last_obu_fragment_size: Option<u32>,
    omit_last_size_field: bool,
    ends_temporal_unit: bool,
}

impl Default for PacketOBUData {
    fn default() -> Self {
        PacketOBUData {
            payload_size: 1, // 1 byte is used for the aggregation header
            omit_last_size_field: true,
            obu_count: 0,
            last_obu_fragment_size: None,
            ends_temporal_unit: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ObuData {
    info: SizedObu,
    bytes: Vec<u8>,
    offset: usize,
    dts: Option<gst::ClockTime>,
    pts: Option<gst::ClockTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    /// Holds header information and raw bytes for all received OBUs,
    /// as well as DTS and PTS
    obus: VecDeque<ObuData>,

    /// Indicates that the first element in the Buffer is an OBU fragment,
    /// left over from the previous RTP packet
    open_obu_fragment: bool,

    /// Indicates the next constructed packet will be the first in its sequence
    /// (Corresponds to `N` field in the aggregation header)
    first_packet_in_seq: bool,

    /// The last observed DTS if upstream does not provide DTS for each OBU
    last_dts: Option<gst::ClockTime>,
    /// The last observed PTS if upstream does not provide PTS for each OBU
    last_pts: Option<gst::ClockTime>,

    /// If the input is TU or frame aligned.
    framed: bool,
}

#[derive(Debug, Default)]
pub struct RTPAv1Pay {
    state: Mutex<State>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            obus: VecDeque::new(),
            open_obu_fragment: false,
            first_packet_in_seq: true,
            last_dts: None,
            last_pts: None,
            framed: false,
        }
    }
}

impl RTPAv1Pay {
    fn reset(&self, state: &mut State, full: bool) {
        gst::debug!(CAT, imp: self, "resetting state");

        if full {
            *state = State::default();
        } else {
            *state = State {
                framed: state.framed,
                ..State::default()
            };
        }
    }

    /// Parses new OBUs, stores them in the state,
    /// and constructs and sends new RTP packets when appropriate.
    fn handle_new_obus(
        &self,
        state: &mut State,
        data: &[u8],
        marker: bool,
        dts: Option<gst::ClockTime>,
        pts: Option<gst::ClockTime>,
    ) -> Result<gst::BufferList, gst::FlowError> {
        let mut reader = Cursor::new(data);

        while reader.position() < data.len() as u64 {
            let obu_start = reader.position();
            let obu = SizedObu::parse(&mut BitReader::endian(&mut reader, ENDIANNESS))
                .map_err(err_flow!(self, buf_read))?;

            // tile lists and temporal delimiters should not be transmitted,
            // see section 5 of the RTP AV1 spec
            match obu.obu_type {
                // completely ignore tile lists
                ObuType::TileList => {
                    gst::log!(CAT, imp: self, "ignoring tile list OBU");
                    reader
                        .seek(SeekFrom::Current(
                            (obu.header_len + obu.leb_size + obu.size) as i64,
                        ))
                        .map_err(err_flow!(self, buf_read))?;
                }

                // keep these OBUs around for now so we know where temporal units end
                ObuType::TemporalDelimiter => {
                    if obu.size != 0 {
                        gst::element_imp_error!(
                            self,
                            gst::ResourceError::Read,
                            ["temporal delimiter OBUs should have empty payload"]
                        );
                        return Err(gst::FlowError::Error);
                    }
                    state.obus.push_back(ObuData {
                        info: obu,
                        bytes: Vec::new(),
                        offset: 0,
                        dts,
                        pts,
                    });
                }

                _ => {
                    let bytes_total = (obu.header_len + obu.size) as usize;
                    let mut bytes = vec![0; bytes_total];

                    // read header
                    reader
                        .seek(SeekFrom::Start(obu_start))
                        .map_err(err_flow!(self, buf_read))?;
                    reader
                        .read_exact(&mut bytes[0..(obu.header_len as usize)])
                        .map_err(err_flow!(self, buf_read))?;

                    // skip size field
                    bytes[0] &= !2_u8; // set `has_size_field` to 0
                    reader
                        .seek(SeekFrom::Current(obu.leb_size as i64))
                        .map_err(err_flow!(self, buf_read))?;

                    // read OBU bytes
                    reader
                        .read_exact(&mut bytes[(obu.header_len as usize)..bytes_total])
                        .map_err(err_flow!(self, buf_read))?;

                    state.obus.push_back(ObuData {
                        info: obu,
                        bytes,
                        offset: 0,
                        dts,
                        pts,
                    });
                }
            }
        }

        let mut list = gst::BufferList::new();
        {
            let list = list.get_mut().unwrap();
            while let Some(packet_data) = self.consider_new_packet(state, false, marker) {
                let buffer = self.generate_new_packet(state, packet_data)?;
                list.add(buffer);
            }
        }

        Ok(list)
    }

    /// Look at the size the currently stored OBUs would require,
    /// as well as their temporal IDs to decide if it is time to construct a
    /// new packet, and what OBUs to include in it.
    ///
    /// If `true` is passed for `force`, packets of any size will be accepted,
    /// which is used in flushing the last OBUs after receiving an EOS for example.
    ///
    /// If `true` is passed for `marker` then all queued OBUs are considered to finish this TU.
    fn consider_new_packet(
        &self,
        state: &mut State,
        force: bool,
        marker: bool,
    ) -> Option<PacketOBUData> {
        gst::trace!(
            CAT,
            imp: self,
            "{} new packet, currently storing {} OBUs (marker {})",
            if force { "forcing" } else { "considering" },
            state.obus.len(),
            marker,
        );

        let payload_limit = gst_rtp::calc_payload_len(self.obj().mtu(), 0, 0);

        // Create information about the packet that can be created now while iterating over the
        // OBUs and return this if a full packet can indeed be created now.
        let mut packet = PacketOBUData::default();
        let mut pending_bytes = 0;
        let mut required_ids = None::<(u8, u8)>;

        // figure out how many OBUs we can fit into this packet
        for (idx, obu) in state.obus.iter().enumerate() {
            // for OBUs with extension headers, spatial and temporal IDs must be equal
            // to all other such OBUs in the packet
            let matching_obu_ids = |obu: &SizedObu, required_ids: &mut Option<(u8, u8)>| -> bool {
                if let Some((sid, tid)) = *required_ids {
                    sid == obu.spatial_id && tid == obu.temporal_id
                } else {
                    *required_ids = Some((obu.spatial_id, obu.temporal_id));
                    true
                }
            };

            let current = &obu.info;

            // should this packet be finished here?
            if current.obu_type == ObuType::TemporalDelimiter {
                // ignore the temporal delimiter, it is not supposed to be transmitted,
                // it will be skipped later when building the packet
                gst::log!(CAT, imp: self, "ignoring temporal delimiter OBU");

                if packet.obu_count > 0 {
                    if marker {
                        gst::warning!(
                            CAT,
                            imp: self,
                            "Temporal delimited in the middle of a frame"
                        );
                    }

                    packet.ends_temporal_unit = true;
                    if packet.obu_count > 3 {
                        packet.payload_size += pending_bytes;
                        packet.omit_last_size_field = false;
                    }

                    return Some(packet);
                }

                continue;
            } else if packet.payload_size >= payload_limit
                || (packet.obu_count > 0 && current.obu_type == ObuType::SequenceHeader)
                || !matching_obu_ids(current, &mut required_ids)
            {
                if packet.obu_count > 3 {
                    packet.payload_size += pending_bytes;
                    packet.omit_last_size_field = false;
                }
                packet.ends_temporal_unit = marker && idx == state.obus.len() - 1;
                return Some(packet);
            }

            // would the full OBU fit?
            if packet.payload_size + pending_bytes + current.full_size() <= payload_limit {
                packet.obu_count += 1;
                packet.payload_size += current.partial_size() + pending_bytes;
                pending_bytes = current.leb_size;
            }
            // would it fit without the size field?
            else if packet.obu_count < 3
                && packet.payload_size + pending_bytes + current.partial_size() <= payload_limit
            {
                packet.obu_count += 1;
                packet.payload_size += current.partial_size() + pending_bytes;
                packet.ends_temporal_unit = marker && idx == state.obus.len() - 1;

                return Some(packet);
            }
            // otherwise consider putting an OBU fragment
            else {
                let leb_size = if packet.obu_count < 3 {
                    0
                } else {
                    // assume the biggest possible OBU fragment,
                    // so if anything the size field will be smaller than expected
                    leb128_size(payload_limit - packet.payload_size) as u32
                };

                // is there even enough space to bother?
                if packet.payload_size + pending_bytes + leb_size + current.header_len
                    < payload_limit
                {
                    packet.obu_count += 1;
                    packet.last_obu_fragment_size =
                        Some(payload_limit - packet.payload_size - pending_bytes - leb_size);
                    packet.payload_size = payload_limit;
                    packet.omit_last_size_field = leb_size == 0;
                } else if packet.obu_count > 3 {
                    packet.ends_temporal_unit = marker && idx == state.obus.len() - 1;
                    packet.payload_size += pending_bytes;
                }

                return Some(packet);
            }
        }

        if (force || marker) && packet.obu_count > 0 {
            if packet.obu_count > 3 {
                packet.payload_size += pending_bytes;
                packet.omit_last_size_field = false;
            }
            packet.ends_temporal_unit = true;

            Some(packet)
        } else {
            // if we ran out of OBUs with space in the packet to spare, wait a bit longer
            None
        }
    }

    /// Given the information returned by consider_new_packet(), construct and return
    /// new RTP packet, filled with those OBUs.
    fn generate_new_packet(
        &self,
        state: &mut State,
        packet: PacketOBUData,
    ) -> Result<gst::Buffer, gst::FlowError> {
        gst::log!(
            CAT,
            imp: self,
            "constructing new RTP packet with {} OBUs",
            packet.obu_count
        );

        // prepare the outgoing buffer
        let mut outbuf =
            gst::Buffer::new_rtp_with_sizes(packet.payload_size, 0, 0).map_err(|err| {
                gst::element_imp_error!(
                    self,
                    gst::ResourceError::Write,
                    ["Failed to allocate output buffer: {}", err]
                );

                gst::FlowError::Error
            })?;

        {
            // this block enforces that outbuf_mut is dropped before pushing outbuf
            let first_obu = state.obus.front().unwrap();
            if let Some(dts) = first_obu.dts {
                state.last_dts = Some(
                    state
                        .last_dts
                        .map_or(dts, |last_dts| cmp::max(last_dts, dts)),
                );
            }
            if let Some(pts) = first_obu.pts {
                state.last_pts = Some(
                    state
                        .last_pts
                        .map_or(pts, |last_pts| cmp::max(last_pts, pts)),
                );
            }

            let outbuf_mut = outbuf
                .get_mut()
                .expect("Failed to get mutable reference to outbuf");
            outbuf_mut.set_dts(state.last_dts);
            outbuf_mut.set_pts(state.last_pts);

            let mut rtp = gst_rtp::RTPBuffer::from_buffer_writable(outbuf_mut)
                .expect("Failed to create RTPBuffer");
            rtp.set_marker(packet.ends_temporal_unit);

            let payload = rtp
                .payload_mut()
                .expect("Failed to get mutable reference to RTP payload");
            let mut writer = Cursor::new(payload);

            {
                // construct aggregation header
                let w = if packet.omit_last_size_field && packet.obu_count < 4 {
                    packet.obu_count
                } else {
                    0
                };

                let aggr_header: [u8; 1] = [
                    (state.open_obu_fragment as u8) << 7 |                  // Z
                    ((packet.last_obu_fragment_size.is_some()) as u8) << 6 |  // Y
                    (w as u8) << 4 |                                        // W
                    (state.first_packet_in_seq as u8) << 3                  // N
                ; 1];

                writer
                    .write(&aggr_header)
                    .map_err(err_flow!(self, aggr_header_write))?;

                state.first_packet_in_seq = false;
            }

            // append OBUs to the buffer
            for _ in 1..packet.obu_count {
                let obu = loop {
                    let obu = state.obus.pop_front().unwrap();

                    if let Some(dts) = obu.dts {
                        state.last_dts = Some(
                            state
                                .last_dts
                                .map_or(dts, |last_dts| cmp::max(last_dts, dts)),
                        );
                    }
                    if let Some(pts) = obu.pts {
                        state.last_pts = Some(
                            state
                                .last_pts
                                .map_or(pts, |last_pts| cmp::max(last_pts, pts)),
                        );
                    }

                    // Drop temporal delimiter from here
                    if obu.info.obu_type != ObuType::TemporalDelimiter {
                        break obu;
                    }
                };

                write_leb128(
                    &mut BitWriter::endian(&mut writer, ENDIANNESS),
                    obu.info.size + obu.info.header_len,
                )
                .map_err(err_flow!(self, leb_write))?;
                writer
                    .write(&obu.bytes[obu.offset..])
                    .map_err(err_flow!(self, obu_write))?;
            }
            state.open_obu_fragment = false;

            {
                let last_obu = loop {
                    let obu = state.obus.front_mut().unwrap();

                    if let Some(dts) = obu.dts {
                        state.last_dts = Some(
                            state
                                .last_dts
                                .map_or(dts, |last_dts| cmp::max(last_dts, dts)),
                        );
                    }
                    if let Some(pts) = obu.pts {
                        state.last_pts = Some(
                            state
                                .last_pts
                                .map_or(pts, |last_pts| cmp::max(last_pts, pts)),
                        );
                    }

                    // Drop temporal delimiter from here
                    if obu.info.obu_type != ObuType::TemporalDelimiter {
                        break obu;
                    }
                    let _ = state.obus.pop_front().unwrap();
                };

                // do the last OBU separately
                // in this instance `obu_size` includes the header length
                let obu_size = if let Some(size) = packet.last_obu_fragment_size {
                    state.open_obu_fragment = true;
                    size
                } else {
                    last_obu.bytes.len() as u32 - last_obu.offset as u32
                };

                if !packet.omit_last_size_field {
                    write_leb128(&mut BitWriter::endian(&mut writer, ENDIANNESS), obu_size)
                        .map_err(err_flow!(self, leb_write))?;
                }

                // if this OBU is not a fragment, handle it as usual
                if packet.last_obu_fragment_size.is_none() {
                    writer
                        .write(&last_obu.bytes[last_obu.offset..])
                        .map_err(err_flow!(self, obu_write))?;
                    let _ = state.obus.pop_front().unwrap();
                }
                // otherwise write only a slice, and update the element
                // to only contain the unwritten bytes
                else {
                    writer
                        .write(
                            &last_obu.bytes[last_obu.offset..last_obu.offset + obu_size as usize],
                        )
                        .map_err(err_flow!(self, obu_write))?;

                    let new_size = last_obu.bytes.len() as u32 - last_obu.offset as u32 - obu_size;
                    last_obu.info = SizedObu {
                        size: new_size,
                        header_len: 0,
                        leb_size: leb128_size(new_size) as u32,
                        is_fragment: true,
                        ..last_obu.info
                    };
                    last_obu.offset += obu_size as usize;
                }
            }
        }

        gst::log!(
            CAT,
            imp: self,
            "generated RTP packet of size {}",
            outbuf.size()
        );

        Ok(outbuf)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RTPAv1Pay {
    const NAME: &'static str = "GstRtpAv1Pay";
    type Type = super::RTPAv1Pay;
    type ParentType = gst_rtp::RTPBasePayload;
}

impl ObjectImpl for RTPAv1Pay {}

impl GstObjectImpl for RTPAv1Pay {}

impl ElementImpl for RTPAv1Pay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP AV1 payloader",
                "Codec/Payloader/Network/RTP",
                "Payload AV1 as RTP packets",
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
                &gst::Caps::builder("video/x-av1")
                    .field("parsed", true)
                    .field("stream-format", "obu-stream")
                    .field("alignment", gst::List::new(["tu", "frame", "obu"]))
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("payload", gst::IntRange::new(96, 127))
                    .field("clock-rate", CLOCK_RATE as i32)
                    .field("encoding-name", "AV1")
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
            self.reset(&mut state, true);
        }

        let ret = self.parent_change_state(transition);

        if matches!(transition, gst::StateChange::PausedToReady) {
            let mut state = self.state.lock().unwrap();
            self.reset(&mut state, true);
        }

        ret
    }
}

impl RTPBasePayloadImpl for RTPAv1Pay {
    fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp: self, "received caps {caps:?}");

        {
            let mut state = self.state.lock().unwrap();
            let s = caps.structure(0).unwrap();
            match s.get::<&str>("alignment").unwrap() {
                "tu" | "frame" => {
                    state.framed = true;
                }
                _ => {
                    state.framed = false;
                }
            }
        }

        self.obj().set_options("video", true, "AV1", CLOCK_RATE);

        Ok(())
    }

    fn handle_buffer(&self, buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, imp: self, "received buffer of size {}", buffer.size());

        let mut state = self.state.lock().unwrap();

        if buffer.flags().contains(gst::BufferFlags::DISCONT) {
            gst::debug!(CAT, imp: self, "buffer discontinuity");
            self.reset(&mut state, false);
        }

        let dts = buffer.dts();
        let pts = buffer.pts();

        let map = buffer.map_readable().map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Read,
                ["Failed to map buffer readable"]
            );

            gst::FlowError::Error
        })?;

        // Does the buffer finished a full TU?
        let marker = buffer.flags().contains(gst::BufferFlags::MARKER) || state.framed;
        let list = self.handle_new_obus(&mut state, map.as_slice(), marker, dts, pts)?;
        drop(map);
        drop(state);

        if !list.is_empty() {
            self.obj().push_list(list)
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        gst::log!(CAT, imp: self, "sink event: {}", event.type_());

        match event.view() {
            gst::EventView::Eos(_) => {
                // flush all remaining OBUs
                let mut list = gst::BufferList::new();
                {
                    let mut state = self.state.lock().unwrap();
                    let list = list.get_mut().unwrap();

                    while let Some(packet_data) = self.consider_new_packet(&mut state, true, true) {
                        match self.generate_new_packet(&mut state, packet_data) {
                            Ok(buffer) => list.add(buffer),
                            Err(_) => break,
                        }
                    }

                    self.reset(&mut state, false);
                }
                if !list.is_empty() {
                    let _ = self.obj().push_list(list);
                }
            }
            gst::EventView::FlushStop(_) => {
                let mut state = self.state.lock().unwrap();
                self.reset(&mut state, false);
            }
            _ => (),
        }

        self.parent_sink_event(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::av1::common::*;

    #[test]
    fn test_consider_new_packet() {
        gst::init().unwrap();

        let base_obu = SizedObu {
            has_extension: false,
            has_size_field: true,
            leb_size: 1,
            header_len: 1,
            is_fragment: false,
            ..SizedObu::default()
        };

        let input_data = [
            (
                false, // force argument
                State {
                    // payloader state
                    obus: VecDeque::from(vec![
                        ObuData {
                            info: SizedObu {
                                obu_type: ObuType::Padding,
                                size: 3,
                                ..base_obu
                            },
                            bytes: vec![1, 2, 3],
                            ..ObuData::default()
                        },
                        ObuData {
                            info: SizedObu {
                                obu_type: ObuType::Frame,
                                size: 4,
                                ..base_obu
                            },
                            bytes: vec![1, 2, 3, 4],
                            ..ObuData::default()
                        },
                        ObuData {
                            info: SizedObu {
                                obu_type: ObuType::Frame,
                                size: 5,
                                ..base_obu
                            },
                            bytes: vec![1, 2, 3, 4, 5],
                            ..ObuData::default()
                        },
                        ObuData {
                            // last two OBUs should not be counted
                            info: SizedObu {
                                obu_type: ObuType::TemporalDelimiter,
                                size: 0,
                                ..base_obu
                            },
                            ..ObuData::default()
                        },
                        ObuData {
                            info: SizedObu {
                                obu_type: ObuType::Frame,
                                size: 10,
                                ..base_obu
                            },
                            bytes: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
                            ..ObuData::default()
                        },
                    ]),
                    ..State::default()
                },
            ),
            (
                true,
                State {
                    obus: VecDeque::from(vec![
                        ObuData {
                            info: SizedObu {
                                obu_type: ObuType::TemporalDelimiter,
                                size: 0,
                                ..base_obu
                            },
                            ..ObuData::default()
                        },
                        ObuData {
                            info: SizedObu {
                                obu_type: ObuType::Frame,
                                size: 7,
                                ..base_obu
                            },
                            bytes: vec![1, 2, 3, 4, 5, 6, 7],
                            ..ObuData::default()
                        },
                        ObuData {
                            info: SizedObu {
                                obu_type: ObuType::Padding,
                                size: 6,
                                ..base_obu
                            },
                            bytes: vec![1, 2, 3, 4, 5, 6],
                            ..ObuData::default()
                        },
                        ObuData {
                            info: SizedObu {
                                obu_type: ObuType::Frame,
                                size: 9,
                                ..base_obu
                            },
                            bytes: vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
                            ..ObuData::default()
                        },
                        ObuData {
                            info: SizedObu {
                                obu_type: ObuType::Frame,
                                size: 3,
                                ..base_obu
                            },
                            bytes: vec![1, 2, 3],
                            ..ObuData::default()
                        },
                    ]),
                    ..State::default()
                },
            ),
            (
                false,
                State {
                    obus: VecDeque::from(vec![
                        ObuData {
                            info: SizedObu {
                                obu_type: ObuType::TemporalDelimiter,
                                size: 0,
                                ..base_obu
                            },
                            ..ObuData::default()
                        },
                        ObuData {
                            info: SizedObu {
                                obu_type: ObuType::Frame,
                                size: 4,
                                ..base_obu
                            },
                            bytes: vec![1, 2, 3, 4],
                            ..ObuData::default()
                        },
                    ]),
                    ..State::default()
                },
            ),
        ];

        let results = [
            (
                Some(PacketOBUData {
                    obu_count: 3,
                    payload_size: 18,
                    last_obu_fragment_size: None,
                    omit_last_size_field: true,
                    ends_temporal_unit: true,
                }),
                State {
                    obus: VecDeque::from(vec![
                        input_data[0].1.obus[0].clone(),
                        input_data[0].1.obus[1].clone(),
                        input_data[0].1.obus[2].clone(),
                        input_data[0].1.obus[4].clone(),
                    ]),
                    ..input_data[0].1
                },
            ),
            (
                Some(PacketOBUData {
                    obu_count: 4,
                    payload_size: 34,
                    last_obu_fragment_size: None,
                    omit_last_size_field: false,
                    ends_temporal_unit: true,
                }),
                State {
                    obus: {
                        let mut copy = input_data[1].1.obus.clone();
                        copy.pop_front().unwrap();
                        copy
                    },
                    ..input_data[1].1
                },
            ),
            (
                None,
                State {
                    obus: {
                        let mut copy = input_data[2].1.obus.clone();
                        copy.pop_front().unwrap();
                        copy
                    },
                    ..input_data[2].1
                },
            ),
        ];

        let element = <RTPAv1Pay as ObjectSubclass>::Type::new();
        let pay = element.imp();
        for idx in 0..input_data.len() {
            println!("running test {idx}...");

            let mut state = pay.state.lock().unwrap();
            *state = input_data[idx].1.clone();

            assert_eq!(
                pay.consider_new_packet(&mut state, input_data[idx].0, false),
                results[idx].0,
            );
            assert_eq!(
                state
                    .obus
                    .iter()
                    .filter(|o| o.info.obu_type != ObuType::TemporalDelimiter)
                    .cloned()
                    .collect::<Vec<_>>(),
                results[idx].1.obus.iter().cloned().collect::<Vec<_>>()
            );
            assert_eq!(state.open_obu_fragment, results[idx].1.open_obu_fragment);
            assert_eq!(
                state.first_packet_in_seq,
                results[idx].1.first_packet_in_seq
            );
        }
    }
}
