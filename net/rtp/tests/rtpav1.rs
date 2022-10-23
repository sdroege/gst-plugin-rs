//
// Copyright (C) 2022 Vivienne Watermeier <vwatermeier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{event::Eos, prelude::*, Buffer, Caps, ClockTime};
use gst_check::Harness;
use gst_rtp::{rtp_buffer::RTPBufferExt, RTPBuffer};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsrtp::plugin_register_static().expect("rtpav1 test");
    });
}

#[test]
#[rustfmt::skip]
fn test_depayloader() {
    let test_packets: [(Vec<u8>, bool, u32); 4] = [
        ( // simple packet, complete TU
            vec![       // RTP payload
                0b0001_1000,
                0b0011_0000, 1, 2, 3, 4, 5, 6,
            ],
            true,       // marker bit
            100_000,    // timestamp
        ), ( // 2 OBUs, last is fragmented
            vec![
                0b0110_0000,
                0b0000_0110, 0b0111_1000, 1, 2, 3, 4, 5,
                             0b0011_0000, 1, 2, 3,
            ],
            false,
            190_000,
        ), ( // continuation of the last OBU
            vec![
                0b1100_0000,
                0b0000_0100, 4, 5, 6, 7,
            ],
            false,
            190_000,
        ), ( // finishing the OBU fragment
            vec![
                0b1001_0000,
                8, 9, 10,
            ],
            true,
            190_000,
        )
    ];

    let expected: [Vec<u8>; 2] = [
        vec![
            0b0001_0010, 0,
            0b0011_0010, 0b0000_0110, 1, 2, 3, 4, 5, 6,
        ],
        vec![
            0b0001_0010, 0,
            0b0111_1010, 0b0000_0101, 1, 2, 3, 4, 5,
            0b0011_0010, 0b0000_1010, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10,
        ],
    ];

    init();

    let mut h = Harness::new("rtpav1depay");
    h.play();

    let caps = Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("payload", 96)
        .field("clock-rate", 90000)
        .field("encoding-name", "AV1")
        .build();
    h.set_src_caps(caps);

    for (idx, (bytes, marker, timestamp)) in test_packets.iter().enumerate() {
        let mut buf = Buffer::new_rtp_with_sizes(bytes.len() as u32, 0, 0).unwrap();
        {
            let buf_mut = buf.get_mut().unwrap();
            let mut rtp_mut = RTPBuffer::from_buffer_writable(buf_mut).unwrap();
            rtp_mut.set_marker(*marker);
            rtp_mut.set_timestamp(*timestamp);
            rtp_mut.set_payload_type(96);
            rtp_mut.set_seq(idx as u16);
            rtp_mut.payload_mut().unwrap().copy_from_slice(bytes);
        }

        h.push(buf).unwrap();
    }
    h.push_event(Eos::new());

    for (idx, ex) in expected.iter().enumerate() {
        println!("checking buffer {}...", idx);

        let buffer = h.pull().unwrap();
        let actual = buffer.into_mapped_buffer_readable().unwrap();
        assert_eq!(actual.as_slice(), ex.as_slice());
    }
}

#[test]
#[rustfmt::skip]
fn test_payloader() {
    let test_buffers: [(u64, Vec<u8>); 3] = [
        (
            0,
            vec![   // this should result in exactly 25 bytes for the RTP payload
                0b0001_0010, 0,
                0b0011_0010, 0b0000_1100, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
                0b0011_0010, 0b0000_1001, 1, 2, 3, 4, 5, 6, 7, 8, 9,
            ],
        ), (
            0,
            vec![   // these all have to go in separate packets since their IDs mismatch
                0b0111_1010, 0b0000_0100, 1, 2, 3, 4,
                0b0011_0110, 0b0010_1000, 0b0000_0101, 1, 2, 3, 4, 5,
                0b0011_0110, 0b0100_1000, 0b0000_0001, 1,
            ],
        ), (
            1_000_000_000,
            vec![
                0b0001_0010, 0,
                0b0011_0010, 0b0000_0100, 1, 2, 3, 4,
            ]
        )
    ];

    let expected = [
        (
            false,  // marker bit
            0,      // relative RTP timestamp
            vec![   // payload bytes
                0b0010_1000,
                0b0000_1101, 0b0011_0000, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
                             0b0011_0000, 1, 2, 3, 4, 5, 6, 7, 8, 9,
            ],
        ), (
            false,
            0,
            vec![
                0b0001_0000,
                0b0111_1000, 1, 2, 3, 4,
            ]
        ), (
            false,
            0,
            vec![
                0b0001_0000,
                0b0011_0100, 0b0010_1000, 1, 2, 3, 4, 5,
            ]
        ), (
            true,
            0,
            vec![
                0b0001_0000,
                0b0011_0100, 0b0100_1000, 1,
            ]
        ), (
            false,
            90_000,
            vec![
                0b0001_0000,
                0b0011_0000, 1, 2, 3, 4,
            ]
        )
    ];

    init();

    let mut h = Harness::new("rtpav1pay");
    {
        let pay = h.element().unwrap();
        pay.set_property(
            "mtu",
            gst_rtp::calc_packet_len(25, 0, 0)
        );
    }
    h.play();

    let caps = Caps::builder("video/x-av1")
        .field("parsed", true)
        .field("stream-format", "obu-stream")
        .field("alignment", "obu")
        .build();
    h.set_src_caps(caps);

    for (pts, bytes) in &test_buffers {
        let mut buffer = Buffer::with_size(bytes.len())
            .unwrap()
            .into_mapped_buffer_writable()
            .unwrap();
        buffer.copy_from_slice(bytes);

        let mut buffer = buffer.into_buffer();
        buffer.get_mut().unwrap().set_pts(ClockTime::try_from(*pts).unwrap());

        h.push(buffer).unwrap();
    }
    h.push_event(Eos::new());

    let mut base_ts = None;
    for (idx, (marker, ts_offset, payload)) in expected.iter().enumerate() {
        println!("checking packet {}...", idx);

        let buffer = h.pull().unwrap();
        let packet = RTPBuffer::from_buffer_readable(&buffer).unwrap();
        if base_ts.is_none() {
            base_ts = Some(packet.timestamp());
        }

        assert_eq!(packet.payload().unwrap(), payload.as_slice());
        assert_eq!(packet.is_marker(), *marker);
        assert_eq!(packet.timestamp(), base_ts.unwrap() + ts_offset);
    }
}
