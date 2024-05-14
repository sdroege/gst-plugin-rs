// GStreamer RTP Opus Payloader / Depayloader - unit tests
//
// Copyright (C) 2024 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::tests::{run_test_pipeline, ExpectedBuffer, ExpectedPacket, Source};
use gst_check::Harness;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtpopus test");
    });
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

// test_opus_pay_dtx
//
// Make sure payloader drops any DTX packets by the encoder (if so requested via the property)
//
#[test]
fn test_opus_pay_dtx() {
    // gst-launch-1.0 audiotestsrc wave=silence
    //  ! opusenc dtx=true bitrate-type=vbr
    //  ! fakesink silent=false dump=true
    const OPUS_BUFFER_SILENCE: &[u8] = &[0xf8, 0xff, 0xfe];
    const OPUS_BUFFER_SILENCE_DTX: &[u8] = &[0xf8];

    init();

    for dtx_prop in [false, true] {
        eprintln!("Testing rtpopuspay2 dtx={dtx_prop} ..");

        let input_caps = gst::Caps::builder("audio/x-opus")
            .field("rate", 48000i32)
            .field("channels", 1i32)
            .field("channel-mapping-family", 0i32)
            .build();

        let input_buffers = vec![
            make_buffer(
                OPUS_BUFFER_SILENCE,
                gst::ClockTime::ZERO,
                gst::ClockTime::from_useconds(13500),
                gst::BufferFlags::DISCONT,
            ),
            make_buffer(
                OPUS_BUFFER_SILENCE,
                gst::ClockTime::from_useconds(13500),
                gst::ClockTime::from_mseconds(20),
                gst::BufferFlags::empty(),
            ),
            make_buffer(
                OPUS_BUFFER_SILENCE_DTX,
                gst::ClockTime::from_useconds(33500),
                gst::ClockTime::from_mseconds(20),
                gst::BufferFlags::empty(),
            ),
        ];

        // TODO: check durations?
        let mut expected_pay = vec![
            vec![ExpectedPacket::builder()
                .pts(gst::ClockTime::ZERO)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(0)
                .marker_bit(true)
                .build()],
            vec![ExpectedPacket::builder()
                .pts(gst::ClockTime::from_useconds(13500))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(648)
                .marker_bit(false)
                .build()],
            vec![ExpectedPacket::builder()
                .pts(gst::ClockTime::from_useconds(33500))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(648 + 960)
                .marker_bit(false)
                .build()],
        ];

        // TODO: check durations?
        let mut expected_depay = vec![
            vec![ExpectedBuffer::builder()
                .pts(gst::ClockTime::ZERO)
                .size(3)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::RESYNC)
                .build()],
            vec![ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_useconds(13500))
                .size(3)
                .flags(gst::BufferFlags::empty())
                .build()],
            vec![ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_useconds(33500))
                .size(1)
                .flags(gst::BufferFlags::empty())
                .build()],
        ];

        if dtx_prop {
            // Payloader should drop DTX buffer if dtx=true and not output it
            expected_pay.pop();
            expected_depay.pop();
        }

        run_test_pipeline(
            Source::Buffers(input_caps, input_buffers),
            &format!("rtpopuspay2 dtx={dtx_prop}"),
            "rtpopusdepay2",
            expected_pay,
            expected_depay,
        );
    }
}

// test_opus_pay_depay
//
// Check basic payloading/depayloading
//
#[test]
fn test_opus_pay_depay() {
    // gst-launch-1.0 audiotestsrc ! opusenc ! multifilesink
    const OPUS_BUFFERS: &[&[u8]] = &[
        include_bytes!("audiotestsrc-1ch-48kHz-000.opus").as_slice(),
        include_bytes!("audiotestsrc-1ch-48kHz-001.opus").as_slice(),
        include_bytes!("audiotestsrc-1ch-48kHz-002.opus").as_slice(),
    ];

    init();

    let input_caps = gst::Caps::builder("audio/x-opus")
        .field("rate", 48000i32)
        .field("channels", 1i32)
        .field("channel-mapping-family", 0i32)
        .field("stream-count", 1i32)
        .field("coupled-count", 0i32)
        .build();

    let input_buffers = vec![
        make_buffer(
            OPUS_BUFFERS[0],
            gst::ClockTime::ZERO,
            gst::ClockTime::from_mseconds(20), // Note: no ClippingMeta for lead-in here unlike opusenc
            gst::BufferFlags::DISCONT,
        ),
        make_buffer(
            OPUS_BUFFERS[1],
            gst::ClockTime::from_mseconds(20),
            gst::ClockTime::from_mseconds(20),
            gst::BufferFlags::empty(),
        ),
        make_buffer(
            OPUS_BUFFERS[2],
            gst::ClockTime::from_mseconds(40),
            gst::ClockTime::from_mseconds(20),
            gst::BufferFlags::empty(),
        ),
    ];

    // TODO: check durations?
    let expected_pay = vec![
        vec![ExpectedPacket::builder()
            .pts(gst::ClockTime::ZERO)
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            .pt(96)
            .rtp_time(0)
            .marker_bit(true)
            .build()],
        vec![ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(20))
            .flags(gst::BufferFlags::empty())
            .pt(96)
            .rtp_time(960)
            .marker_bit(false)
            .build()],
        vec![ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(40))
            .flags(gst::BufferFlags::empty())
            .pt(96)
            .rtp_time(960 + 960)
            .marker_bit(false)
            .build()],
    ];

    // TODO: check durations?
    let expected_depay = vec![
        vec![ExpectedBuffer::builder()
            .pts(gst::ClockTime::ZERO)
            .size(253)
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::RESYNC)
            .build()],
        vec![ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_mseconds(20))
            .size(168)
            .flags(gst::BufferFlags::empty())
            .build()],
        vec![ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_mseconds(40))
            .size(166)
            .flags(gst::BufferFlags::empty())
            .build()],
    ];

    run_test_pipeline(
        Source::Buffers(input_caps, input_buffers),
        "rtpopuspay2",
        "rtpopusdepay2",
        expected_pay,
        expected_depay,
    );
}

// test_opus_depay_pay
//
// Check basic depayloader ! payloader compatibility
//
#[test]
fn test_opus_depay_pay() {
    // gst-launch-1.0 audiotestsrc
    //  ! opusenc bitrate-type=vbr frame-size=2
    //  ! rtpopuspay ! fakesink dump=true
    const OPUS_RTP_BUFFER: &[u8] = &[
        0x80, 0xe0, 0x6c, 0xd6, 0x5f, 0x7a, 0xdd, 0xae, 0xa6, 0x79, 0xe0, 0xc9, 0xe0, 0xff, 0xfe,
    ];

    init();

    let mut h = Harness::new_parse("rtpopusdepay2 ! rtpopuspay2");

    let input_caps = gst::Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("encoding-name", "OPUS")
        .field("clock-rate", 48000i32)
        .field("payload", 96i32)
        .build();

    h.set_src_caps(input_caps);

    let input_buffer = make_buffer(
        OPUS_RTP_BUFFER,
        gst::ClockTime::ZERO,
        gst::ClockTime::from_mseconds(20),
        gst::BufferFlags::DISCONT,
    );

    h.push(input_buffer)
        .expect("Got error flow when pushing buffer");

    let _output_buffer = h.pull().expect("Didn't get output buffer");
}
