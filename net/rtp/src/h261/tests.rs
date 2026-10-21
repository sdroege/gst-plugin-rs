//
// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

use crate::h261::bitstream::test_util::qcif_frame;
use crate::tests::{
    ExpectedBuffer, ExpectedPacket, Source, run_test_pipeline_and_validate_buffer, source_first_pts,
};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtph261 test");
    });
}

fn h261_caps() -> gst::Caps {
    gst::Caps::builder("video/x-h261").build()
}

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

/// A whole picture fits in one packet: one RTP packet with the marker bit,
/// and a byte-exact picture out of the depayloader.
#[test]
fn roundtrip_single_packet() {
    init();

    let frame = qcif_frame(&[(1, 10, 4), (2, 12, 2), (3, 9, 3)]);
    let mut buf = gst::Buffer::from_slice(frame.clone());
    buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);

    let src = Source::Buffers(h261_caps(), vec![buf]);
    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::ZERO)
            .rtp_time(0)
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            .marker_bit(true)
            .size(12 + 4 + frame.len())
            .build(),
    ]];
    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::ZERO)
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::DELTA_UNIT)
            .size(frame.len())
            .build(),
    ]];

    let frame_clone = frame.clone();
    run_test_pipeline_and_validate_buffer(
        src,
        "rtph261pay2",
        "rtph261depay2",
        expected_pay,
        expected_depay,
        move |buffer, _i, _j| assert_frame_roundtrip(&frame_clone, buffer),
    );
}

/// With a small MTU the picture splits on macroblock boundaries: the first
/// packet ends inside GOB 2 (after its first macroblock), the second
/// carries the rest and is marked.
#[test]
fn roundtrip_splits_on_macroblocks() {
    init();

    // Picture header 31 bits; GOB 1: 26 + 4*21 = 110 bits (ends at 141);
    // GOB 2: 26 + 2*21 = 68 bits (ends at 209); GOB 3: 26 + 3*21 = 89 bits
    // (ends at 298 bits = 38 bytes).
    let frame = qcif_frame(&[(1, 10, 4), (2, 12, 2), (3, 9, 3)]);
    assert_eq!(frame.len(), 38);

    let mtu = 40usize; // 24 bytes of payload after RTP and payload headers
    let mut buf = gst::Buffer::from_slice(frame.clone());
    buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);

    // Packet 1: bits [0, 188) -> 24 bytes (GOB 1 whole, GOB 2 header and
    // first macroblock). Packet 2: bits [188, 298) -> 15 bytes.
    let src = Source::Buffers(h261_caps(), vec![buf]);
    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::ZERO)
            .rtp_time(0)
            .flags(gst::BufferFlags::DISCONT)
            .marker_bit(false)
            .size(12 + 4 + 24)
            .build(),
        ExpectedPacket::builder()
            .pts(gst::ClockTime::ZERO)
            .rtp_time(0)
            .flags(gst::BufferFlags::MARKER)
            .marker_bit(true)
            .size(12 + 4 + 15)
            .build(),
    ]];
    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::ZERO)
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::DELTA_UNIT)
            .size(frame.len())
            .build(),
    ]];

    let frame_clone = frame.clone();
    run_test_pipeline_and_validate_buffer(
        src,
        &format!("rtph261pay2 mtu={mtu}"),
        "rtph261depay2",
        expected_pay,
        expected_depay,
        move |buffer, _i, _j| assert_frame_roundtrip(&frame_clone, buffer),
    );
}

/// Consecutive byte-aligned pictures must each roundtrip byte-exactly.
#[test]
fn roundtrip_consecutive_pictures() {
    init();

    // 109 + 21 * 7 = 256 bits: both pictures end on a byte boundary.
    let frame1 = qcif_frame(&[(1, 10, 2), (2, 12, 2), (3, 9, 3)]);
    let frame2 = qcif_frame(&[(1, 8, 3), (2, 15, 1), (3, 3, 3)]);
    assert_eq!(frame1.len(), 32);
    assert_eq!(frame2.len(), 32);

    let mut buf1 = gst::Buffer::from_slice(frame1.clone());
    buf1.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    let mut buf2 = gst::Buffer::from_slice(frame2.clone());
    buf2.get_mut()
        .unwrap()
        .set_pts(gst::ClockTime::from_useconds(33333));

    let src = Source::Buffers(h261_caps(), vec![buf1, buf2]);
    let expected_depay = vec![
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::ZERO)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::DELTA_UNIT)
                .size(frame1.len())
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_useconds(33333))
                .flags(gst::BufferFlags::DELTA_UNIT)
                .size(frame2.len())
                .build(),
        ],
    ];
    let expected_pay = vec![
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::ZERO)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                .marker_bit(true)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_useconds(33333))
                .rtp_time(3000)
                .flags(gst::BufferFlags::MARKER)
                .marker_bit(true)
                .build(),
        ],
    ];

    let frames = std::sync::Arc::new(std::sync::Mutex::new(vec![frame1, frame2]));
    run_test_pipeline_and_validate_buffer(
        src,
        "rtph261pay2",
        "rtph261depay2",
        expected_pay,
        expected_depay,
        move |buffer, _i, _j| {
            let mut frames = frames.lock().unwrap();
            let frame = frames.remove(0);
            assert_frame_roundtrip(&frame, buffer)
        },
    );
}

/// A picture that starts bit-unaligned (the previous picture ended
/// mid-byte) is reconstructed with its start code at that bit offset: the
/// depayloader zero-fills the leading bits instead of re-aligning, exactly
/// like the picture start code scan in decoders expects.
#[test]
fn roundtrip_unaligned_picture_is_bit_shifted() {
    init();

    // First picture ends at bit 298 -> second picture starts at bit 2.
    let frame1 = qcif_frame(&[(1, 10, 4), (2, 12, 2), (3, 9, 3)]);
    let frame2 = qcif_frame(&[(1, 8, 2), (2, 15, 1), (3, 3, 5)]);
    assert_eq!(298 % 8, 2);

    let mut buf1 = gst::Buffer::from_slice(frame1.clone());
    buf1.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    let mut buf2 = gst::Buffer::from_slice(frame2.clone());
    buf2.get_mut()
        .unwrap()
        .set_pts(gst::ClockTime::from_useconds(33333));

    let src = Source::Buffers(h261_caps(), vec![buf1, buf2]);
    let expected_depay = vec![
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::ZERO)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::DELTA_UNIT)
                .size(frame1.len())
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_useconds(33333))
                .flags(gst::BufferFlags::DELTA_UNIT)
                .size(frame2.len())
                .build(),
        ],
    ];
    let expected_pay = vec![
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::ZERO)
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                .marker_bit(true)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_useconds(33333))
                .rtp_time(3000)
                .flags(gst::BufferFlags::MARKER)
                .marker_bit(true)
                .build(),
        ],
    ];

    // The second picture comes out shifted right by 2 bits: compare bit
    // ranges instead of bytes.
    let (frame1, frame2) = (frame1.clone(), frame2.clone());
    run_test_pipeline_and_validate_buffer(
        src,
        "rtph261pay2",
        "rtph261depay2",
        expected_pay,
        expected_depay,
        move |buffer, i, _j| {
            let frame = if i == 0 {
                return assert_frame_roundtrip(&frame1, buffer);
            } else {
                &frame2
            };
            // Second picture: shifted right by 2 bits (it starts at bit
            // 298 of the continuous stream).
            let shift = 2usize;
            let data = buffer.map_readable()?.as_slice().to_vec();
            let nbits = std::cmp::min(data.len() * 8 - shift, frame.len() * 8);
            for bit in 0..nbits {
                let got = (data[(bit + shift) / 8] >> (7 - (bit + shift) % 8)) & 1;
                let want = (frame[bit / 8] >> (7 - bit % 8)) & 1;
                anyhow::ensure!(
                    got == want,
                    "bit {bit} differs after realignment: got {:02x?} want {:02x?}",
                    &data[..6],
                    &frame[..6]
                );
            }
            Ok(())
        },
    );
}

fn depay_rtp_caps() -> gst::Caps {
    gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "H261")
        .field("clock-rate", 90_000i32)
        .field("payload", 96i32)
        .build()
}

/// Build an H.261 payload header (RFC 4587 section 4.1).
fn h261_header(sbit: u8, ebit: u8, intra_only: bool) -> [u8; 4] {
    let mut header = [0u8; 4];
    header[0] = (sbit << 5) | (ebit << 2) | u8::from(intra_only) << 1 | 0x01;
    header
}

/// Feed RTP packets through the depayloader alone. Returns the depayloaded
/// buffers with their flags.
fn run_depay_only(
    packets: &[(&[u8], bool, u32, bool)],
) -> Result<Vec<(Vec<u8>, gst::BufferFlags)>, anyhow::Error> {
    init();

    let rtp_caps = depay_rtp_caps();
    let buffers: Vec<gst::Buffer> = packets
        .iter()
        .enumerate()
        .map(|(i, (payload, marker, ts, discont))| {
            let bytes = rtp_types::RtpPacketBuilder::new()
                .payload_type(96)
                .sequence_number(i as u16)
                .timestamp(*ts)
                .ssrc(0x1234)
                .marker_bit(*marker)
                .payload(*payload)
                .write_vec()
                .unwrap();
            let mut buffer = gst::Buffer::from_slice(bytes);
            if *discont {
                buffer
                    .get_mut()
                    .unwrap()
                    .set_flags(gst::BufferFlags::DISCONT);
            }
            buffer
        })
        .collect();
    let mut buffers = buffers.into_iter();

    let appsrc = gst_app::AppSrc::builder()
        .format(gst::Format::Time)
        .caps(&rtp_caps)
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
    let depay = gst::ElementFactory::make("rtph261depay2").build().unwrap();
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap();
    let appsrc = appsrc.upcast::<gst::Element>();
    pipeline.add_many([&appsrc, &depay, &sink]).unwrap();
    appsrc.link(&depay).unwrap();
    depay.link(&sink).unwrap();

    let out_bufs = std::sync::Arc::new(std::sync::Mutex::new(
        Vec::<(Vec<u8>, gst::BufferFlags)>::new(),
    ));
    depay.static_pad("src").unwrap().add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        {
            let out_bufs = out_bufs.clone();
            move |_pad, info| {
                let bufs: Vec<gst::Buffer> = match info.buffer_list() {
                    Some(list) => (0..list.len())
                        .filter_map(|i| list.get(i).map(|b| b.to_owned()))
                        .collect(),
                    None => info.buffer().map(|b| vec![b.clone()]).unwrap_or_default(),
                };
                let mut out = out_bufs.lock().unwrap();
                for buf in bufs {
                    let flags = buf.flags();
                    out.push((buf.map_readable().unwrap().as_slice().to_vec(), flags));
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

    Ok(std::mem::take(&mut *out_bufs.lock().unwrap()))
}

/// A packet whose payload is just the 4-byte header is dropped.
#[test]
fn depay_drops_header_only_packet() {
    let header = h261_header(0, 0, false);
    let buffers = run_depay_only(&[(&header, true, 0, false)]).unwrap();
    assert!(buffers.is_empty());
}

/// Packets before the first picture start code are dropped; the picture is
/// emitted whole once its start code arrives.
#[test]
fn depay_waits_for_picture_start() {
    let frame = qcif_frame(&[(1, 10, 2)]);

    // A tail fragment of a picture whose start was lost, marked.
    let tail = &frame[frame.len() / 2..];
    let mut packet = h261_header(0, 0, false).to_vec();
    packet.extend_from_slice(tail);

    let mut full = h261_header(0, 0, false).to_vec();
    full.extend_from_slice(&frame);

    let buffers = run_depay_only(&[(&packet, true, 0, false), (&full, true, 1000, false)]).unwrap();
    assert_eq!(buffers.len(), 1);
    assert_eq!(buffers[0].0, frame);
}

/// Fragments split at non-byte positions are spliced back bit-exactly:
/// the trailing bits of one packet merge with the leading bits of the next.
#[test]
fn depay_splices_bit_fragments() {
    let frame = qcif_frame(&[(1, 10, 4), (2, 12, 2), (3, 9, 3)]);

    // Split the picture at bit 188 (byte 23, bit offset 4).
    let split = 188;
    let first_bytes = split / 8; // 23 whole bytes
    let second_bytes = frame.len() - first_bytes; // byte 23 shared

    // Packet 1: bits [0, 188): sbit 0, ebit 4 (4 unused trailing bits).
    let mut p1 = h261_header(0, 4, false).to_vec();
    p1.extend_from_slice(&frame[..first_bytes + 1]);
    // Packet 2: bits [188, 298): sbit 4, ebit 6.
    let mut p2 = h261_header(4, 6, false).to_vec();
    p2.extend_from_slice(&frame[first_bytes..first_bytes + second_bytes]);

    let buffers = run_depay_only(&[(&p1, false, 0, false), (&p2, true, 0, false)]).unwrap();
    assert_eq!(buffers.len(), 1);
    assert_eq!(buffers[0].0, frame);
}

/// A packet carrying a single data byte with EBIT != 0 keeps that byte as
/// the carry-over to the next packet instead of pushing it whole.
#[test]
fn depay_single_data_byte_packet() {
    // Packet 1: picture start code, byte-aligned end.
    let mut p1 = h261_header(0, 0, false).to_vec();
    p1.extend_from_slice(&[0x00, 0x01, 0x00, 0x16]);
    // Packet 2: one data byte, 4 unused trailing bits.
    let p2 = [h261_header(0, 4, false).as_slice(), &[0xAB]].concat();
    // Packet 3: continues at bit 4, ends byte-aligned.
    let p3 = [h261_header(4, 0, false).as_slice(), &[0x0C, 0x99]].concat();

    let buffers = run_depay_only(&[
        (&p1, false, 0, false),
        (&p2, false, 0, false),
        (&p3, true, 0, false),
    ])
    .unwrap();
    assert_eq!(buffers.len(), 1);
    // The single byte contributes its top 4 bits (0xA) merged with the
    // next packet's first nibble (0xC).
    assert_eq!(buffers[0].0, vec![0x00, 0x01, 0x00, 0x16, 0xAC, 0x99]);
}

/// The I flag describes the whole stream: I=0 marks the output inter
/// coded, I=1 intra coded.
#[test]
fn depay_sets_delta_flag_from_i_flag() {
    let frame = qcif_frame(&[(1, 10, 2)]);

    let mut inter = h261_header(0, 0, false).to_vec();
    inter.extend_from_slice(&frame);
    let mut intra = h261_header(0, 0, true).to_vec();
    intra.extend_from_slice(&frame);

    let buffers = run_depay_only(&[(&inter, true, 0, false), (&intra, true, 1000, false)]).unwrap();
    assert_eq!(buffers.len(), 2);
    assert!(buffers[0].1.contains(gst::BufferFlags::DELTA_UNIT));
    assert!(!buffers[1].1.contains(gst::BufferFlags::DELTA_UNIT));
}

/// Push one frame through the payloader alone. Returns the negotiated src
/// caps and the bytes of each output RTP packet, or an error if the
/// pipeline failed.
fn run_pay_only(
    frame: Vec<u8>,
    mtu: u32,
    source_info: bool,
    downstream_caps: Option<&gst::Caps>,
) -> Result<(Option<gst::Caps>, Vec<Vec<u8>>), anyhow::Error> {
    init();

    let mut frame_buf = gst::Buffer::from_slice(frame);
    frame_buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    let mut buffers = vec![frame_buf].into_iter();

    let appsrc = gst_app::AppSrc::builder()
        .format(gst::Format::Time)
        .caps(&h261_caps())
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
    let pay = gst::ElementFactory::make("rtph261pay2")
        .property("mtu", mtu)
        .property("source-info", source_info)
        .build()
        .unwrap();
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
        pay.link(&filter)
            .map_err(|e| anyhow::anyhow!("link failed: {e:?}"))?;
        filter.link(&sink).unwrap();
    } else {
        pay.link(&sink).unwrap();
    }

    let out = std::sync::Arc::new(std::sync::Mutex::new((Vec::new(), None::<gst::Caps>)));
    pay.static_pad("src").unwrap().add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        {
            let out = out.clone();
            move |pad, info| {
                let mut out = out.lock().unwrap();
                // basepay queues one buffer list per input buffer.
                let buffers: Vec<gst::Buffer> = match info.buffer() {
                    Some(buf) => vec![buf.to_owned()],
                    None => info
                        .buffer_list()
                        .map(|list| {
                            (0..list.len())
                                .filter_map(|idx| list.get_owned(idx))
                                .collect()
                        })
                        .unwrap_or_default(),
                };
                for buf in buffers {
                    out.0.push(buf.map_readable().unwrap().as_slice().to_vec());
                }
                if out.1.is_none() {
                    out.1 = pad.current_caps();
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
        .ok_or_else(|| anyhow::anyhow!("no EOS/error from pay pipeline"))?;
    let _ = pipeline.set_state(gst::State::Null);

    if msg.type_() == gst::MessageType::Error {
        return Err(anyhow::anyhow!("pay pipeline failed: {msg:?}"));
    }

    let (packets, caps) = std::mem::take(&mut *out.lock().unwrap());
    Ok((caps, packets))
}

/// The negotiated src caps must describe the H.261 stream
/// (RFC 4587 section 8).
#[test]
fn pay_src_caps_negotiation() {
    let frame = qcif_frame(&[(1, 10, 2), (2, 12, 2), (3, 9, 3)]);
    let (caps, packets) = run_pay_only(frame, 1400, false, None).unwrap();
    assert_eq!(packets.len(), 1);
    let caps = caps.unwrap();
    let s = caps.structure(0).unwrap();
    assert_eq!(s.get::<&str>("media").unwrap(), "video");
    assert_eq!(s.get::<&str>("encoding-name").unwrap(), "H261");
    assert_eq!(s.get::<i32>("clock-rate").unwrap(), 90_000);
}

/// A downstream pinned to a contradicting encoding-name rejects the caps:
/// the pipeline cannot be linked.
#[test]
fn pay_conflicting_downstream_fails() {
    init();

    let frame = qcif_frame(&[(1, 10, 2), (2, 12, 2), (3, 9, 3)]);
    assert!(
        run_pay_only(
            frame,
            1400,
            false,
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

/// Input that contains no picture start code is not H.261 at all: the
/// pipeline fails instead of silently dropping everything.
#[test]
fn pay_rejects_buffer_without_startcode() {
    assert!(
        run_pay_only(
            vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
            1400,
            false,
            None,
        )
        .is_err()
    );
}

/// A picture whose macroblock layer is garbage is dropped without
/// producing any packet.
#[test]
fn pay_drops_picture_with_invalid_macroblock() {
    use crate::h261::bitstream::test_util::{BitWriter, picture_header};

    let mut w = BitWriter::new();
    picture_header(&mut w);
    w.write(16, 0x0001); // GBSC
    w.write(4, 1); // GN
    w.write(5, 8); // GQUANT
    w.write(1, 0); // no GEI
    w.write(16, 0x000F); // matches no MBA codeword
    let (_caps, packets) = run_pay_only(w.finish(), 1400, false, None).unwrap();
    assert_eq!(packets.len(), 0);
}

/// A group start code that no GOB boundary accounts for must stop the
/// payloader instead of making it loop forever: the picture is cut at the
/// unexpected start code and the rest dropped.
#[test]
fn pay_stops_at_unexpected_group_start_code() {
    // The picture header announces three GOBs, four are present. The fourth
    // start code lies inside the extent of the last announced GOB.
    let frame = qcif_frame(&[(1, 10, 1), (2, 11, 1), (3, 12, 1), (4, 13, 1)]);

    let (_caps, packets) = run_pay_only(frame, 1400, false, None).unwrap();

    // One packet covering bits 0..172: picture header, GOBs 1 to 3 and the
    // fourth start code, which is left out.
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].len(), 12 + 4 + 22);
    assert_eq!(packets[0][1] & 0x80, 0, "truncated picture is not marked");
}

/// A macroblock larger than the MTU is sent whole, so a packet can end past
/// the start of the next GOB. The following round must account for the GOBs
/// it skipped over instead of computing a negative bit range.
#[test]
fn pay_oversized_macroblock_crossing_gob_boundaries() {
    use crate::h261::bitstream::test_util::{BitWriter, picture_header};

    let mut w = BitWriter::new();
    picture_header(&mut w); // 31 bits, QCIF: three GOBs
    // Filler between the picture header and GOB 1, free of start codes.
    w.write(32, 0xFFFF_FFFF);
    w.write(27, 0x07FF_FFFF);

    // GOB 1 header at bit 90.
    w.write(16, 0x0001);
    w.write(4, 1); // GN
    w.write(5, 10); // GQUANT
    w.write(1, 0); // no GEI

    // One macroblock of 209 bits: larger than the 12 bytes of payload an
    // MTU of 28 leaves. Its coefficient data holds two group start code
    // patterns, which the GOB scan takes for boundaries but the macroblock
    // parser consumes as coefficients.
    w.write(1, 1); // MBA: difference 1
    w.write(1, 1); // MTYPE: INTER | CBP | TCOEFF
    w.write(3, 0b111); // CBP: blocks 0..3
    w.write(2, 0b10); // first coefficient
    for _ in 0..35 {
        w.write(4, 0b0110); // run 0, level 1
    }
    for _ in 0..2 {
        w.write(6, 0b000001); // escape
        w.write(14, 0); // run 0, level 0
        w.write(4, 0b0110); // run 0, level 1
    }
    w.write(2, 0b10); // EOB
    for _ in 0..3 {
        w.write(2, 0b10); // first coefficient
        w.write(2, 0b10); // EOB
    }

    // GOBs 2 and 3, one macroblock each.
    for gn in [2, 3] {
        w.write(16, 0x0001);
        w.write(4, gn);
        w.write(5, 10);
        w.write(1, 0);
        w.write(1, 1);
        w.write(1, 1);
        w.write(3, 0b111);
        for _ in 0..4 {
            w.write(2, 0b10);
            w.write(2, 0b10);
        }
    }

    let (_caps, packets) = run_pay_only(w.finish(), 28, false, None).unwrap();

    // Two packets: the picture header, then the oversized macroblock. The
    // real GOB 2 header is not one the scan counted, so the rest is dropped
    // and the picture stays unmarked.
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].len(), 12 + 4 + 12);
    assert_eq!(packets[1].len(), 12 + 4 + 30);
    assert_eq!(packets[1][1] & 0x80, 0);
}

/// Packets must not exceed the MTU when a valid split boundary exists:
/// with mtu=39 the first packet ends at the GOB 1 boundary instead of
/// overshooting into GOB 2 with its first macroblock.
#[test]
fn roundtrip_respects_mtu() {
    init();

    let frame = qcif_frame(&[(1, 10, 4), (2, 12, 2), (3, 9, 3)]);
    let mut buf = gst::Buffer::from_slice(frame.clone());
    buf.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    let src = Source::Buffers(h261_caps(), vec![buf]);

    // 39-byte MTU: 23 bytes of payload. GOB 1 (bits [0, 141)) fits; GOB 2's
    // first macroblock ends at bit 188, past the limit, so packet 1 covers
    // bits [0, 141) = 18 bytes and packet 2 the rest (21 bytes).
    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::ZERO)
            .rtp_time(0)
            .flags(gst::BufferFlags::DISCONT)
            .marker_bit(false)
            .size(12 + 4 + 18)
            .build(),
        ExpectedPacket::builder()
            .pts(gst::ClockTime::ZERO)
            .rtp_time(0)
            .flags(gst::BufferFlags::MARKER)
            .marker_bit(true)
            .size(12 + 4 + 21)
            .build(),
    ]];
    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::ZERO)
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::DELTA_UNIT)
            .size(frame.len())
            .build(),
    ]];

    let frame_clone = frame.clone();
    run_test_pipeline_and_validate_buffer(
        src,
        "rtph261pay2 mtu=39",
        "rtph261depay2",
        expected_pay,
        expected_depay,
        move |buffer, _i, _j| assert_frame_roundtrip(&frame_clone, buffer),
    );
}

/// The second packet of a fragmented picture must carry the predictor
/// state of the last macroblock of the first packet (RFC 4587 section
/// 4.1); the first packet starts at the picture header and zeroes them.
#[test]
fn pay_fragmented_packet_headers() {
    // Same picture and split as roundtrip_splits_on_macroblocks: packet 1
    // covers bits [0, 188) (whole GOB 1, GOB 2 header and its first
    // macroblock), packet 2 the rest.
    let frame = qcif_frame(&[(1, 10, 4), (2, 12, 2), (3, 9, 3)]);
    let (_caps, packets) = run_pay_only(frame, 40, false, None).unwrap();
    assert_eq!(packets.len(), 2);
    // RTP header is 12 bytes. Packet 1: sbit=0, ebit=4, V=1, zero
    // predictors (picture header start).
    assert_eq!(&packets[0][12..16], &[0x11, 0x00, 0x00, 0x00]);
    // Packet 2: sbit=4, ebit=6; predicts from the last macroblock of
    // packet 1 (GOB 2, macroblock 1, GQUANT 12, no MVD): GOBN=2, MBAP=0,
    // QUANT=12, HMVD=VMVD=0.
    assert_eq!(&packets[1][12..16], &[0x99, 0x20, 0x30, 0x00]);
}

/// A payload too short to carry a checkable picture start code is dropped.
#[test]
fn depay_drops_short_payload() {
    let mut packet = h261_header(0, 0, false).to_vec();
    packet.extend_from_slice(&[0x00, 0x01, 0x00]);
    let buffers = run_depay_only(&[(&packet, true, 0, false)]).unwrap();
    assert!(buffers.is_empty());
}

/// A timestamp change before picture completion drops the incomplete
/// picture; accumulation restarts with the new picture's packets.
#[test]
fn depay_drops_incomplete_picture_on_timestamp_change() {
    let mut p1 = h261_header(0, 0, false).to_vec();
    p1.extend_from_slice(&[0x00, 0x01, 0x00, 0x16]);
    let mut p2 = h261_header(0, 0, false).to_vec();
    p2.extend_from_slice(&[0xAB, 0xCD]);
    let buffers = run_depay_only(&[(&p1, false, 0, false), (&p2, true, 1000, false)]).unwrap();
    assert_eq!(buffers.len(), 1);
    assert_eq!(buffers[0].0, vec![0xAB, 0xCD]);
}

/// A discontinuity drops the partially assembled picture.
#[test]
fn depay_discont_resets_picture_assembly() {
    let mut p1 = h261_header(0, 0, false).to_vec();
    p1.extend_from_slice(&[0x00, 0x01, 0x00, 0x16]);
    let mut p2 = h261_header(0, 0, false).to_vec();
    p2.extend_from_slice(&[0xAB]);
    let mut p3 = h261_header(0, 0, false).to_vec();
    p3.extend_from_slice(&[0x00, 0x01, 0x00, 0x16, 0x99]);
    let buffers = run_depay_only(&[
        (&p1, false, 0, false),
        (&p2, true, 0, true),
        (&p3, true, 1000, false),
    ])
    .unwrap();
    assert_eq!(buffers.len(), 1);
    assert_eq!(buffers[0].0, vec![0x00, 0x01, 0x00, 0x16, 0x99]);
}

const RUNTIME_SOURCE: &str = "videotestsrc num-buffers=5 ! \
 video/x-raw,format=I420,width=176,height=144,framerate=10/1 ! avenc_h261";

/// One packet per picture (the MTU fits whole QCIF pictures), marker on
/// every picture. The Rust payloader also flags the first packet of the
/// stream as a discontinuity, the C payloader does not.
fn runtime_expected_pay(
    pt: u8,
    first_discont: bool,
    first_pts: gst::ClockTime,
) -> Vec<Vec<ExpectedPacket>> {
    (0..5)
        .map(|i| {
            let mut pkt = ExpectedPacket::builder()
                .pt(pt)
                .pts(first_pts + gst::ClockTime::from_mseconds(i as u64 * 100))
                .rtp_time(i as u32 * 9000)
                // sizes depend on encoder output, not asserted
                .marker_bit(true);
            pkt = if i == 0 && first_discont {
                pkt.flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            } else {
                pkt.flags(gst::BufferFlags::MARKER)
            };
            vec![pkt.build()]
        })
        .collect()
}

/// The payloader always signals I=0, so every depayloaded picture is a
/// delta unit; the depayloader flags the first output buffer as a
/// discontinuity.
fn runtime_expected_depay(
    ignored_flags: gst::BufferFlags,
    first_pts: gst::ClockTime,
) -> Vec<Vec<ExpectedBuffer>> {
    (0..5)
        .map(|i| {
            let mut buf = ExpectedBuffer::builder()
                .pts(first_pts + gst::ClockTime::from_mseconds(i as u64 * 100))
                .ignore_flags(ignored_flags);
            buf = if i == 0 {
                buf.flags(gst::BufferFlags::DISCONT | gst::BufferFlags::DELTA_UNIT)
            } else {
                buf.flags(gst::BufferFlags::DELTA_UNIT)
            };
            vec![buf.build()]
        })
        .collect()
}

fn assert_picture_startcode(buffer: &gst::Buffer, _i: usize, _j: usize) -> anyhow::Result<()> {
    let map = buffer.map_readable()?;
    anyhow::ensure!(map.len() > 4, "picture too small");
    // PSC: 15 zero bits, a one bit, four zero bits, at any bit offset
    // (a picture may start mid-byte).
    let v = u32::from_be_bytes([map[0], map[1], map[2], map[3]]);
    anyhow::ensure!(
        (0..8).any(|off| (v << off) >> 12 == 0x10),
        "no picture start code"
    );
    Ok(())
}

/// Real encoder through the Rust payloader and depayloader.
#[test]
fn test_h261_avenc_rust_roundtrip() {
    init();
    let Some(first_pts) = source_first_pts(RUNTIME_SOURCE, &["avenc_h261"]) else {
        eprintln!("avenc_h261 not available, skipping");
        return;
    };

    run_test_pipeline_and_validate_buffer(
        Source::Bin(RUNTIME_SOURCE),
        "rtph261pay2 mtu=65535",
        "rtph261depay2",
        runtime_expected_pay(96, true, first_pts),
        runtime_expected_depay(gst::BufferFlags::empty(), first_pts),
        assert_picture_startcode,
    );
}

/// Interop: `rtph261pay` output into `rtph261depay2`.
#[test]
fn test_h261_interop_rtph261pay_to_depay2() {
    init();
    let Some(first_pts) = source_first_pts(RUNTIME_SOURCE, &["avenc_h261", "rtph261pay"]) else {
        eprintln!("avenc_h261/rtph261pay not available, skipping");
        return;
    };

    run_test_pipeline_and_validate_buffer(
        Source::Bin(RUNTIME_SOURCE),
        "rtph261pay mtu=65535",
        "rtph261depay2",
        // The C payloader uses the static H.261 payload type.
        runtime_expected_pay(31, false, first_pts),
        runtime_expected_depay(gst::BufferFlags::empty(), first_pts),
        assert_picture_startcode,
    );
}

/// Interop: `rtph261pay2` output into `rtph261depay`.
#[test]
fn test_h261_interop_pay2_to_rtph261depay() {
    init();
    let Some(first_pts) = source_first_pts(RUNTIME_SOURCE, &["avenc_h261", "rtph261depay"]) else {
        eprintln!("avenc_h261/rtph261depay not available, skipping");
        return;
    };

    run_test_pipeline_and_validate_buffer(
        Source::Bin(RUNTIME_SOURCE),
        "rtph261pay2 mtu=65535 pt=31",
        "rtph261depay",
        // The C depayloader's static H.261 payload type. It is set on the
        // payloader rather than taken from the negotiated caps, which no
        // longer advertise it.
        runtime_expected_pay(31, true, first_pts),
        // The C depayloader copies the RTP packet's MARKER flag into some
        // of its output buffers.
        runtime_expected_depay(gst::BufferFlags::MARKER, first_pts),
        assert_picture_startcode,
    );
}

const DECODED_SIZE: usize = 176 * 144 * 3 / 2;

/// Five decoded QCIF frames come out of the depayloader + decoder chain.
#[test]
fn test_h261_avdec_decode() {
    init();
    let Some(first_pts) = source_first_pts(RUNTIME_SOURCE, &["avenc_h261", "avdec_h261"]) else {
        eprintln!("avenc_h261/avdec_h261 not available, skipping");
        return;
    };

    let expected_decoded: Vec<Vec<ExpectedBuffer>> = (0..5)
        .map(|i| {
            let mut buf = ExpectedBuffer::builder()
                .pts(first_pts + gst::ClockTime::from_mseconds(i as u64 * 100))
                .size(DECODED_SIZE);
            if i == 0 {
                buf = buf.flags(gst::BufferFlags::DISCONT);
            }
            vec![buf.build()]
        })
        .collect();

    run_test_pipeline_and_validate_buffer(
        Source::Bin(RUNTIME_SOURCE),
        "rtph261pay2 mtu=65535",
        "rtph261depay2 ! avdec_h261",
        runtime_expected_pay(96, true, first_pts),
        expected_decoded,
        |_, _, _| Ok(()),
    );
}
