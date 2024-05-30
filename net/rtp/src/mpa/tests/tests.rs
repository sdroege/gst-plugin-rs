// GStreamer RTP MPEG audio Payloader / Depayloader - unit tests
//
// Copyright (C) 2024 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::mpa::mpeg_audio_utils::PeekData::FramedData;
use crate::mpa::mpeg_audio_utils::*;

use crate::tests::{
    ExpectedBuffer, ExpectedPacket, Liveness, Source, run_test_pipeline, run_test_pipeline_full,
};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtp mpa test");
    });
}

// gst-launch-1.0 audiotestsrc samplesperbuffer=1152 wave=ticks num-buffers=3
//  ! audio/x-raw,rate=48000 ! lamemp3enc ! filesink
const MP3_DATA: &[u8] = include_bytes!("audiotestsrc-1ch-48kHz.mp3").as_slice();

pub(crate) fn parse_mpa_frames(data: &[u8]) -> Vec<&[u8]> {
    let mut frames = vec![];

    let mut data = &data[0..];

    while !data.is_empty() {
        let hdr = peek_frame_header(FramedData(data)).unwrap();
        let size = hdr.frame_len.expect("frame length");
        eprintln!("MP{} frame {} of size {size}", hdr.layer, frames.len());
        frames.push(&data[0..][..size]);
        data = &data[size..];
    }

    frames
}

fn make_buffer(
    data: &'static [u8],
    pts: gst::ClockTime,
    duration: gst::ClockTime,
    flags: gst::BufferFlags,
) -> gst::Buffer {
    let mut buf = gst::Buffer::from_slice(data);

    let buf_ref = buf.get_mut().unwrap();
    buf_ref.set_pts(pts);
    buf_ref.set_duration(duration);
    buf_ref.set_flags(flags);

    buf
}

// test_mpa_pay_depay_nonlive
//
// Check basic payloading/depayloading, in non-live (aggregate) mode
//
#[test]
#[allow(clippy::vec_init_then_push)]
fn test_mpa_pay_depay_nonlive() {
    init();

    println!("Testing non-live mode (should aggregate frames)..");

    let frames = parse_mpa_frames(MP3_DATA);

    let input_caps = {
        let hdr = peek_frame_header(FramedData(frames[0])).unwrap();

        gst::Caps::builder("audio/mpeg")
            .field("rate", hdr.sample_rate as i32)
            .field("channels", hdr.channels as i32)
            .field("mpegversion", hdr.version as i32)
            .field("layer", hdr.layer as i32)
            .field("parsed", true)
            .build()
    };

    let mut input_buffers = vec![];

    for (i, frame) in frames.iter().enumerate() {
        input_buffers.push(make_buffer(
            frame,
            gst::ClockTime::from_mseconds(24 * i as u64),
            gst::ClockTime::from_mseconds(24),
            if i == 0 {
                gst::BufferFlags::DISCONT
            } else {
                gst::BufferFlags::empty()
            },
        ));
    }

    // If upstream is non-live, the payloader should collect and pack as many MP3 frames
    // into each RTP packet as it can. With mtu=300 and frame size of 96 bytes,
    // that's 2 frames/packet.
    let mut expected_pay = vec![];

    // Each frame is 1152 samples, at 48kHz, but RTP clock-rate is 90kHz.
    expected_pay.push(vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::ZERO)
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            .pt(14)
            .rtp_time(0)
            .marker_bit(true)
            .build(),
    ]);
    expected_pay.push(vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(2 * 24))
            .flags(gst::BufferFlags::empty())
            .pt(14)
            .rtp_time(2 * 1152 * 90000 / 48000)
            .marker_bit(false)
            .build(),
    ]);

    let expected_depay = vec![
        // 2 MP3 frames of 96 bytes / 24ms in a single depayloader output buffer
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::ZERO)
                .duration(gst::ClockTime::from_mseconds(48))
                .size(192)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::RESYNC)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(48))
                .duration(gst::ClockTime::from_mseconds(48))
                .size(192)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
    ];

    run_test_pipeline_full(
        Source::Buffers(input_caps, input_buffers),
        "rtpmpapay2 mtu=300",
        "rtpmpadepay2",
        expected_pay,
        expected_depay,
        None,
        Liveness::NonLive,
    );
}

// test_mpa_pay_depay_live
//
// Check basic payloading/depayloading, in live mode
//
#[test]
fn test_mpa_pay_depay_live() {
    init();

    println!("Testing live mode (should send out frames immediately)..");

    let frames = parse_mpa_frames(MP3_DATA);

    let input_caps = {
        let hdr = peek_frame_header(FramedData(frames[0])).unwrap();

        gst::Caps::builder("audio/mpeg")
            .field("rate", hdr.sample_rate as i32)
            .field("channels", hdr.channels as i32)
            .field("mpegversion", hdr.version as i32)
            .field("layer", hdr.layer as i32)
            .field("parsed", true)
            .build()
    };

    let mut input_buffers = vec![];

    for (i, frame) in frames.iter().enumerate() {
        input_buffers.push(make_buffer(
            frame,
            gst::ClockTime::from_mseconds(24 * i as u64),
            gst::ClockTime::from_mseconds(24),
            if i == 0 {
                gst::BufferFlags::DISCONT
            } else {
                gst::BufferFlags::empty()
            },
        ));
    }

    let mut expected_pay = vec![];

    for (i, _frame) in frames.iter().enumerate() {
        let discont_flag = if i == 0 {
            gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER
        } else {
            gst::BufferFlags::empty()
        };

        // Each frame is 1152 samples, at 48kHz, but RTP clock-rate is 90kHz.
        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(24 * i as u64))
                .flags(discont_flag)
                .pt(14)
                .rtp_time(1152 * i as u32 * 90000 / 48000)
                .marker_bit(discont_flag.contains(gst::BufferFlags::MARKER))
                .build(),
        ]);
    }

    let expected_depay = vec![
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::ZERO)
                .duration(gst::ClockTime::from_mseconds(24))
                .size(96)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::RESYNC)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(24))
                .duration(gst::ClockTime::from_mseconds(24))
                .size(96)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(48))
                .duration(gst::ClockTime::from_mseconds(24))
                .size(96)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(72))
                .duration(gst::ClockTime::from_mseconds(24))
                .size(96)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
    ];

    run_test_pipeline_full(
        Source::Buffers(input_caps, input_buffers),
        "rtpmpapay2 mtu=300",
        "rtpmpadepay2",
        expected_pay,
        expected_depay,
        None,
        Liveness::Live(20_000_000),
    );
}

// test_mpa_pay_depay_fragmented
//
// Check basic payloading/depayloading with small MTU
//
#[test]
fn test_mpa_pay_depay_fragmented() {
    init();

    let frames = parse_mpa_frames(MP3_DATA);

    let input_caps = {
        let hdr = peek_frame_header(frames[0]).unwrap();

        gst::Caps::builder("audio/mpeg")
            .field("rate", hdr.sample_rate as i32)
            .field("channels", hdr.channels as i32)
            .field("mpegversion", hdr.version as i32)
            .field("layer", hdr.layer as i32)
            .field("parsed", true)
            .build()
    };

    let mut input_buffers = vec![];
    let mut expected_pay = vec![];

    for (i, frame) in frames.iter().enumerate() {
        let discont_flag = if i == 0 {
            gst::BufferFlags::DISCONT
        } else {
            gst::BufferFlags::empty()
        };
        input_buffers.push(make_buffer(
            frame,
            gst::ClockTime::from_mseconds(24 * i as u64),
            gst::ClockTime::from_mseconds(24),
            discont_flag,
        ));

        let marker_flag = if i == 0 {
            gst::BufferFlags::MARKER
        } else {
            gst::BufferFlags::empty()
        };

        // Each 96 byte MP3 frame will be split into 3 RTP packets with mtu=60.
        // Each frame is 1152 samples, at 48kHz, but RTP clock-rate is 90kHz.
        // MARKER = start of talk spurt, so first buffer should have it.
        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(24 * i as u64))
                .flags(discont_flag | marker_flag)
                .pt(14)
                .rtp_time(1152 * i as u32 * 90000 / 48000)
                .marker_bit(marker_flag.contains(gst::BufferFlags::MARKER))
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(24 * i as u64))
                .flags(gst::BufferFlags::empty())
                .pt(14)
                .rtp_time(1152 * i as u32 * 90000 / 48000)
                .marker_bit(false)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(24 * i as u64))
                .flags(gst::BufferFlags::empty())
                .pt(14)
                .rtp_time(1152 * i as u32 * 90000 / 48000)
                .marker_bit(false)
                .build(),
        ]);
    }

    let expected_depay = vec![
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::ZERO)
                .duration(gst::ClockTime::from_mseconds(24))
                .size(96)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::RESYNC)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(24))
                .duration(gst::ClockTime::from_mseconds(24))
                .size(96)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(48))
                .duration(gst::ClockTime::from_mseconds(24))
                .size(96)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(72))
                .duration(gst::ClockTime::from_mseconds(24))
                .size(96)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
    ];
    run_test_pipeline(
        Source::Buffers(input_caps, input_buffers),
        "rtpmpapay2 mtu=60",
        "rtpmpadepay2",
        expected_pay,
        expected_depay,
    );
}
