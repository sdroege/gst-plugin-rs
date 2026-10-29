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
