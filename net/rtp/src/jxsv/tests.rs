// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use super::picture_segment::PICTURE_SEGMENT_PREFIX_SIZE;
use crate::tests::{
    ExpectedBuffer, ExpectedPacket, Liveness, Source, run_test_pipeline,
    run_test_pipeline_full_and_validate_data,
};
use gst::prelude::*;
use gst::subclass::prelude::*;

const RTP_HEADER_SIZE: usize = 12;
// RFC 9134 JPEG XS RTP payload header size (`PayloadHeader::SIZE`).
const JXSV_PAYLOAD_HEADER_SIZE: usize = 4;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtpjxsv test");
    });
}

fn jxsc_caps() -> gst::Caps {
    gst::Caps::builder("image/x-jxsc")
        .field("alignment", "frame")
        .field("interlace-mode", "progressive")
        .field("width", 640i32)
        .field("height", 480i32)
        .field("depth", 10i32)
        .field("sampling", "YCbCr-4:2:2")
        .field("framerate", gst::Fraction::new(25, 1))
        .build()
}

/// Minimal JPEG XS header: SOC + PIH with unset Ppih/Plev, then `payload`.
fn make_codestream(payload: Vec<u8>) -> Vec<u8> {
    let mut data = vec![
        0xff, 0x10, // SOC
        0xff, 0x12, // PIH
        0x00, 0x0c, // Lpih
        0x00, 0x00, 0x01, 0x00, // Lcod
        0x00, 0x00, // Ppih
        0x00, 0x00, // Plev
    ];
    data.extend_from_slice(&payload);
    data
}

fn make_buffer(data: Vec<u8>, pts_ms: u64) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(data);
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(gst::ClockTime::from_mseconds(pts_ms));
    }
    buffer
}

#[test]
fn test_jxsv_single_packet_frame() {
    init();

    let frame_data = make_codestream(vec![0xAB; 512]);
    let frame_len = frame_data.len();
    let picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame_len;
    let src = Source::Buffers(jxsc_caps(), vec![make_buffer(frame_data, 0)]);

    let pay = "rtpjxsvpay mtu=1400";
    let depay = "rtpjxsvdepay";

    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            .pt(96)
            .rtp_time(0)
            .marker_bit(true)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + picture_segment_len)
            .build(),
    ]];

    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .size(frame_len)
            .flags(gst::BufferFlags::DISCONT)
            .build(),
    ]];

    run_test_pipeline(src, pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_jxsv_multi_packet_frame() {
    init();

    let frame_data = make_codestream(vec![0xCD; 3000]);
    let frame_len = frame_data.len();
    let picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame_len;
    let src = Source::Buffers(jxsc_caps(), vec![make_buffer(frame_data, 0)]);

    let pay = "rtpjxsvpay mtu=1400";
    let depay = "rtpjxsvdepay";

    let payload_size = 1400 - RTP_HEADER_SIZE - JXSV_PAYLOAD_HEADER_SIZE;
    let last_payload_size = picture_segment_len - 2 * payload_size;
    assert!(last_payload_size <= payload_size);

    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::DISCONT)
            .pt(96)
            .rtp_time(0)
            .marker_bit(false)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + payload_size)
            .build(),
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::empty())
            .pt(96)
            .rtp_time(0)
            .marker_bit(false)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + payload_size)
            .build(),
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::MARKER)
            .pt(96)
            .rtp_time(0)
            .marker_bit(true)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + last_payload_size)
            .build(),
    ]];

    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .size(frame_len)
            .flags(gst::BufferFlags::DISCONT)
            .build(),
    ]];

    run_test_pipeline(src, pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_jxsv_two_frames() {
    init();

    let frame1 = make_codestream(vec![0x11; 800]);
    let frame2 = make_codestream(vec![0x22; 900]);
    let frame1_len = frame1.len();
    let frame2_len = frame2.len();
    let frame1_picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame1_len;
    let frame2_picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame2_len;
    let src = Source::Buffers(
        jxsc_caps(),
        vec![make_buffer(frame1, 0), make_buffer(frame2, 40)],
    );

    let pay = "rtpjxsvpay mtu=1400";
    let depay = "rtpjxsvdepay";

    let expected_pay = vec![
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(0)
                .marker_bit(true)
                .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + frame1_picture_segment_len)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(3600)
                .marker_bit(true)
                .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + frame2_picture_segment_len)
                .build(),
        ],
    ];

    let expected_depay = vec![
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .size(frame1_len)
                .flags(gst::BufferFlags::DISCONT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .size(frame2_len)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
    ];

    run_test_pipeline(src, pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_jxsv_packet_loss() {
    init();

    let frame_data = make_codestream(vec![0xEF; 3000]);
    let frame_len = frame_data.len();
    let picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame_len;
    let src = Source::Buffers(jxsc_caps(), vec![make_buffer(frame_data, 0)]);

    let pay = "rtpjxsvpay mtu=1400";
    let depay = "rtpjxsvdepay";

    let payload_size = 1400 - RTP_HEADER_SIZE - JXSV_PAYLOAD_HEADER_SIZE;
    let last_payload_size = picture_segment_len - 2 * payload_size;
    assert!(last_payload_size <= payload_size);

    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::DISCONT)
            .pt(96)
            .rtp_time(0)
            .marker_bit(false)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + payload_size)
            .drop(true)
            .build(),
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::empty())
            .pt(96)
            .rtp_time(0)
            .marker_bit(false)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + payload_size)
            .build(),
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::MARKER)
            .pt(96)
            .rtp_time(0)
            .marker_bit(true)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + last_payload_size)
            .build(),
    ]];

    let expected_depay: Vec<Vec<ExpectedBuffer>> = vec![];

    run_test_pipeline(src, pay, depay, expected_pay, expected_depay);
}

fn jxsc_caps_with_profile() -> gst::Caps {
    gst::Caps::builder("image/x-jxsc")
        .field("alignment", "frame")
        .field("interlace-mode", "progressive")
        .field("width", 640i32)
        .field("height", 480i32)
        .field("depth", 10i32)
        .field("sampling", "YCbCr-4:2:2")
        .field("framerate", gst::Fraction::new(25, 1))
        .field("colorimetry", "bt709")
        .field("profile", "Main422.10")
        .field("level", "1k-1")
        .field("sublevel", "Full")
        .build()
}

#[test]
fn test_jxsv_profile_level_sublevel_roundtrip() {
    init();

    let frame_data = make_codestream(vec![0xAB; 512]);
    let expected_codestream = frame_data.clone();
    let frame_len = frame_data.len();
    let picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame_len;
    let src = Source::Buffers(jxsc_caps_with_profile(), vec![make_buffer(frame_data, 0)]);

    let pay = "rtpjxsvpay mtu=1400";
    let depay = "rtpjxsvdepay";

    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            .pt(96)
            .rtp_time(0)
            .marker_bit(true)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + picture_segment_len)
            .build(),
    ]];

    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .size(frame_len)
            .flags(gst::BufferFlags::DISCONT)
            .build(),
    ]];

    let expected_depay_caps = gst::Caps::builder("image/x-jxsc")
        .field("alignment", "frame")
        .field("interlace-mode", "progressive")
        .field("width", 640i32)
        .field("height", 480i32)
        .field("depth", 10i32)
        .field("sampling", "YCbCr-4:2:2")
        .field("colorimetry", "bt709")
        .field("framerate", gst::Fraction::new(25, 1))
        .field("profile", "Main422.10")
        .field("level", "1k-1")
        .field("sublevel", "Full")
        .build();

    run_test_pipeline_full_and_validate_data(
        src,
        pay,
        depay,
        expected_pay,
        expected_depay,
        Some(expected_depay_caps),
        Liveness::NonLive,
        move |data, _, _| {
            assert_eq!(data, expected_codestream.as_slice());
            Ok(())
        },
    );
}

#[test]
fn test_jxsv_depay_rejects_unsupported_packetmode_and_transmode() {
    init();

    use crate::basedepay::RtpBaseDepay2Impl;

    let try_caps = |packetmode: Option<&str>, transmode: Option<&str>| -> bool {
        let depay = gst::ElementFactory::make("rtpjxsvdepay")
            .build()
            .unwrap()
            .downcast::<super::depay::RtpJxsvDepay>()
            .unwrap();

        let mut caps = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", crate::jxsv::RTP_ENCODING_NAME)
            .field("clock-rate", 90_000i32);
        if let Some(packetmode) = packetmode {
            caps = caps.field("packetmode", packetmode);
        }
        if let Some(transmode) = transmode {
            caps = caps.field("transmode", transmode);
        }
        RtpBaseDepay2Impl::set_sink_caps(depay.imp(), &caps.build())
    };

    assert!(!try_caps(Some("1"), None));
    assert!(!try_caps(Some("0"), Some("0")));
    assert!(try_caps(None, None));
    assert!(try_caps(Some("0"), Some("1")));
}

#[test]
fn test_jxsv_rtp_encoding_name_is_uppercase_for_gst_sdp() {
    // gst_sdp_media_get_caps_from_media upper-cases the rtpmap encoding-name
    // before storing it on application/x-rtp caps. Pad templates must use
    // that same spelling (as rtpvraw uses RAW) because caps intersection is
    // case-sensitive — otherwise udpsrc ! rtpjxsvdepay fails to link.
    init();

    let depay = gst::ElementFactory::make("rtpjxsvdepay").build().unwrap();
    let sink_tmpl = depay
        .static_pad("sink")
        .expect("sink")
        .pad_template()
        .expect("sink template");
    let sink_caps = sink_tmpl.caps();

    let gst_sdp_style = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "JXSV")
        .field("clock-rate", 90_000i32)
        .build();
    let rfc9134_lowercase = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "jxsv")
        .field("clock-rate", 90_000i32)
        .build();

    assert!(
        sink_caps.can_intersect(&gst_sdp_style),
        "depay sink must accept gst-sdp upper-case JXSV: {sink_caps}"
    );
    assert!(
        !sink_caps.can_intersect(&rfc9134_lowercase),
        "lowercase jxsv must not intersect (caps are case-sensitive): {sink_caps}"
    );

    let pay = gst::ElementFactory::make("rtpjxsvpay").build().unwrap();
    let src_tmpl = pay
        .static_pad("src")
        .expect("src")
        .pad_template()
        .expect("src template");
    let src_caps = src_tmpl.caps();
    let s = src_caps.structure(0).expect("pay src structure");
    assert_eq!(
        s.get::<&str>("encoding-name").unwrap(),
        crate::jxsv::RTP_ENCODING_NAME,
        "payloader src template must advertise upper-case JXSV"
    );
}

fn jxsc_caps_with_colorimetry(colorimetry: &str) -> gst::Caps {
    gst::Caps::builder("image/x-jxsc")
        .field("alignment", "frame")
        .field("interlace-mode", "progressive")
        .field("width", 640i32)
        .field("height", 480i32)
        .field("depth", 10i32)
        .field("sampling", "YCbCr-4:2:2")
        .field("framerate", gst::Fraction::new(25, 1))
        .field("colorimetry", colorimetry)
        .field("profile", "Main422.10")
        .field("level", "1k-1")
        .field("sublevel", "Full")
        .build()
}

#[test]
fn test_jxsv_pay_depay_colorimetry_tcs_range() {
    // Pay must put RFC 9134 colorimetry / TCS / RANGE on application/x-rtp;
    // depay must consume all three (not colorimetry alone).
    init();

    use gst_check::Harness;
    use gst_video::{
        VideoColorMatrix, VideoColorPrimaries, VideoColorRange, VideoColorimetry,
        VideoTransferFunction,
    };
    use std::str::FromStr;

    let mut h_pay = Harness::new("rtpjxsvpay");
    h_pay.play();
    let mut h_depay = Harness::new("rtpjxsvdepay");
    h_depay.play();

    // BT2100 + PQ: TCS must be signalled (RFC 9134 has no BT2100 omission default).
    h_pay.set_src_caps(jxsc_caps_with_colorimetry("bt2100-pq"));
    let rtp = h_pay
        .sinkpad()
        .expect("pay harness sinkpad")
        .current_caps()
        .expect("pay src caps after set_src_caps");
    let s = rtp.structure(0).unwrap();
    assert_eq!(s.get::<&str>("colorimetry").unwrap(), "BT2100");
    assert_eq!(s.get::<&str>("tcs").unwrap(), "PQ");
    assert_eq!(s.get::<&str>("range").unwrap(), "NARROW");

    h_depay.set_src_caps(rtp);
    let jxsc = h_depay
        .sinkpad()
        .expect("depay harness sinkpad")
        .current_caps()
        .expect("depay src caps after set_src_caps");
    assert_eq!(
        jxsc.structure(0)
            .unwrap()
            .get::<&str>("colorimetry")
            .unwrap(),
        "bt2100-pq"
    );

    // FULL range: RANGE must be signalled (omission default is NARROW).
    let full = VideoColorimetry::new(
        VideoColorRange::Range0_255,
        VideoColorMatrix::Bt709,
        VideoTransferFunction::Bt709,
        VideoColorPrimaries::Bt709,
    )
    .to_string();
    h_pay.set_src_caps(jxsc_caps_with_colorimetry(&full));
    let rtp = h_pay
        .sinkpad()
        .expect("pay harness sinkpad")
        .current_caps()
        .expect("pay src caps after FULL colorimetry");
    let s = rtp.structure(0).unwrap();
    assert_eq!(s.get::<&str>("colorimetry").unwrap(), "BT709");
    assert_eq!(s.get::<&str>("tcs").unwrap(), "SDR");
    assert_eq!(s.get::<&str>("range").unwrap(), "FULL");

    h_depay.set_src_caps(rtp);
    let jxsc = h_depay
        .sinkpad()
        .expect("depay harness sinkpad")
        .current_caps()
        .expect("depay src caps after FULL range");
    let out = jxsc
        .structure(0)
        .unwrap()
        .get::<&str>("colorimetry")
        .unwrap();
    assert_eq!(
        VideoColorimetry::from_str(out).unwrap().range(),
        VideoColorRange::Range0_255
    );

    drop(h_pay);
    drop(h_depay);
}
