// Copyright (C) 2026 Fluendo S.A.
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsaudioparsers::plugin_register_static().expect("ac4parse test");
    });
}

#[test]
fn test_parse_ac4_2ch() {
    init();

    let mut h = gst_check::Harness::new("ac4parse");
    h.play();

    let caps_in = gst::Caps::builder("audio/x-ac4")
        .field("stream-format", "syncframe")
        .build();
    h.set_src_caps(caps_in);

    let data = include_bytes!("sample_2ch.ac4");

    // Create a buffer with the data and push it
    let buffer = gst::Buffer::from_slice(data);
    h.push(buffer).unwrap();

    // Extract first buffer
    let out_buffer = h.pull().unwrap();

    // Verify first frame buffer:
    // - Syncword - 2 bytes (0xAC41)
    // - Frame size indicator: 2 bytes (0xFFFF)
    // - Extended frame size: 3 bytes -> 0x0001A7 indicates 423 bytes
    // - TOC + payload: 423 bytes
    // - CRC: 2 bytes
    // Total = 7 (header) + 423 (payload) + 2 (CRC) = 432 bytes
    let expected_frame_size = 432;
    assert_eq!(
        out_buffer.size(),
        expected_frame_size,
        "Buffer size must be {} bytes",
        expected_frame_size
    );

    let caps_out = h.sinkpad().unwrap().current_caps().unwrap();

    // Check caps are the expected ones
    let structure = caps_out.structure(0).unwrap();

    assert_eq!(structure.name(), "audio/x-ac4");

    let alignment = structure.get::<String>("alignment").unwrap();
    assert_eq!(alignment, "frame");

    let rate = structure.get::<i32>("rate").unwrap();
    assert_eq!(rate, 48000);

    let framerate = structure.get::<gst::Fraction>("framerate").unwrap();

    assert!(
        framerate.numer() == 25 && framerate.denom() == 1,
        "Framerate must be 25/1, got {}/{}",
        framerate.numer(),
        framerate.denom()
    );

    let bitstream_version = structure.get::<i32>("bitstream-version").unwrap();
    assert!(
        bitstream_version == 2,
        "bitstream-version must be 2. Obtained one: {}",
        bitstream_version
    );
}

#[test]
fn test_parse_ac4_reserved_frame_rate() {
    init();

    let mut h = gst_check::Harness::new("ac4parse");
    h.play();
    h.set_src_caps(gst::Caps::builder("audio/x-ac4").build());

    // Invalid syncframes with reserved/invalid frame_rate index for the fs_index = 0
    let bad_frame = [0xAC, 0x40, 0x00, 0x08, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut data = Vec::new();
    data.extend_from_slice(&bad_frame);
    data.extend_from_slice(&bad_frame);
    data.extend_from_slice(include_bytes!("sample_2ch.ac4"));
    h.push(gst::Buffer::from_slice(data)).unwrap();

    // First output is the first frame of the sample
    let out_buffer = h.pull().unwrap();
    assert_eq!(out_buffer.size(), 432);
}

// First raw_ac4_frame() of the sample, without the sync header and CRC.
// The first sync frame is laid out as:
//
//   offset 0 - 7      sync header: syncword 0xAC41, escape 0xFFFF,
//                    24-bit frame_size 0x0001A7
//   offset 7 - 430    raw_ac4_frame()
//   offset 430 - 432  CRC
fn raw_frame() -> &'static [u8] {
    &include_bytes!("sample_2ch.ac4")[7..7 + 423]
}

fn raw_harness(sink_caps: Option<gst::Caps>) -> gst_check::Harness {
    let mut h = gst_check::Harness::new("ac4parse");
    if let Some(caps) = sink_caps {
        h.set_sink_caps(caps);
    }
    h.play();
    h.set_src_caps(
        gst::Caps::builder("audio/x-ac4")
            .field("stream-format", "raw")
            .build(),
    );
    h
}

fn syncframe_caps() -> gst::Caps {
    gst::Caps::builder("audio/x-ac4")
        .field("stream-format", "syncframe")
        .build()
}

fn output_stream_format(h: &gst_check::Harness) -> String {
    let caps = h.sinkpad().unwrap().current_caps().unwrap();
    caps.structure(0)
        .unwrap()
        .get::<String>("stream-format")
        .unwrap()
}

#[test]
fn test_parse_ac4_raw_to_syncframe() {
    init();

    let mut h = raw_harness(Some(syncframe_caps()));
    h.push(gst::Buffer::from_slice(raw_frame())).unwrap();

    let out_buffer = h.pull().unwrap();
    let map = out_buffer.map_readable().unwrap();
    assert_eq!(&map[..4], &[0xAC, 0x40, 0x01, 0xA7]);
    assert_eq!(&map[4..], raw_frame());
    assert_eq!(output_stream_format(&h), "syncframe");
}

#[test]
fn test_parse_ac4_raw_to_syncframe_extended_size() {
    init();

    let mut h = raw_harness(Some(syncframe_caps()));

    // Increase frame size so its size doesn't fit the 16-bit frame_size,
    // only the TOC at the start is parsed
    let mut frame = raw_frame().to_vec();
    frame.resize(0x10000, 0);
    h.push(gst::Buffer::from_slice(frame.clone())).unwrap();

    // 24-bit frame_size preceded by escape 0xFFFF
    let out_buffer = h.pull().unwrap();
    let map = out_buffer.map_readable().unwrap();
    assert_eq!(&map[..7], &[0xAC, 0x40, 0xFF, 0xFF, 0x01, 0x00, 0x00]);
    assert_eq!(&map[7..], frame.as_slice());
}

#[test]
fn test_parse_ac4_raw_drop_too_small() {
    init();

    let mut h = raw_harness(None);

    // Invalid TOC, dropped without corrupting the next frame
    h.push(gst::Buffer::from_slice([0u8; 4])).unwrap();
    h.push(gst::Buffer::from_slice(raw_frame())).unwrap();

    let out_buffer = h.pull().unwrap();
    let map = out_buffer.map_readable().unwrap();
    assert_eq!(map.as_slice(), raw_frame());
    assert!(h.try_pull().is_none());
}

#[test]
fn test_parse_ac4_no_stream_format() {
    init();

    let mut h = gst_check::Harness::new("ac4parse");
    h.play();

    // Without stream-format the input is parsed as sync frames
    h.set_src_caps(gst::Caps::builder("audio/x-ac4").build());
    h.push(gst::Buffer::from_slice(include_bytes!("sample_2ch.ac4")))
        .unwrap();

    let out_buffer = h.pull().unwrap();
    assert_eq!(out_buffer.size(), 432);
    assert_eq!(output_stream_format(&h), "syncframe");
}

#[test]
fn test_parse_ac4_unknown_stream_format() {
    init();

    let mut h = gst_check::Harness::new("ac4parse");
    h.play();

    h.set_src_caps(
        gst::Caps::builder("audio/x-ac4")
            .field("stream-format", "beaver")
            .build(),
    );
    assert_eq!(
        h.push(gst::Buffer::from_slice(raw_frame())),
        Err(gst::FlowError::NotNegotiated)
    );
}
