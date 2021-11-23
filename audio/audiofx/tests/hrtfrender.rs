// Copyright (C) 2021 Tomasz Andrzejak <andreiltd@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use gst::glib;
use gst::prelude::*;

use once_cell::sync::Lazy;
use std::io::{Cursor, Write};

static CONFIG: Lazy<glib::Bytes> = Lazy::new(|| {
    let mut buff = Cursor::new(vec![0u8; 1024]);

    write_config(&mut buff).unwrap();
    glib::Bytes::from_owned(buff.get_ref().to_owned())
});

// Generates a fake config
fn write_config(writer: &mut impl Write) -> std::io::Result<()> {
    const SAMPLE_RATE: u32 = 44_100;
    const LENGTH: u32 = 2;
    const VERTEX_COUNT: u32 = 2;
    const INDEX_COUNT: u32 = 2 * 3;

    writer.write_all(b"HRIR")?;
    writer.write_all(&SAMPLE_RATE.to_le_bytes())?;
    writer.write_all(&LENGTH.to_le_bytes())?;
    writer.write_all(&VERTEX_COUNT.to_le_bytes())?;
    writer.write_all(&INDEX_COUNT.to_le_bytes())?;

    // Write Indices
    for _ in 0..INDEX_COUNT {
        writer.write_all(&0u32.to_le_bytes())?;
    }

    // Write Vertices
    for _ in 0..VERTEX_COUNT {
        for _ in 0..3 {
            writer.write_all(&0u32.to_le_bytes())?;
        }

        for _ in 0..LENGTH * 2 {
            writer.write_all(&0f32.to_le_bytes())?;
        }
    }

    Ok(())
}

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsaudiofx::plugin_register_static().expect("Failed to register rsaudiofx plugin");
    });
}

fn build_harness(src_caps: gst::Caps, sink_caps: gst::Caps) -> (gst_check::Harness, gst::Element) {
    let hrtf = gst::ElementFactory::make("hrtfrender", None).unwrap();
    hrtf.set_property("hrir-raw", &*CONFIG);

    let mut h = gst_check::Harness::with_element(&hrtf, Some("sink"), Some("src"));
    h.set_caps(src_caps, sink_caps);

    (h, hrtf)
}

#[test]
fn test_hrtfrender_samples_in_samples_out() {
    init();

    let src_caps = gst::Caps::builder("audio/x-raw")
        .field("format", gst_audio::AUDIO_FORMAT_F32.to_str())
        .field("rate", 44_100i32)
        .field("channels", 1i32)
        .field("channel-mask", gst::Bitmask::new(0x1))
        .field("layout", "interleaved")
        .build();

    let sink_caps = gst::Caps::builder("audio/x-raw")
        .field("format", gst_audio::AUDIO_FORMAT_F32.to_str())
        .field("rate", 44_100i32)
        .field("channels", 2i32)
        .field("layout", "interleaved")
        .build();

    let (mut h, _) = build_harness(src_caps, sink_caps);
    h.play();

    let inbpf = 4;
    let outbpf = 8;
    let full_block = 512 * 8;

    let mut buffer = gst::Buffer::with_size(full_block * inbpf + 20 * inbpf).unwrap();
    let buffer_mut = buffer.get_mut().unwrap();

    let full_block_time = (full_block as u64)
        .mul_div_round(*gst::ClockTime::SECOND, 44_100)
        .map(gst::ClockTime::from_nseconds);

    buffer_mut.set_pts(gst::ClockTime::ZERO);
    buffer_mut.set_duration(full_block_time);
    buffer_mut.set_offset(0);

    let ret = h.push(buffer);
    assert!(ret.is_ok());

    let buffer = h.pull().unwrap();
    assert_eq!(buffer.size(), full_block * outbpf);

    h.push_event(gst::event::Eos::new());
    let buffer = h.pull().unwrap();

    assert_eq!(buffer.size(), 20 * outbpf);
    assert_eq!(buffer.offset(), full_block as u64);
    assert_eq!(buffer.pts(), full_block_time);

    let residue_time = 20
        .mul_div_round(*gst::ClockTime::SECOND, 44_100)
        .map(gst::ClockTime::from_nseconds);

    assert_eq!(buffer.duration(), residue_time);
}

#[test]
fn test_hrtfrender_implicit_spatial_objects() {
    init();

    let src_caps = gst::Caps::builder("audio/x-raw")
        .field("format", gst_audio::AUDIO_FORMAT_F32.to_str())
        .field("rate", 44_100i32)
        .field("channels", 8i32)
        .field("channel-mask", gst::Bitmask::new(0xc3f))
        .field("layout", "interleaved")
        .build();

    let sink_caps = gst::Caps::builder("audio/x-raw")
        .field("format", gst_audio::AUDIO_FORMAT_F32.to_str())
        .field("rate", 44_100i32)
        .field("channels", 2i32)
        .field("layout", "interleaved")
        .build();

    let (mut h, hrtf) = build_harness(src_caps, sink_caps);
    let objs = hrtf.property::<gst::Array>("spatial-objects");

    h.play();

    assert_eq!(objs.len(), 8);
}

#[test]
fn test_hrtfrender_explicit_spatial_objects() {
    init();

    let src_caps = gst::Caps::builder("audio/x-raw")
        .field("format", gst_audio::AUDIO_FORMAT_F32.to_str())
        .field("rate", 44_100i32)
        .field("channels", 8i32)
        .field("layout", "interleaved")
        .build();

    let sink_caps = gst::Caps::builder("audio/x-raw")
        .field("format", gst_audio::AUDIO_FORMAT_F32.to_str())
        .field("rate", 44_100i32)
        .field("channels", 2i32)
        .field("layout", "interleaved")
        .build();

    let (mut h, hrtf) = build_harness(src_caps, sink_caps);

    let objs = (0..8)
        .map(|x| {
            gst::Structure::builder("application/spatial-object")
                .field("x", -1f32 + x as f32 / 8f32)
                .field("y", 0f32)
                .field("z", 1f32)
                .field("distance-gain", 0.1f32)
                .build()
        })
        .collect::<Vec<_>>();

    hrtf.set_property("spatial-objects", gst::Array::new(objs));

    h.play();

    let objs = hrtf.property::<gst::Array>("spatial-objects");
    assert_eq!(objs.len(), 8);
}

#[test]
// Caps negotation should fail if we have mismatch between input channels and
// of objects that we set via property. In this test case input has 6 channels
// but the number of spatial objects set is 2.
fn test_hrtfrender_caps_negotiation_fail() {
    init();

    let src_caps = gst::Caps::builder("audio/x-raw")
        .field("format", gst_audio::AUDIO_FORMAT_F32.to_str())
        .field("rate", 44_100i32)
        .field("channels", 6i32)
        .field("layout", "interleaved")
        .build();

    let sink_caps = gst::Caps::builder("audio/x-raw")
        .field("format", gst_audio::AUDIO_FORMAT_F32.to_str())
        .field("rate", 44_100i32)
        .field("channels", 2i32)
        .field("layout", "interleaved")
        .build();

    let (mut h, hrtf) = build_harness(src_caps, sink_caps);

    let objs = (0..2)
        .map(|_| {
            gst::Structure::builder("application/spatial-object")
                .field("x", 0f32)
                .field("y", 0f32)
                .field("z", 1f32)
                .field("distance-gain", 0.1f32)
                .build()
        })
        .collect::<Vec<_>>();

    hrtf.set_property("spatial-objects", gst::Array::new(objs));

    h.play();

    let buffer = gst::Buffer::with_size(2048).unwrap();
    assert_eq!(h.push(buffer), Err(gst::FlowError::NotNegotiated));

    h.push_event(gst::event::Eos::new());

    // The harness sinkpad end up not having defined caps so, the current_caps
    // should be None
    let current_caps = h.sinkpad().expect("harness has no sinkpad").current_caps();

    assert!(current_caps.is_none());
}

#[test]
fn test_hrtfrender_multiple_instances_sharing_thread_pool() {
    init();

    let src_caps = gst::Caps::builder("audio/x-raw")
        .field("format", gst_audio::AUDIO_FORMAT_F32.to_str())
        .field("rate", 44_100i32)
        .field("channels", 1i32)
        .field("layout", "interleaved")
        .build();

    let sink_caps = gst::Caps::builder("audio/x-raw")
        .field("format", gst_audio::AUDIO_FORMAT_F32.to_str())
        .field("rate", 44_100i32)
        .field("channels", 2i32)
        .field("layout", "interleaved")
        .build();

    let (_, hrtf) = build_harness(src_caps.clone(), sink_caps.clone());

    let block_length: u64 = hrtf.property::<u64>("block-length");
    let steps: u64 = hrtf.property::<u64>("interpolation-steps");
    let bps: u64 = 4;

    drop(hrtf);
    let blksz = (block_length * steps * bps) as usize;

    let mut harnesses = (0..4)
        .map(|_| build_harness(src_caps.clone(), sink_caps.clone()).0)
        .collect::<Vec<_>>();

    for h in harnesses.iter_mut() {
        h.play();

        let buffer = gst::Buffer::with_size(blksz).unwrap();
        assert!(h.push(buffer).is_ok());

        let buffer = h.pull().unwrap();
        assert_eq!(buffer.size(), 2 * blksz);

        h.push_event(gst::event::Eos::new());
    }
}
