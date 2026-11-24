//
// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

use crate::h263p::depay::imp::MAX_FRAME_SIZE;
use crate::tests::{
    ExpectedBuffer, ExpectedPacket, Source, run_test_pipeline,
    run_test_pipeline_and_validate_buffer, source_first_pts,
};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtph263p test");
    });
}

/// Build a synthetic H.263+ picture: an intra picture header followed by
/// `num_gobs` GOB segments, each with `filler_len` bytes of payload.
fn make_frame(num_gobs: usize, filler_len: usize) -> Vec<u8> {
    let mut frame = vec![
        0x00, 0x00, 0x80, 0x06, // PSC (22 bits) + TR=1 + PTYPE bits 1-2 ('10')
        0x08, 0x00, // PTYPE rest: SRC='010' (QCIF), INTRA
    ];

    for gob in 1..=num_gobs {
        // GOB header: GBSC (17 bits) + GN (5 bits) + GFID (2 bits); the
        // fourth byte starts with GQUANT.
        frame.extend_from_slice(&[0x00, 0x00, 0x80 | (gob as u8) << 2 | 0x03, 0x01]);
        for i in 0..filler_len {
            // Never zero so that no accidental start codes appear
            frame.push((i as u8) | 0x01);
        }
    }

    frame
}

fn h263_caps() -> gst::Caps {
    gst::Caps::builder("video/x-h263")
        .field("variant", "itu")
        .field("h263version", "h263p")
        .field("parsed", true)
        .build()
}

/// Expected packets for one frame of `frame_len` bytes payload-fragmented
/// with follow-on packets and the given MTU. The MTU covers the complete RTP
/// packet, including the 12-byte RTP header and the 2-byte payload header.
fn expected_follow_on_packets(
    pts: gst::ClockTime,
    rtp_time: u32,
    frame_len: usize,
    mtu: usize,
    discont: bool,
    drop_first: bool,
) -> Vec<ExpectedPacket> {
    let max_payload = mtu - 12 - 2;
    let payload_len = frame_len - 2;
    let num_packets = payload_len.div_ceil(max_payload);

    (0..num_packets)
        .map(|i| {
            let len = std::cmp::min(payload_len - i * max_payload, max_payload);
            let mut flags = gst::BufferFlags::empty();
            if i == 0 && discont {
                flags |= gst::BufferFlags::DISCONT;
            }
            if i + 1 == num_packets {
                flags |= gst::BufferFlags::MARKER;
            }
            ExpectedPacket::builder()
                .pts(pts)
                .rtp_time(rtp_time)
                .flags(flags)
                .marker_bit(i + 1 == num_packets)
                .size(len + 12 + 2)
                .drop(drop_first && i == 0)
                .build()
        })
        .collect()
}

/// The depayloaded buffer must contain the original frame byte-exactly.
fn assert_frame_roundtrip(frame: &[u8], buffer: &gst::Buffer) -> anyhow::Result<()> {
    let data = buffer.map_readable()?.as_slice().to_vec();

    anyhow::ensure!(
        data == frame,
        "Depayloaded data does not match input frame: {} bytes vs {} bytes",
        data.len(),
        frame.len()
    );

    Ok(())
}

#[test]
fn roundtrip_follow_on() {
    init();

    let frame = make_frame(2, 100); // 214 bytes
    let mtu = 100usize;

    let mut buf1 = gst::Buffer::from_slice(frame.clone());
    buf1.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);

    let src = Source::Buffers(h263_caps(), vec![buf1]);
    let expected_pay = vec![expected_follow_on_packets(
        gst::ClockTime::ZERO,
        0,
        frame.len(),
        mtu,
        true,
        false,
    )];
    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::ZERO)
            .flags(gst::BufferFlags::DISCONT)
            .size(frame.len())
            .build(),
    ]];

    let frame_clone = frame.clone();
    run_test_pipeline_and_validate_buffer(
        src,
        &format!("rtph263ppay2 mtu={mtu}"),
        "rtph263pdepay2",
        expected_pay,
        expected_depay,
        move |buffer, _i, _j| assert_frame_roundtrip(&frame_clone, buffer),
    );
}

#[test]
fn roundtrip_segments() {
    init();

    // Segments: 6-byte picture header, then 3 GOB segments of 104 bytes each;
    // with MTU 100 (86 usable payload bytes) the picture header segment fits
    // alone and each GOB needs two packets.
    let frame = make_frame(3, 100);
    let mtu = 100usize;

    let mut buf1 = gst::Buffer::from_slice(frame.clone());
    buf1.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);

    let src = Source::Buffers(h263_caps(), vec![buf1]);

    let segments = {
        let sizes = [18usize, 100, 30, 100, 30, 100, 30];
        let last = sizes.len() - 1;
        let mut v = Vec::new();
        for (i, size) in sizes.into_iter().enumerate() {
            let mut flags = gst::BufferFlags::empty();
            if i == 0 {
                flags |= gst::BufferFlags::DISCONT;
            }
            if i == last {
                flags |= gst::BufferFlags::MARKER;
            }
            v.push(
                ExpectedPacket::builder()
                    .pts(gst::ClockTime::ZERO)
                    .flags(flags)
                    .size(size)
                    .marker_bit(i == last)
                    .build(),
            );
        }
        v
    };
    let expected_pay = vec![segments];
    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::ZERO)
            .flags(gst::BufferFlags::DISCONT)
            .size(frame.len())
            .build(),
    ]];

    let frame_clone = frame.clone();
    run_test_pipeline_and_validate_buffer(
        src,
        &format!("rtph263ppay2 mtu={mtu} fragmentation-mode=sync"),
        "rtph263pdepay2",
        expected_pay,
        expected_depay,
        move |buffer, _i, _j| assert_frame_roundtrip(&frame_clone, buffer),
    );
}

#[test]
fn roundtrip_with_packet_loss() {
    init();

    let frame1 = make_frame(2, 100);
    let frame2 = make_frame(1, 50);
    let mtu = 100usize;

    let mut buf1 = gst::Buffer::from_slice(frame1.clone());
    buf1.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    let mut buf2 = gst::Buffer::from_slice(frame2.clone());
    buf2.get_mut()
        .unwrap()
        .set_pts(gst::ClockTime::from_mseconds(100));

    let src = Source::Buffers(h263_caps(), vec![buf1, buf2]);

    // Drop the first packet of the first frame: the depayloader must discard
    // the frame and output only the second one.
    let expected_pay = vec![
        expected_follow_on_packets(gst::ClockTime::ZERO, 0, frame1.len(), mtu, true, true),
        expected_follow_on_packets(
            gst::ClockTime::from_mseconds(100),
            9000,
            frame2.len(),
            mtu,
            false,
            false,
        ),
    ];
    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_mseconds(100))
            .flags(gst::BufferFlags::DISCONT)
            .size(frame2.len())
            .build(),
    ]];

    run_test_pipeline_and_validate_buffer(
        src,
        &format!("rtph263ppay2 mtu={mtu}"),
        "rtph263pdepay2",
        expected_pay,
        expected_depay,
        move |buffer, _i, _j| assert_frame_roundtrip(&frame2, buffer),
    );
}

/// A whole picture that fits into a single packet (RFC 4629 section 6.1.1):
/// one packet with P=1 and the marker bit set.
#[test]
fn roundtrip_single_packet() {
    init();

    let frame = make_frame(1, 20); // 30 bytes, fits in the default MTU

    let mut buf = gst::Buffer::from_slice(frame.clone());
    buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);

    let src = Source::Buffers(h263_caps(), vec![buf]);
    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::ZERO)
            .rtp_time(0)
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            .marker_bit(true)
            .size(frame.len() - 2 + 12 + 2)
            .build(),
    ]];
    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::ZERO)
            .flags(gst::BufferFlags::DISCONT)
            .size(frame.len())
            .build(),
    ]];

    let frame_clone = frame.clone();
    run_test_pipeline_and_validate_buffer(
        src,
        "rtph263ppay2",
        "rtph263pdepay2",
        expected_pay,
        expected_depay,
        move |buffer, _i, _j| assert_frame_roundtrip(&frame_clone, buffer),
    );
}

/// RTP sink caps for the depayloader.
fn depay_rtp_caps(encoding_name: &str, extra: &[(&str, &str)]) -> gst::Caps {
    let mut s = gst::Structure::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", encoding_name)
        .field("clock-rate", 90_000i32)
        .field("payload", 96i32)
        .build();
    for (key, value) in extra {
        s.set_value(*key, glib::SendValue::from(*value));
    }
    gst::Caps::builder_full().structure(s).build()
}

/// Feed RTP packets through the depayloader alone. Returns the negotiated
/// srcpad caps and the depayloaded buffers.
fn run_depay_only(
    rtp_caps: &gst::Caps,
    packets: &[(&[u8], bool, u32)],
) -> Result<(gst::Caps, Vec<Vec<u8>>), anyhow::Error> {
    init();

    let buffers: Vec<gst::Buffer> = packets
        .iter()
        .enumerate()
        .map(|(i, (payload, marker, ts))| {
            let bytes = rtp_types::RtpPacketBuilder::new()
                .payload_type(96)
                .sequence_number(i as u16)
                .timestamp(*ts)
                .ssrc(0x1234)
                .marker_bit(*marker)
                .payload(*payload)
                .write_vec()
                .unwrap();
            gst::Buffer::from_slice(bytes)
        })
        .collect();
    let mut buffers = buffers.into_iter();

    let appsrc = gst_app::AppSrc::builder()
        .format(gst::Format::Time)
        .caps(rtp_caps)
        .callbacks(
            gst_app::AppSrcCallbacks::builder()
                .need_data(move |appsrc, _offset| {
                    let Some(buffer) = buffers.next() else {
                        let _ = appsrc.end_of_stream();
                        return;
                    };
                    // Errors are collected from the bus below.
                    let _ = appsrc.push_buffer(buffer);
                })
                .build(),
        )
        .build();

    let pipeline = gst::Pipeline::new();
    let depay = gst::ElementFactory::make("rtph263pdepay2").build().unwrap();
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap();
    let appsrc = appsrc.upcast::<gst::Element>();
    pipeline.add_many([&appsrc, &depay, &sink]).unwrap();
    appsrc.link(&depay).unwrap();
    depay.link(&sink).unwrap();

    let out_caps = std::sync::Arc::new(std::sync::Mutex::new(None::<gst::Caps>));
    let out_bufs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    depay.static_pad("src").unwrap().add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        {
            let out_caps = out_caps.clone();
            let out_bufs = out_bufs.clone();
            move |pad, info| {
                let bufs: Vec<gst::Buffer> = match info.buffer_list() {
                    Some(list) => (0..list.len())
                        .filter_map(|i| list.get(i).map(|b| b.to_owned()))
                        .collect(),
                    None => info.buffer().map(|b| vec![b.clone()]).unwrap_or_default(),
                };
                let mut out_caps = out_caps.lock().unwrap();
                if out_caps.is_none() {
                    *out_caps = pad.current_caps();
                }
                drop(out_caps);

                let mut out = out_bufs.lock().unwrap();
                for buf in bufs {
                    out.push(buf.map_readable().unwrap().as_slice().to_vec());
                }
                gst::PadProbeReturn::Ok
            }
        },
    );

    pipeline.set_state(gst::State::Playing).unwrap();
    let msg = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Error, gst::MessageType::Eos],
        )
        .ok_or_else(|| anyhow::anyhow!("no EOS/error from depay pipeline"))?;
    let _ = pipeline.set_state(gst::State::Null);

    if msg.type_() == gst::MessageType::Error {
        return Err(anyhow::anyhow!("depay pipeline failed: {msg:?}"));
    }

    let caps = out_caps
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no caps negotiated"))?;
    Ok((caps, std::mem::take(&mut *out_bufs.lock().unwrap())))
}

/// Runs `appsrc ! rtph263ppay2 [! capsfilter] ! fakesink` with the given
/// input caps and returns the caps the payloader negotiated on its src pad,
/// or an error if the pipeline failed.
fn run_pay_only(
    input_caps: &gst::Caps,
    downstream_caps: Option<&gst::Caps>,
) -> Result<gst::Caps, anyhow::Error> {
    init();

    let mut frame_buf = gst::Buffer::from_slice(make_frame(1, 20));
    frame_buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    let mut buffers = vec![frame_buf].into_iter();

    let appsrc = gst_app::AppSrc::builder()
        .format(gst::Format::Time)
        .caps(input_caps)
        .callbacks(
            gst_app::AppSrcCallbacks::builder()
                .need_data(move |appsrc, _offset| {
                    let Some(buffer) = buffers.next() else {
                        let _ = appsrc.end_of_stream();
                        return;
                    };
                    let _ = appsrc.push_buffer(buffer);
                })
                .build(),
        )
        .build();

    let pipeline = gst::Pipeline::new();
    let pay = gst::ElementFactory::make("rtph263ppay2").build().unwrap();
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap();
    let appsrc = appsrc.upcast::<gst::Element>();
    pipeline.add_many([&appsrc, &pay, &sink]).unwrap();
    appsrc.link(&pay).unwrap();
    if let Some(caps) = downstream_caps {
        let filter = gst::ElementFactory::make("capsfilter")
            .property("caps", caps)
            .build()
            .unwrap();
        pipeline.add(&filter).unwrap();
        pay.link(&filter).unwrap();
        filter.link(&sink).unwrap();
    } else {
        pay.link(&sink).unwrap();
    }

    let out_caps = std::sync::Arc::new(std::sync::Mutex::new(None::<gst::Caps>));
    pay.static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, {
            let out_caps = out_caps.clone();
            move |pad, _info| {
                out_caps
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| pad.current_caps().expect("no caps on pay src pad"));
                gst::PadProbeReturn::Ok
            }
        });

    pipeline.set_state(gst::State::Playing).unwrap();
    let msg = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Error, gst::MessageType::Eos],
        )
        .ok_or_else(|| anyhow::anyhow!("no EOS/error from pay pipeline"))?;
    let _ = pipeline.set_state(gst::State::Null);

    if msg.type_() == gst::MessageType::Error {
        return Err(anyhow::anyhow!("pay pipeline failed: {msg:?}"));
    }

    out_caps
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no caps negotiated"))
}

/// The payloader src caps follow the stream: encoding-name from
/// `h263version`, annex parameters from parser-provided `annex-*` fields
/// (RFC 4629 section 8.1).
#[test]
fn pay_src_caps_negotiation() {
    init();

    for (version, annexes, name, params) in [
        ("h263", &[][..], "H263-1998", &[][..]),
        ("h263p", &[][..], "H263-1998", &[]),
        ("h263pp", &[][..], "H263-2000", &[]),
        (
            "h263p",
            &[("annex-f", true)][..],
            "H263-1998",
            &[("f", "1")][..],
        ),
        (
            "h263p",
            &[("annex-i", true), ("annex-n", true)][..],
            "H263-1998",
            &[("i", "1"), ("n", "1")][..],
        ),
        (
            "h263pp",
            &[("annex-k", true)][..],
            "H263-2000",
            &[("k", "1")][..],
        ),
    ] {
        let mut builder = gst::Caps::builder("video/x-h263")
            .field("variant", "itu")
            .field("parsed", true)
            .field("h263version", version);
        for (field, set) in annexes {
            builder = builder.field(*field, *set);
        }
        let out = run_pay_only(&builder.build(), None).unwrap();
        let s = out.structure(0).unwrap();
        assert_eq!(
            s.get::<&str>("encoding-name").unwrap(),
            name,
            "version={version}"
        );
        for (param, value) in params {
            assert_eq!(
                s.get::<&str>(*param).unwrap(),
                *value,
                "version={version} annexes={annexes:?}"
            );
        }
        for param in ["f", "i", "j", "t", "k", "n"] {
            if !params.iter().any(|(p, _)| *p == param) {
                assert!(s.get::<&str>(param).is_err(), "{param} must be absent");
            }
        }
    }
}

/// The payloader needs one complete picture per buffer, so unparsed input is
/// rejected at negotiation.
#[test]
fn pay_rejects_unparsed_input() {
    init();

    let caps = gst::Caps::builder("video/x-h263")
        .field("variant", "itu")
        .field("h263version", "h263p")
        .build();

    let err = run_pay_only(&caps, None).unwrap_err();
    assert!(
        format!("{err}").contains("not-negotiated"),
        "expected a negotiation failure, got: {err}"
    );
}

/// A downstream pinned to a contradicting encoding-name (e.g. an RTP
/// session negotiated from SDP) must fail on the first buffer: the
/// payloader pushes its caps, downstream rejects them, and the resulting
/// flow error stops the pipeline.
#[test]
fn pay_conflicting_downstream_fails_on_first_buffer() {
    init();

    let caps = gst::Caps::builder("video/x-h263")
        .field("variant", "itu")
        .field("h263version", "h263p")
        .build();
    assert!(
        run_pay_only(
            &caps,
            Some(
                &gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("encoding-name", "H263-2000")
                    .field("clock-rate", 90_000i32)
                    .build(),
            ),
        )
        .is_err()
    );
}

/// The src caps must distinguish baseline, H.263+ and H.263++ streams
/// (RFC 4629 sections 8.1 and 9).
#[test]
fn depay_src_caps_negotiation() {
    init();

    let frame = make_frame(1, 20);
    let mut payload = vec![0x04, 0x00]; // P=1
    payload.extend_from_slice(&frame[2..]);
    let packets = [(&payload[..], true, 0)];

    for (name, extra, version) in [
        ("H263-1998", &[][..], "h263"),
        ("H263-1998", &[("f", "1")][..], "h263p"),
        ("H263-1998", &[("i", "1")][..], "h263p"),
        ("H263-1998", &[("j", "1")][..], "h263p"),
        ("H263-1998", &[("t", "1")][..], "h263p"),
        ("H263-1998", &[("k", "2")][..], "h263p"),
        ("H263-1998", &[("n", "1")][..], "h263p"),
        ("H263-1998", &[("p", "1")][..], "h263p"),
        ("H263-2000", &[][..], "h263pp"),
    ] {
        let caps = depay_rtp_caps(name, extra);
        let (out_caps, bufs) = run_depay_only(&caps, &packets).unwrap();
        let s = out_caps.structure(0).unwrap();
        assert_eq!(
            s.get::<&str>("h263version").unwrap(),
            version,
            "encoding-name={name} extra={extra:?}"
        );
        assert_eq!(bufs.len(), 1, "encoding-name={name} extra={extra:?}");
    }

    // An unsupported encoding-name must fail negotiation.
    assert!(run_depay_only(&depay_rtp_caps("H261", &[]), &packets).is_err());
}

/// A packet with the V bit and PLEN>0: the VRC byte and the extra picture
/// header are payload header, not bitstream data (RFC 4629 sections 5 and
/// 6.1).
#[test]
fn depay_skips_vrc_and_extra_picture_header() {
    init();

    let frame = make_frame(1, 20);

    // P=1, V=1, PLEN=2, PEBIT=3 -> [0x06, 0x13], then VRC byte and the
    // 2-byte extra picture header.
    let mut payload = vec![0x06, 0x13, 0x55, 0xAA, 0xBB];
    payload.extend_from_slice(&frame[2..]);

    let (_, bufs) = run_depay_only(
        &depay_rtp_caps("H263-1998", &[]),
        &[(&payload[..], true, 0)],
    )
    .expect("depay failed");
    assert_eq!(bufs.len(), 1);

    assert_eq!(bufs[0], frame);
}

/// PLEN>0 is also allowed on follow-on packets (RFC 4629 section 6.2).
#[test]
fn depay_follow_on_with_extra_picture_header() {
    init();

    let frame = make_frame(1, 20); // 30 bytes

    let mut p1 = vec![0x04, 0x00]; // P=1
    p1.extend_from_slice(&frame[2..10]);
    // P=0, PLEN=1, PEBIT=0 -> [0x00, 0x08], then 1 extra header byte.
    let mut p2 = vec![0x00, 0x08, 0xCC];
    p2.extend_from_slice(&frame[10..]);

    let packets = [(&p1[..], false, 0), (&p2[..], true, 0)];
    let (_, bufs) =
        run_depay_only(&depay_rtp_caps("H263-1998", &[]), &packets).expect("depay failed");
    assert_eq!(bufs.len(), 1);

    assert_eq!(bufs[0], frame);
}

/// The payloader must refuse input that does not start with a start code.
#[test]
fn pay_rejects_buffer_without_startcode() {
    init();

    let mut pushed = false;
    let appsrc = gst_app::AppSrc::builder()
        .format(gst::Format::Time)
        .caps(&h263_caps())
        .callbacks(
            gst_app::AppSrcCallbacks::builder()
                .need_data(move |appsrc, _offset| {
                    if pushed {
                        let _ = appsrc.end_of_stream();
                        return;
                    }
                    pushed = true;
                    let mut buf = gst::Buffer::from_slice([0x11u8, 0x22, 0x33, 0x44]);
                    buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
                    let _ = appsrc.push_buffer(buf);
                })
                .build(),
        )
        .build();

    let pipeline = gst::Pipeline::new();
    let pay = gst::ElementFactory::make("rtph263ppay2").build().unwrap();
    let sink = gst::ElementFactory::make("fakesink").build().unwrap();
    let appsrc = appsrc.upcast::<gst::Element>();
    pipeline.add_many([&appsrc, &pay, &sink]).unwrap();
    appsrc.link(&pay).unwrap();
    pay.link(&sink).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();
    let msg = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(gst::ClockTime::from_seconds(10), &[gst::MessageType::Error]);
    let _ = pipeline.set_state(gst::State::Null);

    assert!(
        msg.is_some(),
        "expected a pipeline error for a buffer without start code"
    );
}

/// Sequence-ending segments (EOS, EOSBS) must get a packet of their own,
/// even when they would fit into the preceding segment packet (RFC 4629
/// section 6.1.3). EOS third byte is 0xFC..0xFF, EOSBS 0xF8/0xF9.
#[test]
fn roundtrip_sync_sequence_end_packet() {
    init();

    for end_byte in [0xFC, 0xF8] {
        let mut frame = make_frame(1, 20); // 30 bytes
        frame.extend_from_slice(&[0x00, 0x00, end_byte, 0x00]); // EOS / EOSBS
        assert_eq!(frame.len(), 34);

        let mut buf = gst::Buffer::from_slice(frame.clone());
        buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);

        let src = Source::Buffers(h263_caps(), vec![buf]);
        // Packet 1: picture header + GOB (30 - 2 = 28 bytes of payload).
        // Packet 2: sequence end alone (4 - 2 = 2 bytes of payload).
        let expected_pay = vec![vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::ZERO)
                .rtp_time(0)
                .flags(gst::BufferFlags::DISCONT)
                .size(42)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::ZERO)
                .rtp_time(0)
                .flags(gst::BufferFlags::MARKER)
                .marker_bit(true)
                .size(16)
                .build(),
        ]];
        let expected_depay = vec![vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::ZERO)
                .flags(gst::BufferFlags::DISCONT)
                .size(34)
                .build(),
        ]];

        let frame_clone = frame.clone();
        run_test_pipeline_and_validate_buffer(
            src,
            "rtph263ppay2 fragmentation-mode=sync",
            "rtph263pdepay2",
            expected_pay,
            expected_depay,
            move |buffer, _i, _j| assert_frame_roundtrip(&frame_clone, buffer),
        );
    }
}

/// Slice-structured (Annex K) streams split at SSC segments. avenc_h263p
/// never emits SSC codes, so this uses a synthetic slice segment.
#[test]
fn roundtrip_sync_ssc_segment_split() {
    init();

    let mut frame = make_frame(1, 20); // 30 bytes (picture header + GOB)
    // SSC (K.2.2: 16 zero bits + '1') + SEPB1 '1' (K.2.3) -> third byte
    // 0xC0.., then MBA + slice header + MB data
    frame.extend_from_slice(&[0x00, 0x00, 0xC0, 0x40]);
    frame.extend_from_slice(&[0x01; 8]);
    assert_eq!(frame.len(), 42);

    let mut buf = gst::Buffer::from_slice(frame.clone());
    buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);

    let src = Source::Buffers(h263_caps(), vec![buf]);
    // MTU 50 -> 36 usable: header+GOB (28) cannot fit the 12-byte slice.
    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::ZERO)
            .rtp_time(0)
            .flags(gst::BufferFlags::DISCONT)
            .size(42)
            .build(),
        ExpectedPacket::builder()
            .pts(gst::ClockTime::ZERO)
            .rtp_time(0)
            .flags(gst::BufferFlags::MARKER)
            .marker_bit(true)
            .size(24)
            .build(),
    ]];
    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::ZERO)
            .flags(gst::BufferFlags::DISCONT)
            .size(42)
            .build(),
    ]];

    let frame_clone = frame.clone();
    run_test_pipeline_and_validate_buffer(
        src,
        "rtph263ppay2 mtu=50 fragmentation-mode=sync",
        "rtph263pdepay2",
        expected_pay,
        expected_depay,
        move |buffer, _i, _j| assert_frame_roundtrip(&frame_clone, buffer),
    );
}

// Runtime tests against the real libav encoder/decoder, skipped
// explicitly when the required elements are missing (gst-libav,
// gst-plugins-bad videoparsers).
//
// H263-2000 has no encoder available here; its signalling is covered by
// pay_src_caps_negotiation and depay_src_caps_negotiation.

/// Black frames at the coarsest quantizer: every picture fits a single
/// packet at mtu=600 regardless of encoder version, so packet counts and
/// flags are stable even though sizes are not.
const AVENC_SRC: &str = "videotestsrc num-buffers=5 pattern=black \
     ! video/x-raw,format=I420,width=176,height=144,framerate=10/1 \
     ! avenc_h263p ps=500 gop-size=5 quantizer=30";
const DECODED_SIZE: usize = 176 * 144 * 3 / 2;

fn avenc_expected_pay(first_pts: gst::ClockTime) -> Vec<Vec<ExpectedPacket>> {
    (0..5)
        .map(|i| {
            let mut pkt = ExpectedPacket::builder()
                .pts(first_pts + gst::ClockTime::from_mseconds(i as u64 * 100))
                .rtp_time(i as u32 * 9000)
                // sizes depend on encoder output, not asserted
                .marker_bit(true);
            pkt = if i == 0 {
                pkt.flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            } else {
                pkt.flags(gst::BufferFlags::MARKER)
            };
            vec![pkt.build()]
        })
        .collect()
}

/// First picture intra (DISCONT), rest inter (DELTA_UNIT): exercises the
/// depayer's INTRA/DELTA_UNIT signalling on a real GOP.
fn avenc_expected_depay(first_pts: gst::ClockTime) -> Vec<Vec<ExpectedBuffer>> {
    (0..5)
        .map(|i| {
            let mut buf = ExpectedBuffer::builder()
                .pts(first_pts + gst::ClockTime::from_mseconds(i as u64 * 100));
            buf = if i == 0 {
                buf.flags(gst::BufferFlags::DISCONT)
            } else {
                buf.flags(gst::BufferFlags::DELTA_UNIT)
            };
            vec![buf.build()]
        })
        .collect()
}

fn avenc_expected_decoded(first_pts: gst::ClockTime) -> Vec<Vec<ExpectedBuffer>> {
    (0..5)
        .map(|i| {
            let mut buf = ExpectedBuffer::builder()
                .pts(first_pts + gst::ClockTime::from_mseconds(i as u64 * 100))
                .size(DECODED_SIZE);
            if i == 0 {
                buf = buf.flags(gst::BufferFlags::DISCONT);
            }
            vec![buf.build()]
        })
        .collect()
}

fn assert_picture_startcode(buffer: &gst::Buffer, _i: usize, _j: usize) -> anyhow::Result<()> {
    let map = buffer.map_readable()?;
    anyhow::ensure!(map.len() >= 4, "picture too short");
    anyhow::ensure!(map[..3] == [0x00, 0x00, 0x80], "no picture start code");
    Ok(())
}

/// Real encoder roundtrip, follow-on mode (RFC 4629 section 6.2).
#[test]
fn test_h263p_avenc_follow_on() {
    init();
    let Some(first_pts) = source_first_pts(AVENC_SRC, &["avenc_h263p"]) else {
        return;
    };

    run_test_pipeline_and_validate_buffer(
        Source::Bin(AVENC_SRC),
        "rtph263ppay2 mtu=600",
        "rtph263pdepay2",
        avenc_expected_pay(first_pts),
        avenc_expected_depay(first_pts),
        assert_picture_startcode,
    );
}

/// Real encoder roundtrip, segment mode (RFC 4629 section 6.1).
#[test]
fn test_h263p_avenc_sync() {
    init();
    let Some(first_pts) = source_first_pts(AVENC_SRC, &["avenc_h263p"]) else {
        return;
    };

    run_test_pipeline_and_validate_buffer(
        Source::Bin(AVENC_SRC),
        "rtph263ppay2 mtu=600 fragmentation-mode=sync",
        "rtph263pdepay2",
        avenc_expected_pay(first_pts),
        avenc_expected_depay(first_pts),
        assert_picture_startcode,
    );
}

/// No SDP parameters -> h263version=h263: the generic libav H.263 decoder
/// must decode the depayloaded stream.
#[test]
fn test_h263p_avenc_decode() {
    init();
    let Some(first_pts) = source_first_pts(AVENC_SRC, &["avenc_h263p", "avdec_h263"]) else {
        return;
    };

    run_test_pipeline(
        Source::Bin(AVENC_SRC),
        "rtph263ppay2 mtu=600",
        "rtph263pdepay2 ! avdec_h263",
        avenc_expected_pay(first_pts),
        avenc_expected_decoded(first_pts),
    );
}

/// RTP caps carrying an SDP annex parameter (as an SDP-driven session
/// delivers via rtpbin) make the depayer negotiate h263version=h263p, so
/// the strict H.263+ decoder accepts the stream.
#[test]
fn test_h263p_avenc_annex_params() {
    init();
    let Some(first_pts) = source_first_pts(AVENC_SRC, &["avenc_h263p", "avdec_h263p"]) else {
        return;
    };

    run_test_pipeline(
        Source::Bin(AVENC_SRC),
        "rtph263ppay2 mtu=600 ! capsfilter \
         caps=\"application/x-rtp,media=video,encoding-name=H263-1998,clock-rate=90000,f=(string)1\"",
        "rtph263pdepay2 ! avdec_h263p",
        avenc_expected_pay(first_pts),
        avenc_expected_decoded(first_pts),
    );
}

/// h263parse frames the input and its bitstream-derived annex caps after
/// the depayer let avdec_h263p negotiate without hand-written SDP
/// parameters: annexes flow encoder -> parser -> RTP caps -> depayer.
#[test]
fn test_h263p_avenc_parser_chain() {
    init();
    let parser_chain = format!("{AVENC_SRC} ! h263parse");
    let Some(first_pts) =
        source_first_pts(&parser_chain, &["avenc_h263p", "h263parse", "avdec_h263p"])
    else {
        return;
    };

    run_test_pipeline(
        Source::Bin(&parser_chain),
        "rtph263ppay2 mtu=600",
        "rtph263pdepay2 ! h263parse ! avdec_h263p",
        avenc_expected_pay(first_pts),
        avenc_expected_decoded(first_pts),
    );
}

/// A packet whose payload is shorter than its own declared payload header
/// (P=1, PLEN=8, 9-byte payload) must be dropped, not panic, and must not
/// corrupt the depayer state for subsequent packets.
#[test]
fn depay_drops_truncated_payload() {
    init();

    // P=1, V=0, PLEN=8: byte0 = 0x04 | (8 >> 5), byte1 = (8 & 0x1F) << 3
    let mut truncated = vec![0x04, 0x40];
    truncated.extend_from_slice(&[0x00; 7]); // total 9 < header_len 10

    // P=1 with payload exactly header_len bytes: no data byte, dropped.
    let exact = [0x04u8, 0x00];

    let frame = make_frame(1, 20);
    let mut valid = vec![0x04, 0x00]; // P=1
    valid.extend_from_slice(&frame[2..]);

    let packets = [
        (&truncated[..], false, 0),
        (&exact[..], false, 0),
        (&valid[..], true, 0),
    ];
    let caps = depay_rtp_caps("H263-1998", &[]);
    let (_out_caps, bufs) = run_depay_only(&caps, &packets).unwrap();

    assert_eq!(bufs.len(), 1, "only the valid packet must survive");
    assert_eq!(bufs[0], frame);
}

/// Packets of a stream that never sets the P bit are not RFC 4629
/// conformant (every frame must start with a P=1 segment packet or P=1
/// follow-on): they are dropped until a P=1 packet arrives.
#[test]
fn depay_drops_stream_without_p_bit() {
    init();

    let frame = make_frame(1, 20);
    let mut p0 = vec![0x00, 0x00]; // P=0
    p0.extend_from_slice(&frame);

    let mut p1 = vec![0x04, 0x00]; // P=1
    p1.extend_from_slice(&frame[2..]);

    let packets = [
        (&p0[..], false, 0),
        (&p0[..], true, 0),
        (&p1[..], true, 9000),
    ];

    let caps = depay_rtp_caps("H263-1998", &[]);
    let (_out_caps, bufs) = run_depay_only(&caps, &packets).unwrap();

    assert_eq!(bufs.len(), 1, "only the P=1 frame must survive");
    assert_eq!(bufs[0], frame);
}

/// When a marker packet is lost, the incomplete frame must be dropped when
/// packets of the next frame (different RTP timestamp) arrive, not spliced
/// with them.
#[test]
fn depay_drops_frame_on_timestamp_change() {
    init();

    let frame_a = make_frame(1, 20);
    let mut a = vec![0x04, 0x00]; // P=1, marker lost
    a.extend_from_slice(&frame_a[2..]);

    let frame_b = make_frame(2, 20);
    let mut b = vec![0x04, 0x00]; // P=1, new timestamp, marker
    b.extend_from_slice(&frame_b[2..]);

    let packets = [(&a[..], false, 0), (&b[..], true, 9000)];

    let caps = depay_rtp_caps("H263-1998", &[]);
    let (_out_caps, bufs) = run_depay_only(&caps, &packets).unwrap();

    assert_eq!(bufs.len(), 1, "only frame B must survive");
    assert_eq!(bufs[0], frame_b);
}

/// A follow-on packet (P=0) carrying a new RTP timestamp never starts a frame
/// of its own: it must not be appended to the pending frame of the previous
/// timestamp, which would splice two frames together.
#[test]
fn depay_drops_follow_on_with_new_timestamp() {
    init();

    let frame_a = make_frame(1, 20);
    let split = 16;

    let mut a = vec![0x04, 0x00]; // P=1, first part of frame A
    a.extend_from_slice(&frame_a[2..split]);

    let mut rest = vec![0x00, 0x00]; // P=0, rest of frame A, marker
    rest.extend_from_slice(&frame_a[split..]);

    // Continuation of a later frame, arriving before frame A completed.
    let mut stray = vec![0x00, 0x00]; // P=0
    stray.extend_from_slice(&[0xA5; 8]);

    let frame_b = make_frame(2, 20);
    let mut b = vec![0x04, 0x00]; // P=1, marker
    b.extend_from_slice(&frame_b[2..]);

    let packets = [
        (&a[..], false, 0),
        (&stray[..], false, 9000),
        (&rest[..], true, 0),
        (&b[..], true, 18000),
    ];

    let caps = depay_rtp_caps("H263-1998", &[]);
    let (_out_caps, bufs) = run_depay_only(&caps, &packets).unwrap();

    assert_eq!(bufs.len(), 1, "only frame B must survive");
    assert_eq!(bufs[0], frame_b);
}

/// Continuation packets with the same timestamp and no marker bit must not
/// grow the reassembly buffer without end: past the cap the frame is
/// dropped and the next one arrives discontinuous.
#[test]
fn depay_max_frame_size_caps_and_resyncs() {
    init();

    let caps = depay_rtp_caps("H263-1998", &[]);

    let frame = make_frame(1, 20);
    // P=1 starts a frame; the packet carries the start code without its
    // two leading zero bytes.
    let mut good = vec![0x04, 0x00];
    good.extend_from_slice(&frame[2..]);
    // P=0 continuations of 4000 filler bytes.
    let mut cont = vec![0x00, 0x00];
    cont.extend_from_slice(&[0xAB; 4000]);
    let mut end = vec![0x00, 0x00];
    end.extend_from_slice(&[0xAB; 4]);

    let mut packets: Vec<(&[u8], bool, u32, bool)> = Vec::new();
    // Frame 1 is complete and consumes the base class' initial DISCONT.
    packets.push((good.as_slice(), true, 0, false));
    // Frame 2 dribbles past the reassembly cap and then ends. The cap must
    // drop it, so only frames 1 and 3 come out.
    packets.push((good.as_slice(), false, 2000, false));
    // The two payload-header bytes per packet do not reach the accumulator.
    for _ in 0..MAX_FRAME_SIZE / (cont.len() - 2) + 2 {
        packets.push((cont.as_slice(), false, 2000, false));
    }
    packets.push((end.as_slice(), true, 2000, false));
    // Frame 3 completes again after the cap fired.
    packets.push((good.as_slice(), true, 4000, false));

    let buffers = crate::tests::run_depay_only("rtph263pdepay2", &caps, &packets).unwrap();
    assert_eq!(buffers.len(), 2);
    assert_eq!(buffers[0].0, frame);
    assert_eq!(buffers[1].0, frame);
    assert!(buffers[0].1.contains(gst::BufferFlags::DISCONT));
    // The drops in between must mark frame 3 discontinuous.
    assert!(buffers[1].1.contains(gst::BufferFlags::DISCONT));
}
