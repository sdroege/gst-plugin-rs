// GStreamer RTP MPEG-1/MPEG-2 Video Elementary Stream Payloading - unit tests
//
// Copyright (C) 2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::mpv::mpeg_video_packet::*;

use crate::tests::{ExpectedBuffer, ExpectedPacket, Source, run_test_pipeline};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtp mpv test");
    });
}

// gst-launch-1.0 videotestsrc num-buffers=2
//  ! video/x-raw,framerate=50/1
//  ! timeoverlay font-desc=Sans,36
//  ! avenc_mpeg2video bitrate=5000
//  ! filesink location=vts-320x240_mpeg2.mpv
//
const MP2V_DATA: &[u8] = include_bytes!("vts-320x240-mpeg2.mpv").as_slice();

fn make_buffer(
    data: Vec<u8>,
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

// Check basic MPEG-2 MPV payloading/depayloading
//
#[test]
#[allow(clippy::vec_init_then_push)]
fn rtp_mpv_pay_depay_mpeg2_simple() {
    init();

    let packets = parse_packets_from_slice(MP2V_DATA).expect("packets");

    let second_pic_hdr = packets
        .iter()
        .filter(|p| p.ptype() == PacketType::Picture)
        .nth(1)
        .expect("second frame");

    let (frame1_packets, frame2_packets) = packets.split_at(second_pic_hdr.index());

    let mut frame1 = vec![];
    for p in frame1_packets {
        frame1.extend_from_slice(p.data(MP2V_DATA));
    }

    let mut frame2 = vec![];
    for p in frame2_packets {
        frame2.extend_from_slice(p.data(MP2V_DATA));
    }

    let input_caps = {
        gst::Caps::builder("video/mpeg")
            .field("systemstream", false)
            .field("mpegversion", 2)
            .field("width", 320)
            .field("height", 240)
            .field("framerate", gst::Fraction::new(50, 1))
            .field("parsed", true)
            .build()
    };

    let mut input_buffers = vec![];

    input_buffers.push(make_buffer(
        frame1,
        gst::ClockTime::from_mseconds(0),
        gst::ClockTime::from_mseconds(20),
        gst::BufferFlags::DISCONT,
    ));

    input_buffers.push(make_buffer(
        frame2,
        gst::ClockTime::from_mseconds(20),
        gst::ClockTime::from_mseconds(20),
        gst::BufferFlags::empty(),
    ));

    let mut expected_pay = vec![];

    // First frame is payloaded into 19 RTP packets
    // RTP packet with payload of size  599: Seq Hdr 34, Gop Hdr 8, Pic Hdr 17, UserData 12, Slice 0 512
    // RTP packet with payload of size 1060: Slice 1, 1044
    // RTP packet with payload of size 1200: Slice 2, 2533
    // RTP packet with payload of size 1200
    // RTP packet with payload of size  181
    // RTP packet with payload of size 1200: Slice 3, 1205
    // RTP packet with payload of size   37
    // RTP packet with payload of size 1040: Slice 4, 512; Slice 5: 512
    // RTP packet with payload of size 1040: Slice 6, 512; Slice 7: 512
    // RTP packet with payload of size 1040: Slice 8, 512; Slice 9: 512
    // RTP packet with payload of size  532: Slice 10, 516
    // RTP packet with payload of size 1200: Slice 11, 2055
    // RTP packet with payload of size  887
    // RTP packet with payload of size 1200: Slice 12, 1271
    // RTP packet with payload of size  103
    // RTP packet with payload of size 1200: Slice 13: 1296
    // RTP packet with payload of size  128
    // RTP packet with payload of size 1200: Slice 14: 1287
    // RTP packet with payload of size  119
    {
        let mut expected_rtp_packet_list = vec![];

        expected_rtp_packet_list.push(
            ExpectedPacket::builder()
                .pts(gst::ClockTime::ZERO)
                .flags(gst::BufferFlags::DISCONT)
                .pt(32)
                .rtp_time(0)
                .build(),
        );

        for _ in 1..18 {
            expected_rtp_packet_list.push(
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::ZERO)
                    .flags(gst::BufferFlags::empty())
                    .pt(32)
                    .rtp_time(0)
                    .build(),
            );
        }

        expected_rtp_packet_list.push(
            ExpectedPacket::builder()
                .pts(gst::ClockTime::ZERO)
                .flags(gst::BufferFlags::MARKER)
                .marker_bit(true)
                .pt(32)
                .rtp_time(0)
                .build(),
        );

        expected_pay.push(expected_rtp_packet_list);
    }

    // Second is payloaded into 5 RTP packets
    // RTP packet with payload of size 455: Pic Hdr, UserData, Slice 0-10
    // RTP packet with payload of size 911: Slice 11
    // RTP packet with payload of size 678: Slice 12
    // RTP packet with payload of size 694: Slice 13
    // RTP packet with payload of size 682: Slice 14
    {
        expected_pay.push(
            [
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::from_mseconds(20))
                    .flags(gst::BufferFlags::empty())
                    .pt(32)
                    .rtp_time(1800)
                    .build(),
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::from_mseconds(20))
                    .flags(gst::BufferFlags::empty())
                    .pt(32)
                    .rtp_time(1800)
                    .build(),
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::from_mseconds(20))
                    .flags(gst::BufferFlags::empty())
                    .pt(32)
                    .rtp_time(1800)
                    .build(),
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::from_mseconds(20))
                    .flags(gst::BufferFlags::empty())
                    .pt(32)
                    .rtp_time(1800)
                    .build(),
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::from_mseconds(20))
                    .flags(gst::BufferFlags::MARKER)
                    .marker_bit(true)
                    .pt(32)
                    .rtp_time(1800)
                    .build(),
            ]
            .to_vec(),
        );
    }

    let mut expected_depay = vec![];

    for (i, &plen) in [
        // Frame 1
        583, 1044, 1184, 1184, 165, 1184, 21, 1024, 1024, 1024, 516, 1184, 871, 1184, 87, 1184, 112,
        1184, 103, // Frame 2
        439, 895, 662, 678, 666,
    ]
    .iter()
    .enumerate()
    {
        let expected_flags = match i {
            0 => gst::BufferFlags::DISCONT,
            18 | 23 => gst::BufferFlags::MARKER, // End of frame
            _ => gst::BufferFlags::empty(),
        };
        let expected_ts = match i {
            0..=18 => gst::ClockTime::ZERO,
            19.. => gst::ClockTime::from_mseconds(20),
        };
        expected_depay.push(vec![
            ExpectedBuffer::builder()
                .pts(expected_ts)
                .size(plen)
                .flags(expected_flags)
                .build(),
        ]);
    }

    run_test_pipeline(
        Source::Buffers(input_caps, input_buffers),
        "rtpmpvpay2",
        "rtpmpvdepay2",
        expected_pay,
        expected_depay,
    );
}

// gst-launch-1.0 videotestsrc num-buffers=2
//  ! video/x-raw,framerate=50/1
//  ! timeoverlay font-desc=Sans,36
//  ! mpeg2enc
//  ! filesink location=vts-320x240_mpeg1.mpv
//
const MPV1_DATA: &[u8] = include_bytes!("vts-320x240-mpeg1.mpv").as_slice();

// Check basic MPEG-1 MPV payloading/depayloading
//
#[test]
#[allow(clippy::vec_init_then_push)]
fn rtp_mpv_pay_depay_mpeg1_simple() {
    init();

    let packets = parse_packets_from_slice(MPV1_DATA).expect("packets");

    let second_pic_hdr = packets
        .iter()
        .filter(|p| p.ptype() == PacketType::Picture)
        .nth(1)
        .expect("second frame");

    let (frame1_packets, frame2_packets) = packets.split_at(second_pic_hdr.index());

    let mut frame1 = vec![];
    for p in frame1_packets {
        frame1.extend_from_slice(p.data(MPV1_DATA));
    }

    let mut frame2 = vec![];
    for p in frame2_packets {
        frame2.extend_from_slice(p.data(MPV1_DATA));
    }

    let input_caps = {
        gst::Caps::builder("video/mpeg")
            .field("systemstream", false)
            .field("mpegversion", 1)
            .field("width", 320)
            .field("height", 240)
            .field("framerate", gst::Fraction::new(50, 1))
            .field("parsed", true)
            .build()
    };

    let mut input_buffers = vec![];

    input_buffers.push(make_buffer(
        frame1,
        gst::ClockTime::from_mseconds(0),
        gst::ClockTime::from_mseconds(20),
        gst::BufferFlags::DISCONT,
    ));

    input_buffers.push(make_buffer(
        frame2,
        gst::ClockTime::from_mseconds(20),
        gst::ClockTime::from_mseconds(20),
        gst::BufferFlags::empty(),
    ));

    let mut expected_pay = vec![];

    // First frame is payloaded into 7 RTP packets
    {
        let mut expected_rtp_packet_list = vec![];

        expected_rtp_packet_list.push(
            ExpectedPacket::builder()
                .pts(gst::ClockTime::ZERO)
                .flags(gst::BufferFlags::DISCONT)
                .pt(32)
                .rtp_time(0)
                .build(),
        );

        for _ in 1..6 {
            expected_rtp_packet_list.push(
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::ZERO)
                    .flags(gst::BufferFlags::empty())
                    .pt(32)
                    .rtp_time(0)
                    .build(),
            );
        }

        expected_rtp_packet_list.push(
            ExpectedPacket::builder()
                .pts(gst::ClockTime::ZERO)
                .flags(gst::BufferFlags::MARKER)
                .marker_bit(true)
                .pt(32)
                .rtp_time(0)
                .build(),
        );

        expected_pay.push(expected_rtp_packet_list);
    }

    // Second is payloaded into 2 RTP packets
    {
        expected_pay.push(
            [
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::from_mseconds(20))
                    .flags(gst::BufferFlags::empty())
                    .pt(32)
                    .rtp_time(1800)
                    .build(),
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::from_mseconds(20))
                    .flags(gst::BufferFlags::MARKER)
                    .marker_bit(true)
                    .pt(32)
                    .rtp_time(1800)
                    .build(),
            ]
            .to_vec(),
        );
    }

    let mut expected_depay = vec![];

    for (i, &plen) in [
        // Frame 1
        496, 1107, 1003, 435, 767, 994, 488, // Frame 2
        929, 693,
    ]
    .iter()
    .enumerate()
    {
        let expected_flags = match i {
            0 => gst::BufferFlags::DISCONT,
            6 | 8 => gst::BufferFlags::MARKER, // End of frame
            _ => gst::BufferFlags::empty(),
        };
        let expected_ts = match i {
            0..=6 => gst::ClockTime::ZERO,
            7.. => gst::ClockTime::from_mseconds(20),
        };
        expected_depay.push(vec![
            ExpectedBuffer::builder()
                .pts(expected_ts)
                .size(plen)
                .flags(expected_flags)
                .build(),
        ]);
    }

    run_test_pipeline(
        Source::Buffers(input_caps, input_buffers),
        "rtpmpvpay2",
        "rtpmpvdepay2",
        expected_pay,
        expected_depay,
    );
}
