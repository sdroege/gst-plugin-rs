// GStreamer RTP AC-3 Payloader / Depayloader - unit tests
//
// Copyright (C) 2024 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::ac3::ac3_audio_utils::*;
use crate::ac3::depay::imp::MAX_FRAME_SIZE;
use crate::tests::{
    ExpectedBuffer, ExpectedPacket, Liveness, Source, run_test_pipeline_and_validate_data,
    run_test_pipeline_full_and_validate_data,
};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtpac3 test");
    });
}

// gst-launch-1.0 audiotestsrc samplesperbuffer=1536 wave=ticks num-buffers=3
//  ! audio/x-raw,rate=48000 ! avenc_ac3 ! filesink
const AC3_DATA: &[u8] = include_bytes!("audiotestsrc-1ch-48kHz.ac3").as_slice();

pub(crate) fn parse_ac3_frames(data: &[u8]) -> Vec<&[u8]> {
    let mut frames = vec![];

    let mut data = &data[0..];

    while !data.is_empty() {
        let hdr = peek_frame_header(data).unwrap();
        let size = hdr.frame_len;
        // eprintln!("AC-3 frame {} of size {size}", frames.len());
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

// test_ac3_pay_depay
//
// Check basic payloading/depayloading, in live and non-live mode
//
#[test]
fn test_ac3_pay_depay() {
    init();

    fn run_ac3_pay_depay_test(liveness: Liveness) {
        let frames = parse_ac3_frames(AC3_DATA);

        let input_caps = gst::Caps::builder("audio/x-ac3")
            .field("rate", 48000i32)
            .field("channels", 1i32)
            .field("framed", true)
            .field("alignment", "frame")
            .build();

        let mut input_buffers = vec![];

        for (i, frame) in frames.iter().enumerate() {
            input_buffers.push(make_buffer(
                frame,
                gst::ClockTime::from_mseconds(32 * i as u64),
                gst::ClockTime::from_mseconds(32),
                if i == 0 {
                    gst::BufferFlags::DISCONT
                } else {
                    gst::BufferFlags::empty()
                },
            ));
        }

        // If upstream is non-live, the payloader should collect and pack as many AC-3 frames
        // into each RTP packet as it can. With mtu=1400 and frame size of 384 bytes,
        // that's 3 frames/packet.
        let mut expected_pay = vec![];
        if liveness == Liveness::NonLive {
            expected_pay.push(vec![
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::ZERO)
                    .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                    .pt(96)
                    .rtp_time(0)
                    .marker_bit(true)
                    .build(),
            ]);
            expected_pay.push(vec![
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::from_mseconds(96))
                    .flags(gst::BufferFlags::MARKER)
                    .pt(96)
                    .rtp_time(3 * 1536)
                    .marker_bit(true)
                    .build(),
            ]);
        } else {
            for (i, _frame) in frames.iter().enumerate() {
                let discont_flag = if i == 0 {
                    gst::BufferFlags::DISCONT
                } else {
                    gst::BufferFlags::empty()
                };

                expected_pay.push(vec![
                    ExpectedPacket::builder()
                        .pts(gst::ClockTime::from_mseconds(32 * i as u64))
                        .flags(discont_flag | gst::BufferFlags::MARKER)
                        .pt(96)
                        .rtp_time(1536 * i as u32)
                        .marker_bit(true)
                        .build(),
                ]);
            }
        }

        let expected_depay = vec![
            vec![
                ExpectedBuffer::builder()
                    .pts(gst::ClockTime::ZERO)
                    .duration(gst::ClockTime::from_mseconds(32))
                    .size(384)
                    .flags(gst::BufferFlags::DISCONT)
                    .build(),
            ],
            vec![
                ExpectedBuffer::builder()
                    .pts(gst::ClockTime::from_mseconds(32))
                    .duration(gst::ClockTime::from_mseconds(32))
                    .size(384)
                    .flags(gst::BufferFlags::empty())
                    .build(),
            ],
            vec![
                ExpectedBuffer::builder()
                    .pts(gst::ClockTime::from_mseconds(64))
                    .duration(gst::ClockTime::from_mseconds(32))
                    .size(384)
                    .flags(gst::BufferFlags::empty())
                    .build(),
            ],
            vec![
                ExpectedBuffer::builder()
                    .pts(gst::ClockTime::from_mseconds(96))
                    .duration(gst::ClockTime::from_mseconds(32))
                    .size(384)
                    .flags(gst::BufferFlags::empty())
                    .build(),
            ],
            vec![
                ExpectedBuffer::builder()
                    .pts(gst::ClockTime::from_mseconds(128))
                    .duration(gst::ClockTime::from_mseconds(32))
                    .size(384)
                    .flags(gst::BufferFlags::empty())
                    .build(),
            ],
        ];

        run_test_pipeline_full_and_validate_data(
            Source::Buffers(input_caps, input_buffers),
            "rtpac3pay2",
            "rtpac3depay2",
            expected_pay,
            expected_depay,
            None,
            liveness,
            |ac3_data, _, _| {
                if peek_frame_header(ac3_data).is_err() {
                    anyhow::bail!("Expected AC-3 frame header, got {ac3_data:02x?}")
                } else {
                    Ok(())
                }
            },
        );
    }

    println!("Testing non-live mode (should aggregate frames)..");
    run_ac3_pay_depay_test(Liveness::NonLive);

    println!("Testing live mode (should send out frames immediately)..");
    run_ac3_pay_depay_test(Liveness::Live(20_000_000));
}

// test_ac3_pay_depay_fragmented
//
// Check basic payloading/depayloading with small MTU
//
#[test]
fn test_ac3_pay_depay_fragmented() {
    init();

    let frames = parse_ac3_frames(AC3_DATA);

    let input_caps = gst::Caps::builder("audio/x-ac3")
        .field("rate", 48000i32)
        .field("channels", 1i32)
        .field("framed", true)
        .field("alignment", "frame")
        .build();

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
            gst::ClockTime::from_mseconds(32 * i as u64),
            gst::ClockTime::from_mseconds(32),
            discont_flag,
        ));

        // Each 384 byte AC-3 frame will be split into 2 RTP packets with mtu=250
        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(32 * i as u64))
                .flags(discont_flag)
                .pt(96)
                .rtp_time(1536 * i as u32)
                .marker_bit(false)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(32 * i as u64))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(1536 * i as u32)
                .marker_bit(true)
                .build(),
        ]);
    }

    let expected_depay = vec![
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::ZERO)
                .duration(gst::ClockTime::from_mseconds(32))
                .size(384)
                .flags(gst::BufferFlags::DISCONT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(32))
                .duration(gst::ClockTime::from_mseconds(32))
                .size(384)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(64))
                .duration(gst::ClockTime::from_mseconds(32))
                .size(384)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(96))
                .duration(gst::ClockTime::from_mseconds(32))
                .size(384)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .duration(gst::ClockTime::from_mseconds(32))
                .pts(gst::ClockTime::from_mseconds(128))
                .size(384)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
    ];
    run_test_pipeline_and_validate_data(
        Source::Buffers(input_caps, input_buffers),
        "rtpac3pay2 mtu=250",
        "rtpac3depay2",
        expected_pay,
        expected_depay,
        |ac3_data, _, _| {
            if peek_frame_header(ac3_data).is_err() {
                anyhow::bail!("Expected AC-3 frame header, got {ac3_data:02x?}")
            } else {
                Ok(())
            }
        },
    );
}

// test_ac3_pay_depay_fragmented_with_packet_loss
//
// Check basic payloading/depayloading with small MTU and some packet loss
//
#[test]
fn test_ac3_pay_depay_fragmented_with_packet_loss() {
    init();

    fn run_ac3_pay_depay_fragmented_with_packet_loss_with_drop_mask(
        drop_mask: u32,
        initial_seqnum: Option<u16>,
    ) {
        let frames = parse_ac3_frames(AC3_DATA);

        let input_caps = gst::Caps::builder("audio/x-ac3")
            .field("rate", 48000i32)
            .field("channels", 1i32)
            .field("framed", true)
            .field("alignment", "frame")
            .build();

        let mut input_buffers = vec![];
        let mut expected_pay = vec![];
        let mut expected_depay = vec![];

        for (i, frame) in frames.iter().enumerate() {
            let packet_mask = (drop_mask >> (2 * i)) & 0b11;

            let discont_flag = if i == 0 {
                gst::BufferFlags::DISCONT
            } else {
                gst::BufferFlags::empty()
            };
            input_buffers.push(make_buffer(
                frame,
                gst::ClockTime::from_mseconds(32 * i as u64),
                gst::ClockTime::from_mseconds(32),
                discont_flag,
            ));

            // Each 384 byte AC-3 frame will be split into 2 RTP packets with mtu=250
            expected_pay.push(vec![
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::from_mseconds(32 * i as u64))
                    .flags(discont_flag)
                    .pt(96)
                    .rtp_time(1536 * i as u32)
                    .marker_bit(false)
                    .drop((packet_mask & 0b0001) == 0b0001)
                    .build(),
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::from_mseconds(32 * i as u64))
                    .flags(gst::BufferFlags::MARKER)
                    .pt(96)
                    .rtp_time(1536 * i as u32)
                    .marker_bit(true)
                    .drop((packet_mask & 0b0010) == 0b0010)
                    .build(),
            ]);

            // Expect discont on first packet and if previous packet got dropped
            let expected_flags = if i == 0 || (drop_mask >> (2 * (i - 1))) & 0b11 != 0b0000 {
                gst::BufferFlags::DISCONT
            } else {
                gst::BufferFlags::empty()
            };

            // If any of the fragments got dropped, we can't reconstruct the original payload
            if packet_mask == 0b0000 {
                expected_depay.push(vec![
                    ExpectedBuffer::builder()
                        .pts(gst::ClockTime::from_mseconds(32 * i as u64))
                        .duration(gst::ClockTime::from_mseconds(32))
                        .size(384)
                        .flags(expected_flags)
                        .build(),
                ]);
            }
        }

        let payloader = if let Some(seqnum_offset) = initial_seqnum {
            format!("rtpac3pay2 mtu=250 seqnum-offset={seqnum_offset}")
        } else {
            "rtpac3pay2 mtu=250".to_string()
        };

        run_test_pipeline_and_validate_data(
            Source::Buffers(input_caps, input_buffers),
            &payloader,
            "rtpac3depay2",
            expected_pay,
            expected_depay,
            |ac3_data, _, _| {
                if peek_frame_header(ac3_data).is_err() {
                    anyhow::bail!("Expected AC-3 frame header, got {ac3_data:02x?}")
                } else {
                    Ok(())
                }
            },
        );
    }

    for drop_mask in 0..(1 << (6 * 2)) {
        run_ac3_pay_depay_fragmented_with_packet_loss_with_drop_mask(drop_mask, None);
        if drop_mask % 3 == 3 {
            run_ac3_pay_depay_fragmented_with_packet_loss_with_drop_mask(drop_mask, Some(65533));
        }
    }
}

#[test]
fn depay_max_frame_size_caps_and_resyncs() {
    init();

    let caps = gst::Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("clock-rate", 48_000i32)
        .field("encoding-name", "AC3")
        .field("payload", 96i32)
        .build();

    // A real AC3 frame from the fixture, so frame header parsing succeeds.
    let frame = parse_ac3_frames(AC3_DATA)[0].to_vec();

    // Fragment types as this depayer decodes them: 1 or 2 = start, 3 with
    // the marker bit = end, 0 = whole frame.
    let mut good = vec![0x00, 0x00];
    good.extend_from_slice(&frame);
    // The depayer ignores packets with payloads shorter than 8 bytes.
    let mut start = vec![0x01, 0x00];
    start.extend_from_slice(&frame[..16]);
    // Continuations carry real frame bytes so the reassembled header stays
    // parseable: without the cap the end packet would complete an
    // over-large frame instead of it failing to parse.
    let mut cont = vec![0x03, 0x00];
    cont.extend_from_slice(&frame);
    let mut end = vec![0x03, 0x00];
    end.extend_from_slice(&[0xAB; 8]);

    let mut packets: Vec<(&[u8], bool, u32, bool)> = Vec::new();
    // Frame 1 is complete and consumes the base class' initial DISCONT.
    packets.push((good.as_slice(), true, 0, false));
    // Frame 2 dribbles past the reassembly cap and then ends. The cap must
    // drop it, so only frames 1 and 3 come out.
    packets.push((start.as_slice(), false, 2000, false));
    for _ in 0..(MAX_FRAME_SIZE / frame.len() + 2) {
        packets.push((cont.as_slice(), false, 2000, false));
    }
    packets.push((end.as_slice(), true, 2000, false));
    // Frame 3 completes again after the cap fired.
    packets.push((good.as_slice(), true, 4000, false));

    let buffers = crate::tests::run_depay_only("rtpac3depay2", &caps, &packets).unwrap();
    assert_eq!(buffers.len(), 2);
    assert_eq!(buffers[0].0, frame);
    assert_eq!(buffers[1].0, frame);
    assert!(buffers[0].1.contains(gst::BufferFlags::DISCONT));
    // The drops in between must mark frame 3 discontinuous.
    assert!(buffers[1].1.contains(gst::BufferFlags::DISCONT));
}
