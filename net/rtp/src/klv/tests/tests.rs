// GStreamer RTP KLV Payloader / Depayloader - unit tests
//
// Copyright (C) 2024 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::klv::klv_utils::*;
use crate::tests::{run_test_pipeline_full, ExpectedBuffer, ExpectedPacket, Source};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtp klv test");
    });
}

const KLV_DATA: &[u8] = include_bytes!("day-flight.klv").as_slice();

pub(crate) fn parse_klv_packets(data: &[u8]) -> anyhow::Result<Vec<&[u8]>> {
    let mut packets = vec![];

    let mut data = &data[0..];

    while !data.is_empty() {
        let size = peek_klv(data)?;
        // eprintln!("KLV unit {} of size {size}", packets.len());
        packets.push(&data[0..][..size]);
        data = &data[size..];
    }

    Ok(packets)
}

fn make_buffer(
    data: &'static [u8],
    pts: gst::ClockTime,
    duration: Option<gst::ClockTime>,
    flags: gst::BufferFlags,
) -> gst::Buffer {
    let mut buf = gst::Buffer::from_slice(data);

    let buf_ref = buf.get_mut().unwrap();
    buf_ref.set_pts(pts);
    if let Some(duration) = duration {
        buf_ref.set_duration(duration);
    }
    buf_ref.set_flags(flags);

    buf
}

// test_klv_pay_depay
//
// Check basic payloading/depayloading
//
#[test]
fn test_klv_pay_depay() {
    let klv_packets = parse_klv_packets(KLV_DATA).unwrap();

    init();

    let input_caps = gst::Caps::builder("meta/x-klv")
        .field("parsed", true)
        .build();

    let mut input_buffers = vec![];

    for (i, klv) in klv_packets.iter().enumerate() {
        input_buffers.push(make_buffer(
            klv,
            gst::ClockTime::from_seconds(i as u64),
            gst::ClockTime::NONE,
            if i == 0 {
                gst::BufferFlags::DISCONT
            } else {
                gst::BufferFlags::empty()
            },
        ));
    }

    let mut expected_pay = vec![];

    for (i, _klv) in klv_packets.iter().enumerate() {
        let expected_flags = match i {
            0 => gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER,
            _ => gst::BufferFlags::MARKER,
        };
        expected_pay.push(vec![ExpectedPacket::builder()
            .pts(gst::ClockTime::from_seconds(i as u64))
            .flags(expected_flags)
            .pt(96)
            .rtp_time(i as u32 * 90_000)
            .marker_bit(true)
            .build()]);
    }

    let mut expected_depay = vec![];

    for (i, _klv) in klv_packets.into_iter().enumerate() {
        let expected_flags = match i {
            0 => gst::BufferFlags::DISCONT,
            _ => gst::BufferFlags::empty(),
        };
        let expected_size = match i {
            0..=4 => 163,
            5 => 162,
            _ => unreachable!(),
        };
        expected_depay.push(vec![ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_seconds(i as u64))
            .size(expected_size)
            .flags(expected_flags)
            .build()]);
    }

    let expected_output_caps = input_caps.clone();

    run_test_pipeline_full(
        Source::Buffers(input_caps, input_buffers),
        "rtpklvpay2",
        "rtpklvdepay2",
        expected_pay,
        expected_depay,
        Some(expected_output_caps),
    );
}

// test_klv_pay_depay_fragmented
//
// Check basic payloading/depayloading with fragmentated payloads
//
#[test]
fn test_klv_pay_depay_fragmented() {
    let klv_packets = parse_klv_packets(KLV_DATA).unwrap();

    init();

    let input_caps = gst::Caps::builder("meta/x-klv")
        .field("parsed", true)
        .build();

    let mut input_buffers = vec![];

    for (i, klv) in klv_packets.iter().enumerate() {
        input_buffers.push(make_buffer(
            klv,
            gst::ClockTime::from_seconds(i as u64),
            gst::ClockTime::NONE,
            if i == 0 {
                gst::BufferFlags::DISCONT
            } else {
                gst::BufferFlags::empty()
            },
        ));
    }

    let mut expected_pay = vec![];

    for (i, _klv) in klv_packets.iter().enumerate() {
        expected_pay.push(vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_seconds(i as u64))
                .flags(if i == 0 {
                    gst::BufferFlags::DISCONT
                } else {
                    gst::BufferFlags::empty()
                })
                .pt(96)
                .rtp_time(i as u32 * 90_000)
                .marker_bit(false)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_seconds(i as u64))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(i as u32 * 90_000)
                .marker_bit(false)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_seconds(i as u64))
                .flags(gst::BufferFlags::empty())
                .pt(96)
                .rtp_time(i as u32 * 90_000)
                .marker_bit(false)
                .build(),
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_seconds(i as u64))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(i as u32 * 90_000)
                .marker_bit(true)
                .build(),
        ]);
    }

    let mut expected_depay = vec![];

    for (i, _klv) in klv_packets.into_iter().enumerate() {
        let expected_flags = match i {
            0 => gst::BufferFlags::DISCONT,
            _ => gst::BufferFlags::empty(),
        };
        let expected_size = match i {
            0..=4 => 163,
            5 => 162,
            _ => unreachable!(),
        };
        expected_depay.push(vec![ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_seconds(i as u64))
            .size(expected_size)
            .flags(expected_flags)
            .build()]);
    }

    let expected_output_caps = input_caps.clone();

    run_test_pipeline_full(
        Source::Buffers(input_caps, input_buffers),
        "rtpklvpay2 mtu=60",
        "rtpklvdepay2",
        expected_pay,
        expected_depay,
        Some(expected_output_caps),
    );
}
