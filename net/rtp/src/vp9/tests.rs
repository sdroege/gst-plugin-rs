//
// Copyright (C) 2024 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::tests::{ExpectedBuffer, ExpectedPacket, Source, run_test_pipeline};
use crate::vp9::depay::imp::MAX_FRAME_SIZE;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtpvp9 test");
    });
}

#[test]
fn test_vp9() {
    init();

    // Generates encoded frames of sizes 1342 (key), 96, 41, 55, 41
    let src = "videotestsrc num-buffers=5 pattern=gradient ! video/x-raw,format=I420,width=1920,height=1080,framerate=25/1 ! vp9enc target-bitrate=4000000";
    let pay = "rtpvp9pay2 mtu=1200 picture-id-mode=7-bit";
    let depay = "rtpvp9depay2";

    let expected_pay = vec![
        vec![
            // First frame is split into two packets
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::DISCONT)
                .pt(96)
                .rtp_time(0)
                .marker_bit(false)
                .size(1200)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(0)
                .marker_bit(true)
                .size(170)
                .build(),
        ],
        // Second and following frames
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(3_600)
                .marker_bit(true)
                .size(110)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(7_200)
                .marker_bit(true)
                .size(55)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(10_800)
                .marker_bit(true)
                .size(69)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(14_400)
                .marker_bit(true)
                .size(55)
                .build(),
        ],
    ];

    let expected_depay = vec![
        // One buffer per frame
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .size(1342)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .size(96)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .size(41)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .size(55)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .size(41)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
    ];

    run_test_pipeline(Source::Bin(src), pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_vp9_small_mtu() {
    init();

    // Generates encoded frames of sizes 1342 (key), 96, 41, 55, 41
    let src = "videotestsrc num-buffers=5 pattern=gradient ! video/x-raw,format=I420,width=1920,height=1080,framerate=25/1 ! vp9enc target-bitrate=4000000";
    let pay = "rtpvp9pay2 mtu=500 picture-id-mode=15-bit";
    let depay = "rtpvp9depay2";

    let expected_pay = vec![
        vec![
            // First frame is split into three packets
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::DISCONT)
                .pt(96)
                .rtp_time(0)
                .marker_bit(false)
                .size(500)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(0)
                .marker_bit(false)
                .size(500)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(0)
                .marker_bit(true)
                .size(387)
                .build(),
        ],
        // Second and following frames
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(3_600)
                .marker_bit(true)
                .size(111)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(7_200)
                .marker_bit(true)
                .size(56)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(10_800)
                .marker_bit(true)
                .size(70)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(14_400)
                .marker_bit(true)
                .size(56)
                .build(),
        ],
    ];

    let expected_depay = vec![
        // One buffer per frame
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .size(1342)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .size(96)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .size(41)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .size(55)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .size(41)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
    ];

    run_test_pipeline(Source::Bin(src), pay, depay, expected_pay, expected_depay);
}

#[test]
fn depay_max_frame_size_caps_and_resyncs() {
    init();

    let caps = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("clock-rate", 90_000i32)
        .field("encoding-name", "VP9")
        .field("payload", 96i32)
        .build();

    // B=1, P=0: start of a key picture.
    let good = vec![0x08, 0x90, 0x01, 0x00, 0x00, 0x42];
    let start = vec![0x08, 0x90, 0x01, 0x00, 0x00];
    let mut cont = vec![0x00];
    cont.extend_from_slice(&[0xAB; 4000]);
    let mut end = vec![0x00];
    end.extend_from_slice(&[0xAB; 4]);

    let mut packets: Vec<(&[u8], bool, u32, bool)> = Vec::new();
    // Picture 1 is complete and consumes the base class' initial DISCONT.
    packets.push((good.as_slice(), true, 0, false));
    // Picture 2 dribbles past the reassembly cap and then ends on the same
    // timestamp, so only the cap can explain its absence; without the cap
    // the end packet would complete an oversized picture.
    packets.push((start.as_slice(), false, 2000, false));
    // One descriptor byte per packet does not reach the accumulator.
    for _ in 0..MAX_FRAME_SIZE / (cont.len() - 1) + 2 {
        packets.push((cont.as_slice(), false, 2000, false));
    }
    packets.push((end.as_slice(), true, 2000, false));
    // Picture 3 completes again after the cap fired.
    packets.push((good.as_slice(), true, 4000, false));

    let buffers = crate::tests::run_depay_only("rtpvp9depay2", &caps, &packets).unwrap();
    assert_eq!(buffers.len(), 2);
    assert_eq!(buffers[0].0, vec![0x90, 0x01, 0x00, 0x00, 0x42]);
    assert_eq!(buffers[1].0, vec![0x90, 0x01, 0x00, 0x00, 0x42]);
    assert!(buffers[0].1.contains(gst::BufferFlags::DISCONT));
    // The drops in between must mark picture 3 discontinuous.
    assert!(buffers[1].1.contains(gst::BufferFlags::DISCONT));
}
