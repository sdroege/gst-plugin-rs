// GStreamer RTP GSM Payloader / Depayloader - unit tests
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
        crate::plugin_register_static().expect("rtpgsm test");
    });
}

#[test]
fn test_gsm() {
    init();

    let src = "audiotestsrc num-buffers=100 samplesperbuffer=160 ! audio/x-raw,rate=8000,channels=1 ! gsmenc";
    let pay = "rtpgsmpay2";
    let depay = "rtpgsmdepay2";

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
                .pt(3)
                .rtp_time(((i * 160) & 0xffff_ffff) as u32)
                .marker_bit(i == 0)
                .size(45)
                .build(),
        ]);
    }

    let mut expected_depay = Vec::with_capacity(100);
    for i in 0..100 {
        expected_depay.push(vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(i * 20))
                .size(33)
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
fn test_gsm_min_ptime() {
    init();

    let caps = gst::Caps::builder("audio/x-gsm")
        .field("channels", 1)
        .field("rate", 8000i32)
        .build();

    let pay = "rtpgsmpay2 min-ptime=40000000";
    let depay = "rtpgsmdepay2";

    let mut buffers = Vec::with_capacity(100);
    for i in 0..100 {
        let mut buffer = gst::Buffer::from_mut_slice(vec![0x4; 33]);
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_mseconds(i as u64 * 20));
        }
        buffers.push(buffer);
    }

    let mut expected_pay = Vec::with_capacity(50);
    for i in 0..50 {
        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(i * 40))
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER
                } else {
                    gst::BufferFlags::empty()
                })
                .pt(3)
                .rtp_time(((i * 320) & 0xffff_ffff) as u32)
                .marker_bit(i == 0)
                .size(12 + 66)
                .build(),
        ]);
    }

    let mut expected_depay = Vec::with_capacity(50);
    for i in 0..50 {
        expected_depay.push(vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(i * 40))
                .size(66)
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
fn test_gsm_partial_frame_panic() {
    init();

    let caps = gst::Caps::builder("audio/x-gsm")
        .field("channels", 1i32)
        .field("rate", 8000i32)
        .build();

    let mut buffer = gst::Buffer::from_mut_slice(vec![0x4a; 10]);
    buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);

    run_test_pipeline(
        Source::Buffers(caps, vec![buffer]),
        "rtpgsmpay2",
        "rtpgsmdepay2",
        vec![],
        vec![],
    );
}
