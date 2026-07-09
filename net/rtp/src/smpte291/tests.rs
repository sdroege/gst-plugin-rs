//
// Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::tests::{ExpectedBuffer, ExpectedPacket, Source, run_test_pipeline_and_validate_data};
use anyhow::bail;
use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtpsmpte291 test");
    });
}

#[test]
fn test_smpte291() {
    init();

    let caps = gst::Caps::builder("meta/x-st-2038")
        .field("alignment", "frame")
        .build();

    // Two ST2038 packets with a single CEA708 CC ANC packet
    let packets = [
        &[
            0x00, 0x02, 0x40, 0x02, 0x61, 0x80, 0x64, 0x96, 0x59, 0x69, 0x92, 0x64, 0xf9, 0x0e,
            0x02, 0x8f, 0x57, 0x2b, 0xd1, 0xfc, 0xa0, 0x28, 0x0b, 0xf6, 0x80, 0xa0, 0x1f, 0xa4,
            0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f,
            0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40,
            0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00,
            0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9,
            0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0x74, 0x80, 0xa3, 0xd5,
            0x06, 0xab,
        ],
        &[
            0x00, 0x02, 0x40, 0x02, 0x61, 0x80, 0x64, 0x96, 0x59, 0x69, 0x92, 0x64, 0xf9, 0x0e,
            0x02, 0x8f, 0x97, 0x2b, 0xd1, 0xfc, 0xa0, 0x28, 0x0b, 0xf6, 0x80, 0xa0, 0x1f, 0xa4,
            0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f,
            0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40,
            0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00,
            0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9,
            0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0x74, 0x80, 0xa3, 0xe4,
            0xfe, 0xab,
        ],
    ];

    let buffers = packets
        .into_iter()
        .enumerate()
        .map(|(i, data)| {
            let mut buffer = gst::Buffer::from_slice(data);
            {
                let buffer = buffer.get_mut().unwrap();
                buffer.set_pts(
                    gst::ClockTime::SECOND
                        .mul_div_ceil(i as u64, 30000 / 1001)
                        .unwrap(),
                );
            }

            buffer
        })
        .collect();

    let pay = "rtpsmpte291pay";
    let depay = "rtpsmpte291depay";

    let expected_pay = vec![
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(0)
                .marker_bit(true)
                .size(124)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_nseconds(34482759))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(3104)
                .marker_bit(true)
                .size(124)
                .build(),
        ],
    ];

    let expected_depay = vec![
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .size(100)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_nseconds(34482759))
                .size(100)
                .flags(gst::BufferFlags::MARKER)
                .build(),
        ],
    ];

    run_test_pipeline_and_validate_data(
        Source::Buffers(caps, buffers),
        pay,
        depay,
        expected_pay,
        expected_depay,
        |data, list_idx, buffer_idx| {
            if buffer_idx != 0 {
                bail!("Got multiple output packets per RTP packet");
            }

            if list_idx >= 2 {
                bail!("Too many packets (got {}, expected {})", list_idx + 1, 2);
            }

            if packets[list_idx] != data {
                bail!("Packet {} has the wrong content", list_idx);
            }

            Ok(())
        },
    );
}

// A single frame can carry more than one ANC packet. The payloader packs them
// into one RTP packet (each padded to a 32-bit boundary, marker bit on the
// frame's last packet), so the depayloader must skip the inter-packet
// word_align padding and set MARKER only on the last packet - otherwise every
// packet after the first is misread and dropped, or the frame is split.
#[test]
fn test_smpte291_multiple_anc_packets_per_frame() {
    init();

    let caps = gst::Caps::builder("meta/x-st-2038")
        .field("alignment", "frame")
        .build();

    // Two CEA708 CC ANC packets (as in test_smpte291).
    // They are concatenated below into a single buffer.
    let packets: [&[u8]; 2] = [
        &[
            0x00, 0x02, 0x40, 0x02, 0x61, 0x80, 0x64, 0x96, 0x59, 0x69, 0x92, 0x64, 0xf9, 0x0e,
            0x02, 0x8f, 0x57, 0x2b, 0xd1, 0xfc, 0xa0, 0x28, 0x0b, 0xf6, 0x80, 0xa0, 0x1f, 0xa4,
            0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f,
            0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40,
            0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00,
            0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9,
            0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0x74, 0x80, 0xa3, 0xd5,
            0x06, 0xab,
        ],
        &[
            0x00, 0x02, 0x40, 0x02, 0x61, 0x80, 0x64, 0x96, 0x59, 0x69, 0x92, 0x64, 0xf9, 0x0e,
            0x02, 0x8f, 0x97, 0x2b, 0xd1, 0xfc, 0xa0, 0x28, 0x0b, 0xf6, 0x80, 0xa0, 0x1f, 0xa4,
            0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f,
            0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40,
            0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00,
            0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9,
            0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0x74, 0x80, 0xa3, 0xe4,
            0xfe, 0xab,
        ],
    ];

    let mut buffer = gst::Buffer::from_slice(packets.concat());
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(gst::ClockTime::ZERO);
    }
    let buffers = vec![buffer];

    let pay = "rtpsmpte291pay";
    let depay = "rtpsmpte291depay";

    // Both ANC packets in one RTP packet, marker bit set (last packet of frame).
    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::ZERO)
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            .pt(96)
            .rtp_time(0)
            .marker_bit(true)
            .size(228)
            .build(),
    ]];

    // One output sample carrying both packets; MARKER only on the last.
    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::ZERO)
            .size(100)
            .flags(gst::BufferFlags::DISCONT)
            .build(),
        // Subsequent buffers in a list carry no PTS of their own.
        ExpectedBuffer::builder()
            .size(100)
            .flags(gst::BufferFlags::MARKER)
            .build(),
    ]];

    run_test_pipeline_and_validate_data(
        Source::Buffers(caps, buffers),
        pay,
        depay,
        expected_pay,
        expected_depay,
        |data, _list_idx, buffer_idx| {
            if buffer_idx >= 2 {
                bail!("Too many output packets for one frame");
            }

            if packets[buffer_idx] != data {
                bail!("Packet {} has the wrong content", buffer_idx);
            }

            Ok(())
        },
    );
}
