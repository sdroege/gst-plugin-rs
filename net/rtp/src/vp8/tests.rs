//
// Copyright (C) 2024 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::tests::{ExpectedBuffer, ExpectedPacket, Source, run_test_pipeline};
use crate::vp8::depay::imp::MAX_FRAME_SIZE;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtpvp8 test");
    });
}

#[test]
fn test_vp8() {
    init();

    // Generates encoded frames of sizes 1915 (key), 110, 103, 100, 100
    let src = "videotestsrc num-buffers=5 pattern=smpte100 ! video/x-raw,format=I420,width=1280,height=720,framerate=25/1 ! vp8enc target-bitrate=4000000";
    let pay = "rtpvp8pay2 picture-id-mode=7-bit mtu=1400";
    let depay = "rtpvp8depay2";

    let expected_pay = vec![
        vec![
            // First frame is split into two packets
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::DISCONT)
                .pt(96)
                .rtp_time(0)
                .marker_bit(false)
                .size(1400)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(0)
                .marker_bit(true)
                .size(545)
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
                .size(125)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(7_200)
                .marker_bit(true)
                .size(118)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(10_800)
                .marker_bit(true)
                .size(115)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(14_400)
                .marker_bit(true)
                .size(115)
                .build(),
        ],
    ];

    let expected_depay = vec![
        // One buffer per frame
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .size(1915)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .size(110)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .size(103)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .size(100)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .size(100)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
    ];

    run_test_pipeline(Source::Bin(src), pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_vp8_small_mtu() {
    init();

    // Generates encoded frames of sizes 1915 (key), 110, 103, 100, 100
    let src = "videotestsrc num-buffers=5 pattern=smpte100 ! video/x-raw,format=I420,width=1280,height=720,framerate=25/1 ! vp8enc target-bitrate=4000000";
    let pay = "rtpvp8pay2 mtu=800 picture-id-mode=15-bit";
    let depay = "rtpvp8depay2";

    let expected_pay = vec![
        vec![
            // First frame is split into three packets
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::DISCONT)
                .pt(96)
                .rtp_time(0)
                .marker_bit(false)
                .size(800)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(0)
                .marker_bit(false)
                .size(800)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(0)
                .marker_bit(true)
                .size(363)
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
                .size(126)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(7_200)
                .marker_bit(true)
                .size(119)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(10_800)
                .marker_bit(true)
                .size(116)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(14_400)
                .marker_bit(true)
                .size(116)
                .build(),
        ],
    ];

    let expected_depay = vec![
        // One buffer per frame
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .size(1915)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .size(110)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .size(103)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .size(100)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .size(100)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
    ];

    run_test_pipeline(Source::Bin(src), pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_vp8_partitions() {
    init();

    // Generates encoded frames of sizes 1927 (key), 122, 115, 112, 112
    let src = "videotestsrc num-buffers=5 pattern=smpte100 ! video/x-raw,format=I420,width=1280,height=720,framerate=25/1 ! vp8enc token-partitions=4 target-bitrate=4000000";
    let pay = "rtpvp8pay2 mtu=800 fragmentation-mode=every-partition picture-id-mode=15-bit";
    let depay = "rtpvp8depay2";

    let expected_pay = vec![
        vec![
            // First frame is split into seven packets (3 first partition, 4 for the other partitions)
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::DISCONT)
                .pt(96)
                .rtp_time(0)
                .marker_bit(false)
                .size(800)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(0)
                .marker_bit(false)
                .size(800)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(0)
                .marker_bit(false)
                .size(190)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(0)
                .marker_bit(false)
                .size(187)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(0)
                .marker_bit(false)
                .size(28)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(0)
                .marker_bit(false)
                .size(17)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(0)
                .marker_bit(true)
                .size(17)
                .build(),
        ],
        // Second and following frames are all split into 5 packets, one per partition
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(3_600)
                .marker_bit(false)
                .size(129)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(3_600)
                .marker_bit(false)
                .size(22)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(3_600)
                .marker_bit(false)
                .size(17)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(3_600)
                .marker_bit(false)
                .size(17)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(3_600)
                .marker_bit(true)
                .size(17)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(7_200)
                .marker_bit(false)
                .size(125)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(7_200)
                .marker_bit(false)
                .size(19)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(7_200)
                .marker_bit(false)
                .size(17)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(7_200)
                .marker_bit(false)
                .size(17)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(7_200)
                .marker_bit(true)
                .size(17)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(10_800)
                .marker_bit(false)
                .size(124)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(10_800)
                .marker_bit(false)
                .size(17)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(10_800)
                .marker_bit(false)
                .size(17)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(10_800)
                .marker_bit(false)
                .size(17)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(10_800)
                .marker_bit(true)
                .size(17)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(14_400)
                .marker_bit(false)
                .size(124)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(14_400)
                .marker_bit(false)
                .size(17)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(14_400)
                .marker_bit(false)
                .size(17)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(14_400)
                .marker_bit(false)
                .size(17)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(14_400)
                .marker_bit(true)
                .size(17)
                .build(),
        ],
    ];

    let expected_depay = vec![
        // One buffer per frame
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .size(1927)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .size(122)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(80))
                .size(115)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(120))
                .size(112)
                .flags(gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(160))
                .size(112)
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
        .field("encoding-name", "VP8")
        .field("payload", 96i32)
        .build();

    // S=1 starts a frame; keyframe uncompressed header (sync code 9d 01 2a).
    let header = [
        0x10, 0x00, 0x00, 0x00, 0x9d, 0x01, 0x2a, 0x00, 0x40, 0x00, 0x40, 0x00,
    ];
    let mut good = header.to_vec();
    good.extend_from_slice(&[0x42; 10]);
    let mut start = header.to_vec();
    start.extend_from_slice(&[0x11; 50]);
    let mut cont = vec![0x00];
    cont.extend_from_slice(&[0xAB; 4000]);
    let mut end = vec![0x00];
    end.extend_from_slice(&[0xAB; 4]);

    let mut packets: Vec<(&[u8], bool, u32, bool)> = Vec::new();
    // Picture 1 is complete and consumes the base class' initial DISCONT.
    packets.push((good.as_slice(), true, 0, false));
    // Picture 2 dribbles past the reassembly cap and then ends. The cap must
    // drop it, so only pictures 1 and 3 come out; without the cap the end
    // packet would complete an oversized picture.
    packets.push((start.as_slice(), false, 2000, false));
    // One descriptor byte per packet does not reach the accumulator.
    for _ in 0..MAX_FRAME_SIZE / (cont.len() - 1) + 2 {
        packets.push((cont.as_slice(), false, 2000, false));
    }
    packets.push((end.as_slice(), true, 2000, false));
    // Picture 3 completes again after the cap fired.
    packets.push((good.as_slice(), true, 4000, false));

    let buffers = crate::tests::run_depay_only("rtpvp8depay2", &caps, &packets).unwrap();
    assert_eq!(buffers.len(), 2);
    assert_eq!(buffers[0].0.len(), 21);
    assert_eq!(buffers[1].0.len(), 21);
    assert!(buffers[0].1.contains(gst::BufferFlags::DISCONT));
    // The drops in between must mark picture 3 discontinuous.
    assert!(buffers[1].1.contains(gst::BufferFlags::DISCONT));
}
