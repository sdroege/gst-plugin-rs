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

    let caps_in = gst::Caps::builder("audio/x-ac4").build();
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
    let framed = structure.get::<bool>("framed").unwrap();
    assert!(framed);

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
