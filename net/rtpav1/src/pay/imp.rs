//
// Copyright (C) 2022 Vivienne Watermeier <vwatermeier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{
    glib,
    subclass::{prelude::*, ElementMetadata},
    Buffer, BufferFlags, Caps, ClockTime, DebugCategory, DebugColorFlags, Event, EventType,
    FlowError, FlowSuccess, IntRange, LoggableError, PadDirection, PadPresence, PadTemplate,
    ResourceError, StateChange, StateChangeError, StateChangeSuccess,
};
use gst_rtp::{prelude::*, rtp_buffer::RTPBuffer, subclass::prelude::*, RTPBasePayload};
use std::{
    io::{Cursor, Read, Seek, SeekFrom, Write},
    sync::{Mutex, MutexGuard},
};

use bitstream_io::{BitReader, BitWriter};
use once_cell::sync::Lazy;

use crate::common::{
    err_flow, leb128_size, write_leb128, ObuType, SizedObu, CLOCK_RATE, ENDIANNESS,
};

static CAT: Lazy<DebugCategory> = Lazy::new(|| {
    DebugCategory::new(
        "rtpav1pay",
        DebugColorFlags::empty(),
        Some("RTP AV1 Payloader"),
    )
});

// TODO: properly handle `max_ptime` and `min_ptime`

/// Information about the OBUs intended to be grouped into one packet
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct PacketOBUData {
    obu_count: usize,
    payload_size: u32,
    last_obu_fragment_size: Option<u32>,
    omit_last_size_field: bool,
    ends_temporal_unit: bool,
}

/// Temporary information held between invocations of `consider_new_packet()`
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct TempPacketData {
    payload_limit: u32,
    required_ids: Option<(u8, u8)>,
    /// bytes used for an OBUs size field will only be added to the total
    /// once its known for sure it will be placed in the packet
    pending_bytes: u32,
    packet: PacketOBUData,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ObuData {
    info: SizedObu,
    bytes: Vec<u8>,
    dts: Option<ClockTime>,
    pts: Option<ClockTime>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    /// Holds header information and raw bytes for all received OBUs,
    /// as well as DTS and PTS
    //obus: Vec<(SizedObu, Vec<u8>, Option<ClockTime>, Option<ClockTime>)>,
    obus: Vec<ObuData>,

    /// Indicates that the first element in the Buffer is an OBU fragment,
    /// left over from the previous RTP packet
    open_obu_fragment: bool,

    /// Indicates the next constructed packet will be the first in its sequence
    /// (Corresponds to `N` field in the aggregation header)
    first_packet_in_seq: bool,

    temp_packet_data: Option<TempPacketData>,
}

#[derive(Debug, Default)]
pub struct RTPAv1Pay {
    state: Mutex<State>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            obus: Vec::new(),
            open_obu_fragment: false,
            first_packet_in_seq: true,
            temp_packet_data: None,
        }
    }
}

impl RTPAv1Pay {
    fn reset(&self, element: &<Self as ObjectSubclass>::Type, state: &mut State) {
        gst::debug!(CAT, obj: element, "resetting state");

        state.obus.clear();
    }

    /// Parses new OBUs, stores them in the state,
    /// and constructs and sends new RTP packets when appropriate.
    fn handle_new_obus<'s>(
        &'s self,
        element: &<Self as ObjectSubclass>::Type,
        state: &mut MutexGuard<'s, State>,
        data: &[u8],
        dts: Option<ClockTime>,
        pts: Option<ClockTime>,
    ) -> Result<FlowSuccess, FlowError> {
        let mut reader = Cursor::new(data);

        while reader.position() < data.len() as u64 {
            let obu_start = reader.position();
            let obu = SizedObu::parse(&mut BitReader::endian(&mut reader, ENDIANNESS))
                .map_err(err_flow!(element, buf_read))?;

            // tile lists and temporal delimiters should not be transmitted,
            // see section 5 of the RTP AV1 spec
            match obu.obu_type {
                // completely ignore tile lists
                ObuType::TileList => {
                    gst::log!(CAT, obj: element, "ignoring tile list OBU");
                    reader
                        .seek(SeekFrom::Current(
                            (obu.header_len + obu.leb_size + obu.size) as i64,
                        ))
                        .map_err(err_flow!(element, buf_read))?;
                }

                // keep these OBUs around for now so we know where temporal units end
                ObuType::TemporalDelimiter => {
                    if obu.size != 0 {
                        gst::element_error!(
                            element,
                            ResourceError::Read,
                            ["temporal delimiter OBUs should have empty payload"]
                        );
                        return Err(FlowError::Error);
                    }
                    state.obus.push(ObuData {
                        info: obu,
                        bytes: Vec::new(),
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
                        .map_err(err_flow!(element, buf_read))?;
                    reader
                        .read_exact(&mut bytes[0..(obu.header_len as usize)])
                        .map_err(err_flow!(element, buf_read))?;

                    // skip size field
                    bytes[0] &= !2_u8; // set `has_size_field` to 0
                    reader
                        .seek(SeekFrom::Current(obu.leb_size as i64))
                        .map_err(err_flow!(element, buf_read))?;

                    // read OBU bytes
                    reader
                        .read_exact(&mut bytes[(obu.header_len as usize)..bytes_total])
                        .map_err(err_flow!(element, buf_read))?;

                    state.obus.push(ObuData {
                        info: obu,
                        bytes,
                        dts,
                        pts,
                    });
                }
            }
        }

        while let Some(packet_data) = self.consider_new_packet(element, state, false) {
            self.push_new_packet(element, state, packet_data)?;
        }

        Ok(FlowSuccess::Ok)
    }

    /// Look at the size the currently stored OBUs would require,
    /// as well as their temportal IDs to decide if it is time to construct a
    /// new packet, and what OBUs to include in it.
    ///
    /// If `true` is passed for `force`, packets of any size will be accepted,
    /// which is used in flushing the last OBUs after receiving an EOS for example.
    fn consider_new_packet(
        &self,
        element: &<Self as ObjectSubclass>::Type,
        state: &mut State,
        force: bool,
    ) -> Option<PacketOBUData> {
        gst::trace!(
            CAT,
            obj: element,
            "{} new packet, currently storing {} OBUs",
            if force { "forcing" } else { "considering" },
            state.obus.len()
        );

        let mut data = state.temp_packet_data.take().unwrap_or_else(|| {
            TempPacketData {
                payload_limit: RTPBuffer::calc_payload_len(element.mtu(), 0, 0),
                packet: PacketOBUData {
                    payload_size: 1, // 1 byte is used for the aggregation header
                    omit_last_size_field: true,
                    ..PacketOBUData::default()
                },
                ..TempPacketData::default()
            }
        });
        let mut packet = data.packet;

        // figure out how many OBUs we can fit into this packet
        while packet.obu_count < state.obus.len() {
            // for OBUs with extension headers, spatial and temporal IDs must be equal
            // to all other such OBUs in the packet
            let matching_obu_ids = |obu: &SizedObu, data: &mut TempPacketData| -> bool {
                if let Some((sid, tid)) = data.required_ids {
                    sid == obu.spatial_id && tid == obu.temporal_id
                } else {
                    data.required_ids = Some((obu.spatial_id, obu.temporal_id));
                    true
                }
            };

            let current = state.obus[packet.obu_count].info;

            // should this packet be finished here?
            if current.obu_type == ObuType::TemporalDelimiter {
                // remove the temporal delimiter, it is not supposed to be transmitted
                gst::log!(CAT, obj: element, "ignoring temporal delimiter OBU");
                state.obus.remove(packet.obu_count);

                if packet.obu_count > 0 {
                    packet.ends_temporal_unit = true;
                    if packet.obu_count > 3 {
                        packet.payload_size += data.pending_bytes;
                        packet.omit_last_size_field = false;
                    }
                    return Some(packet);
                } else {
                    continue;
                }
            } else if packet.payload_size >= data.payload_limit
                || (packet.obu_count > 0 && current.obu_type == ObuType::SequenceHeader)
                || !matching_obu_ids(&state.obus[packet.obu_count].info, &mut data)
            {
                if packet.obu_count > 3 {
                    packet.payload_size += data.pending_bytes;
                    packet.omit_last_size_field = false;
                }
                return Some(packet);
            }

            // would the full OBU fit?
            if packet.payload_size + data.pending_bytes + current.full_size() <= data.payload_limit
            {
                packet.obu_count += 1;
                packet.payload_size += current.partial_size() + data.pending_bytes;
                data.pending_bytes = current.leb_size;
            }
            // would it fit without the size field?
            else if packet.obu_count < 3
                && packet.payload_size + data.pending_bytes + current.partial_size()
                    <= data.payload_limit
            {
                packet.obu_count += 1;
                packet.payload_size += current.partial_size() + data.pending_bytes;

                return Some(packet);
            }
            // otherwise consider putting an OBU fragment
            else {
                let leb_size = if packet.obu_count < 3 {
                    0
                } else {
                    // assume the biggest possible OBU fragment,
                    // so if anything the size field will be smaller than expected
                    leb128_size(data.payload_limit - packet.payload_size) as u32
                };

                // is there even enough space to bother?
                if packet.payload_size + data.pending_bytes + leb_size + current.header_len
                    < data.payload_limit
                {
                    packet.obu_count += 1;
                    packet.last_obu_fragment_size = Some(
                        data.payload_limit - packet.payload_size - data.pending_bytes - leb_size,
                    );
                    packet.payload_size = data.payload_limit;
                    packet.omit_last_size_field = leb_size == 0;
                } else if packet.obu_count > 3 {
                    packet.payload_size += data.pending_bytes;
                }

                return Some(packet);
            }
        }

        if force && packet.obu_count > 0 {
            if packet.obu_count > 3 {
                packet.payload_size += data.pending_bytes;
                packet.omit_last_size_field = false;
            }
            Some(packet)
        } else {
            // if we ran out of OBUs with space in the packet to spare, wait a bit longer
            data.packet = packet;
            state.temp_packet_data = Some(data);
            None
        }
    }

    /// Given the information returned by consider_new_packet(), construct and push
    /// new RTP packet, filled with those OBUs.
    fn push_new_packet<'s>(
        &'s self,
        element: &<Self as ObjectSubclass>::Type,
        state: &mut MutexGuard<'s, State>,
        packet: PacketOBUData,
    ) -> Result<FlowSuccess, FlowError> {
        gst::log!(
            CAT,
            obj: element,
            "constructing new RTP packet with {} OBUs",
            packet.obu_count
        );

        // prepare the outgoing buffer
        let mut outbuf = Buffer::new_rtp_with_sizes(packet.payload_size, 0, 0).map_err(|err| {
            gst::element_error!(
                element,
                ResourceError::Write,
                ["Failed to allocate output buffer: {}", err]
            );

            FlowError::Error
        })?;

        {
            // this block enforces that outbuf_mut is dropped before pushing outbuf
            let outbuf_mut = outbuf
                .get_mut()
                .expect("Failed to get mutable reference to outbuf");
            outbuf_mut.set_dts(state.obus[0].dts);
            outbuf_mut.set_pts(state.obus[0].pts);

            let mut rtp =
                RTPBuffer::from_buffer_writable(outbuf_mut).expect("Failed to create RTPBuffer");
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
                    ((packet.last_obu_fragment_size != None) as u8) << 6 |  // Y
                    (w as u8) << 4 |                                        // W
                    (state.first_packet_in_seq as u8) << 3                  // N
                ; 1];

                writer
                    .write(&aggr_header)
                    .map_err(err_flow!(element, aggr_header_write))?;

                state.first_packet_in_seq = false;
            }

            // append OBUs to the buffer
            for _ in 1..packet.obu_count {
                let obu = &state.obus[0];

                write_leb128(
                    &mut BitWriter::endian(&mut writer, ENDIANNESS),
                    obu.info.size + obu.info.header_len,
                )
                .map_err(err_flow!(element, leb_write))?;
                writer
                    .write(&obu.bytes)
                    .map_err(err_flow!(element, obu_write))?;

                state.obus.remove(0);
            }
            state.open_obu_fragment = false;

            {
                // do the last OBU separately
                // in this instance `obu_size` includes the header length
                let obu_size = if let Some(size) = packet.last_obu_fragment_size {
                    state.open_obu_fragment = true;
                    size
                } else {
                    state.obus[0].bytes.len() as u32
                };

                if !packet.omit_last_size_field {
                    write_leb128(&mut BitWriter::endian(&mut writer, ENDIANNESS), obu_size)
                        .map_err(err_flow!(element, leb_write))?;
                }

                // if this OBU is not a fragment, handle it as usual
                if packet.last_obu_fragment_size == None {
                    writer
                        .write(&state.obus[0].bytes)
                        .map_err(err_flow!(element, obu_write))?;
                    state.obus.remove(0);
                }
                // otherwise write only a slice, and update the element
                // to only contain the unwritten bytes
                else {
                    writer
                        .write(&state.obus[0].bytes[0..obu_size as usize])
                        .map_err(err_flow!(element, obu_write))?;

                    let new_size = state.obus[0].bytes.len() as u32 - obu_size;
                    state.obus[0] = ObuData {
                        info: SizedObu {
                            size: new_size,
                            header_len: 0,
                            leb_size: leb128_size(new_size) as u32,
                            is_fragment: true,
                            ..state.obus[0].info
                        },
                        bytes: Vec::from(
                            &state.obus[0].bytes[obu_size as usize..state.obus[0].bytes.len()],
                        ),
                        ..state.obus[0]
                    };
                }
            }
        }

        gst::log!(
            CAT,
            obj: element,
            "pushing RTP packet of size {}",
            outbuf.size()
        );
        element.push(outbuf)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RTPAv1Pay {
    const NAME: &'static str = "GstRtpAv1Pay";
    type Type = super::RTPAv1Pay;
    type ParentType = RTPBasePayload;
}

impl ObjectImpl for RTPAv1Pay {}

impl GstObjectImpl for RTPAv1Pay {}

impl ElementImpl for RTPAv1Pay {
    fn metadata() -> Option<&'static ElementMetadata> {
        static ELEMENT_METADATA: Lazy<ElementMetadata> = Lazy::new(|| {
            ElementMetadata::new(
                "RTP AV1 payloader",
                "Codec/Payloader/Network/RTP",
                "Payload AV1 as RTP packets",
                "Vivienne Watermeier <vwatermeier@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<PadTemplate>> = Lazy::new(|| {
            let sink_pad_template = PadTemplate::new(
                "sink",
                PadDirection::Sink,
                PadPresence::Always,
                &Caps::builder("video/x-av1")
                    .field("parsed", true)
                    .field("stream-format", "obu-stream")
                    .field("alignment", "obu")
                    .build(),
            )
            .unwrap();

            let src_pad_template = PadTemplate::new(
                "src",
                PadDirection::Src,
                PadPresence::Always,
                &Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("payload", IntRange::new(96, 127))
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
        element: &Self::Type,
        transition: StateChange,
    ) -> Result<StateChangeSuccess, StateChangeError> {
        gst::debug!(CAT, obj: element, "changing state: {}", transition);

        if matches!(transition, StateChange::ReadyToPaused) {
            let mut state = self.state.lock().unwrap();
            self.reset(element, &mut state);
        }

        let ret = self.parent_change_state(element, transition);

        if matches!(transition, StateChange::PausedToReady) {
            let mut state = self.state.lock().unwrap();
            self.reset(element, &mut state);
        }

        ret
    }
}

impl RTPBasePayloadImpl for RTPAv1Pay {
    fn set_caps(&self, element: &Self::Type, _caps: &Caps) -> Result<(), LoggableError> {
        element.set_options("video", true, "AV1", CLOCK_RATE);

        gst::debug!(CAT, obj: element, "setting caps");

        Ok(())
    }

    fn handle_buffer(
        &self,
        element: &Self::Type,
        buffer: Buffer,
    ) -> Result<FlowSuccess, FlowError> {
        gst::trace!(
            CAT,
            obj: element,
            "received buffer of size {}",
            buffer.size()
        );

        let mut state = self.state.lock().unwrap();

        if buffer.flags().contains(BufferFlags::DISCONT) {
            gst::debug!(CAT, obj: element, "buffer discontinuity");
            self.reset(element, &mut state);
        }

        let dts = buffer.dts();
        let pts = buffer.pts();

        let buffer = buffer.into_mapped_buffer_readable().map_err(|_| {
            gst::element_error!(
                element,
                ResourceError::Read,
                ["Failed to map buffer readable"]
            );

            FlowError::Error
        })?;

        self.handle_new_obus(element, &mut state, buffer.as_slice(), dts, pts)
    }

    fn sink_event(&self, element: &Self::Type, event: Event) -> bool {
        gst::log!(CAT, obj: element, "sink event: {}", event.type_());

        if matches!(event.type_(), EventType::Eos) {
            let mut state = self.state.lock().unwrap();

            // flush all remaining OBUs
            while let Some(packet_data) = self.consider_new_packet(element, &mut state, true) {
                if self
                    .push_new_packet(element, &mut state, packet_data)
                    .is_err()
                {
                    break;
                }
            }
        }

        self.parent_sink_event(element, event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::*;

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
                    obus: vec![
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
                    ],
                    ..State::default()
                },
            ),
            (
                true,
                State {
                    obus: vec![
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
                    ],
                    ..State::default()
                },
            ),
            (
                false,
                State {
                    obus: vec![ObuData {
                        info: SizedObu {
                            obu_type: ObuType::Frame,
                            size: 4,
                            ..base_obu
                        },
                        bytes: vec![1, 2, 3, 4],
                        ..ObuData::default()
                    }],
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
                    obus: vec![
                        input_data[0].1.obus[0].clone(),
                        input_data[0].1.obus[1].clone(),
                        input_data[0].1.obus[2].clone(),
                        input_data[0].1.obus[4].clone(),
                    ],
                    ..input_data[0].1
                },
            ),
            (
                Some(PacketOBUData {
                    obu_count: 4,
                    payload_size: 34,
                    last_obu_fragment_size: None,
                    omit_last_size_field: false,
                    ends_temporal_unit: false,
                }),
                State {
                    obus: input_data[1].1.obus[1..].to_owned(),
                    ..input_data[1].1
                },
            ),
            (None, input_data[2].1.clone()),
        ];

        let element = <RTPAv1Pay as ObjectSubclass>::Type::new();
        let pay = element.imp();
        for idx in 0..input_data.len() {
            println!("running test {}...", idx);

            let mut state = pay.state.lock().unwrap();
            *state = input_data[idx].1.clone();

            assert_eq!(
                pay.consider_new_packet(&element, &mut state, input_data[idx].0),
                results[idx].0,
            );
            assert_eq!(state.obus, results[idx].1.obus);
            assert_eq!(state.open_obu_fragment, results[idx].1.open_obu_fragment);
            assert_eq!(
                state.first_packet_in_seq,
                results[idx].1.first_packet_in_seq
            );
        }
    }
}
