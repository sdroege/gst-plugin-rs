//
// Copyright (C) 2026 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::tests::{ExpectedBuffer, ExpectedPacket, Source, run_test_pipeline};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtpg72x test");
    });
}

#[test]
fn test_g722() {
    init();

    let src = "audiotestsrc num-buffers=100 samplesperbuffer=320 ! audio/x-raw,rate=16000,channels=1 ! avenc_g722";
    let pay = "rtpg722pay2";
    let depay = "rtpg722depay2";

    let mut expected_pay = Vec::with_capacity(100);
    for i in 0..100 {
        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(i * 20))
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER
                } else {
                    gst::BufferFlags::empty()
                })
                .pt(9)
                .rtp_time(((i * 160) & 0xffff_ffff) as u32)
                .marker_bit(i == 0)
                .size(172)
                .build(),
        ]);
    }

    let mut expected_depay = Vec::with_capacity(100);
    for i in 0..100 {
        expected_depay.push(vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(i * 20))
                .size(160)
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::RESYNC
                } else {
                    gst::BufferFlags::empty()
                })
                .build(),
        ]);
    }

    run_test_pipeline(Source::Bin(src), pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_g723() {
    init();

    let src = "audiotestsrc num-buffers=100 samplesperbuffer=240 ! audio/x-raw,rate=8000,channels=1 ! avenc_g723_1";
    let pay = "rtpg723pay2";
    let depay = "rtpg723depay2";

    let mut expected_pay = Vec::with_capacity(100);
    for i in 0..100 {
        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(i * 30))
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER
                } else {
                    gst::BufferFlags::empty()
                })
                .pt(4)
                .rtp_time(((i * 240) & 0xffff_ffff) as u32)
                .marker_bit(i == 0)
                .size(36)
                .build(),
        ]);
    }

    let mut expected_depay = Vec::with_capacity(100);
    for i in 0..100 {
        expected_depay.push(vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(i * 30))
                .size(24)
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::RESYNC
                } else {
                    gst::BufferFlags::empty()
                })
                .build(),
        ]);
    }

    run_test_pipeline(Source::Bin(src), pay, depay, expected_pay, expected_depay);
}

fn g723_frame_size(frame_byte: u8) -> usize {
    match frame_byte {
        0 => 24,
        1 => 20,
        2 => 4,
        _ => unreachable!(),
    }
}

#[test]
fn test_g723_combined() {
    init();

    let caps = gst::Caps::builder("audio/G723")
        .field("channels", 1)
        .field("rate", 8000i32)
        .build();

    let pay = "rtpg723pay2 min-ptime=120000000";
    let depay = "rtpg723depay2";

    let mut buffers = Vec::with_capacity(100);
    for i in 0..100 {
        let frame = (i / 4) % 3;
        let size = g723_frame_size(frame);

        let mut buffer = gst::Buffer::from_mut_slice(vec![frame; size]);
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_mseconds(i as u64 * 30));
        }
        buffers.push(buffer);
    }

    let mut expected_pay = Vec::with_capacity(25);
    for i in 0..25 {
        let frame = i % 3;
        let size = g723_frame_size(frame);
        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(i as u64 * 120))
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER
                } else {
                    gst::BufferFlags::empty()
                })
                .pt(4)
                .rtp_time(((i as u64 * 240 * 4) & 0xffff_ffff) as u32)
                .marker_bit(i == 0)
                .size(12 + 4 * size)
                .build(),
        ]);
    }

    let mut expected_depay = Vec::with_capacity(100);
    for i in 0..25 {
        let frame = i % 3;
        let size = g723_frame_size(frame);
        expected_depay.push(vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(i as u64 * 120))
                .size(size * 4)
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::RESYNC
                } else {
                    gst::BufferFlags::empty()
                })
                .build(),
        ]);
    }

    run_test_pipeline(
        Source::Buffers(caps, buffers),
        pay,
        depay,
        expected_pay,
        expected_depay,
    );
}

#[test]
fn test_g723_splitting() {
    init();

    let caps = gst::Caps::builder("audio/G723")
        .field("channels", 1)
        .field("rate", 8000i32)
        .build();

    // splits 4 frame inputs into individual frames for payloading
    let pay = "rtpg723pay2 max-ptime=30000000";
    let depay = "rtpg723depay2";

    let mut buffers = Vec::with_capacity(25);
    for i in 0..25 {
        let frame = i % 3;
        let size = g723_frame_size(frame);

        let mut buffer = gst::Buffer::from_mut_slice(vec![frame; size * 4]);
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_mseconds(i as u64 * 120));
        }
        buffers.push(buffer);
    }

    let mut expected_pay = Vec::with_capacity(100);
    for i in 0..100 {
        let frame = (i / 4) % 3;
        let size = g723_frame_size(frame);
        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(i as u64 * 30))
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER
                } else {
                    gst::BufferFlags::empty()
                })
                .pt(4)
                .rtp_time(((i as u64 * 240) & 0xffff_ffff) as u32)
                .marker_bit(i == 0)
                .size(12 + size)
                .build(),
        ]);
    }

    let mut expected_depay = Vec::with_capacity(100);
    for i in 0..100 {
        let frame = (i / 4) % 3;
        let size = g723_frame_size(frame);
        expected_depay.push(vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(i as u64 * 30))
                .size(size)
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::RESYNC
                } else {
                    gst::BufferFlags::empty()
                })
                .build(),
        ]);
    }

    run_test_pipeline(
        Source::Buffers(caps, buffers),
        pay,
        depay,
        expected_pay,
        expected_depay,
    );
}

#[test]
fn test_g723_overlapping_input() {
    init();

    let caps = gst::Caps::builder("audio/G723")
        .field("channels", 1)
        .field("rate", 8000i32)
        .build();

    // splits 3 frame inputs into 2 frames of payloading
    let pay = "rtpg723pay2 max-ptime=60000000 min-ptime=60000000";
    let depay = "rtpg723depay2";

    let mut buffers = Vec::with_capacity(4);
    for i in 0..4 {
        let frame = i % 3;
        let size = g723_frame_size(frame);

        let mut buffer = gst::Buffer::from_mut_slice(vec![frame; size * 3]);
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_mseconds(i as u64 * 90));
        }
        buffers.push(buffer);
    }

    let mut expected_pay = Vec::with_capacity(36);
    for i in 0..6 {
        let input_frame = i * 2;
        let size =
            g723_frame_size((input_frame / 3) % 3) + g723_frame_size(((input_frame + 1) / 3) % 3);
        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(i as u64 * 60))
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER
                } else {
                    gst::BufferFlags::empty()
                })
                .pt(4)
                .rtp_time(((i as u64 * 480) & 0xffff_ffff) as u32)
                .marker_bit(i == 0)
                .size(12 + size)
                .build(),
        ]);
    }

    let mut expected_depay = Vec::with_capacity(36);
    for i in 0..6 {
        let input_frame = i * 2;
        let size =
            g723_frame_size((input_frame / 3) % 3) + g723_frame_size(((input_frame + 1) / 3) % 3);
        expected_depay.push(vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(i as u64 * 60))
                .size(size)
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::RESYNC
                } else {
                    gst::BufferFlags::empty()
                })
                .build(),
        ]);
    }

    run_test_pipeline(
        Source::Buffers(caps, buffers),
        pay,
        depay,
        expected_pay,
        expected_depay,
    );
}

#[test]
fn test_g726() {
    init();

    let src = "audiotestsrc num-buffers=100 samplesperbuffer=2048 ! audio/x-raw,rate=8000,channels=1 ! avenc_g726";
    let pay = "rtpg726pay2";
    let depay = "rtpg726depay2";

    let mut expected_pay = Vec::with_capacity(100);
    for i in 0..100 {
        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(i * 256))
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER
                } else {
                    gst::BufferFlags::empty()
                })
                .pt(96)
                .rtp_time(((i * 2048) & 0xffff_ffff) as u32)
                .marker_bit(i == 0)
                .size(1036)
                .build(),
        ]);
    }

    let mut expected_depay = Vec::with_capacity(100);
    for i in 0..100 {
        expected_depay.push(vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(i * 256))
                .size(1024)
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::RESYNC
                } else {
                    gst::BufferFlags::empty()
                })
                .build(),
        ]);
    }

    run_test_pipeline(Source::Bin(src), pay, depay, expected_pay, expected_depay);
}
