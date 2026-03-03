// Copyright (C) 2026 Collabora Ltd
//   @author: Daniel Morin <daniel.morin@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::sync::{Arc, Mutex};

use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstcompress::plugin_register_static().unwrap();
    });
}

struct Compressor {
    media_type: &'static str,
    compress_element: &'static str,
    decompress_element: &'static str,
}

const FLATE_ZLIB: Compressor = Compressor {
    media_type: "application/x-zlib-compressed",
    compress_element: "zlibcompress",
    decompress_element: "zlibdecompress",
};

const FLATE_DEFLATE: Compressor = Compressor {
    media_type: "application/x-deflate-compressed",
    compress_element: "deflatecompress",
    decompress_element: "deflatedecompress",
};

struct Pipeline(gst::Pipeline);

impl std::ops::Deref for Pipeline {
    type Target = gst::Pipeline;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

impl Pipeline {
    fn into_completion(self) {
        self.set_state(gst::State::Playing)
            .expect("Unable to set pipeline to Playing");

        for msg in self.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(..) => break,
                MessageView::Error(err) => {
                    panic!(
                        "Error from {src:?}: {error} ({debug:?})",
                        src = err.src().map(|s| s.path_string()),
                        error = err.error(),
                        debug = err.debug()
                    );
                }
                _ => (),
            }
        }
    }
}

// Attach a callback to `appsink` that copies each buffer's bytes into a
// shared Vec.  Returns the Vec that is populated as the pipeline runs.
fn collect_frames(appsink: &gst_app::AppSink) -> Arc<Mutex<Vec<Vec<u8>>>> {
    let frames: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let frames_clone = frames.clone();

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                frames_clone.lock().unwrap().push(map.as_slice().to_vec());
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    frames
}

// Generates compressible data: values cycling 0..100 provide enough
// repetition to compress well while not being trivially constant.
fn compressible_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 100) as u8).collect()
}

fn make_compress_harness(c: &Compressor) -> gst_check::Harness {
    gst_check::Harness::new(c.compress_element)
}

fn make_decompress_harness(c: &Compressor) -> gst_check::Harness {
    gst_check::Harness::new(c.decompress_element)
}

// direct compress + decompress pipeline
// N frames produced by the compressor all emerge from the decompressor.
fn frame_count_impl(c: &Compressor) {
    const NUM_BUFFERS: usize = 5;

    let fixed_caps = gst::Caps::builder("application").build();

    let mut h_compress = make_compress_harness(c);
    h_compress.set_src_caps(fixed_caps.clone());
    h_compress.set_sink_caps(gst::Caps::builder(c.media_type).build());
    h_compress.play();

    for _ in 0..NUM_BUFFERS {
        h_compress
            .push(gst::Buffer::from_slice(vec![0u8; 256]))
            .unwrap();
    }

    let compressed_caps = gst::Caps::builder(c.media_type)
        .field("original-caps", fixed_caps.clone())
        .build();

    let mut h_decompress = make_decompress_harness(c);
    h_decompress.set_src_caps(compressed_caps);
    h_decompress.set_sink_caps(fixed_caps);
    h_decompress.play();

    for _ in 0..NUM_BUFFERS {
        h_decompress.push(h_compress.pull().unwrap()).unwrap();
    }

    assert_eq!(
        h_decompress.buffers_in_queue() as usize,
        NUM_BUFFERS,
        "expected {NUM_BUFFERS} decompressed buffers"
    );
}

// Compress → decompress is lossless, each frame is byte-for-byte identical
// to the original produced by `videotestsrc`.
fn data_integrity_impl(c: &Compressor) {
    const NUM_BUFFERS: i32 = 3;

    let Ok(pipeline) = gst::parse::launch(&format!(
        "videotestsrc num-buffers={NUM_BUFFERS} \
         ! video/x-raw,format=RGB,width=32,height=24 \
         ! tee name=t \
           t. ! queue ! appsink name=original sync=false \
           t. ! queue ! {compress} ! {decompress} ! appsink name=processed sync=false",
        compress = c.compress_element,
        decompress = c.decompress_element,
    )) else {
        println!("could not build pipeline");
        return;
    };

    let pipeline = Pipeline(pipeline.downcast::<gst::Pipeline>().unwrap());

    let original_sink = pipeline
        .by_name("original")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let processed_sink = pipeline
        .by_name("processed")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();

    let original_frames = collect_frames(&original_sink);
    let processed_frames = collect_frames(&processed_sink);

    pipeline.into_completion();

    let original = original_frames.lock().unwrap();
    let processed = processed_frames.lock().unwrap();

    assert_eq!(original.len(), NUM_BUFFERS as usize);
    assert_eq!(processed.len(), NUM_BUFFERS as usize);

    for (i, (orig, proc)) in original.iter().zip(processed.iter()).enumerate() {
        assert_eq!(orig, proc, "frame {i} differs after compress → decompress");
    }
}

// GDP file round-trip
// Frames written through `compress ! gdppay ! filesink` are fully
// recovered by `filesrc ! gdpdepay ! decompress`.
// The `original-caps` embedded in the compressed stream is carried by GDP,
// so the decompressor can restore the correct srcpad caps without out-of-band
// information.
fn gdp_file_roundtrip_impl(c: &Compressor) {
    const NUM_BUFFERS: i32 = 10;

    let dir = tempfile::TempDir::new().unwrap();
    let location = dir.path().join("test.bin");
    let location_str = location.to_str().unwrap();

    // Write
    let Ok(pipeline) = gst::parse::launch(&format!(
        "videotestsrc num-buffers={NUM_BUFFERS} \
         ! video/x-raw,format=RGB,width=32,height=24 \
         ! {compress} ! gdppay ! filesink location={location_str}",
        compress = c.compress_element,
    )) else {
        println!("could not build write pipeline");
        return;
    };
    Pipeline(pipeline.downcast::<gst::Pipeline>().unwrap()).into_completion();

    // Read
    let Ok(pipeline) = gst::parse::launch(&format!(
        "filesrc location={location_str} \
         ! gdpdepay ! {decompress} ! appsink name=sink sync=false",
        decompress = c.decompress_element,
    )) else {
        println!("could not build read pipeline ");
        return;
    };
    let pipeline = Pipeline(pipeline.downcast::<gst::Pipeline>().unwrap());
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let frames = collect_frames(&sink);

    pipeline.into_completion();

    assert_eq!(
        frames.lock().unwrap().len(),
        NUM_BUFFERS as usize,
        "expected {NUM_BUFFERS} frames from GDP round-trip"
    );
}

// Frames written through `compress ! filesink` (raw concatenated compressed
// streams) are recovered by `filesrc ! decompress ! rawvideoparse`.
//
// The decompressor uses `GstAdapter` to accumulate the arbitrary-sized
// chunks produced by `filesrc` and `flate2::Decompress` to detect stream
// boundaries.
fn raw_file_roundtrip_impl(c: &Compressor) {
    const NUM_BUFFERS: i32 = 10;
    const WIDTH: i32 = 32;
    const HEIGHT: i32 = 24;
    const FORMAT_CAPS: &str = "RGB";
    const FORMAT_PARSE: &str = "rgb";

    let dir = tempfile::TempDir::new().unwrap();
    let location = dir.path().join("test.bin");
    let location_str = location.to_str().unwrap();

    // Write each compressed frame
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc num-buffers={NUM_BUFFERS} \
         ! video/x-raw,format={FORMAT_CAPS},width={WIDTH},height={HEIGHT} \
         ! {compress} ! filesink location={location_str}",
        compress = c.compress_element,
    ))
    .expect("could not build write pipeline");
    Pipeline(pipeline.downcast::<gst::Pipeline>().unwrap()).into_completion();

    // The decompressor reassembles complete frames from the raw byte stream
    // using GstAdapter. No caps are embedded; downstream specifies the format
    // via rawvideoparse. The named decompressor element has fixed sink caps,
    // so negotiation with filesrc completes automatically.
    let pipeline = gst::parse::launch(&format!(
        "filesrc location={location_str} \
         ! {decompress} \
         ! rawvideoparse format={FORMAT_PARSE} width={WIDTH} height={HEIGHT} \
         ! appsink name=sink sync=false",
        decompress = c.decompress_element,
    ))
    .expect("could not build read pipeline");
    let pipeline = Pipeline(pipeline.downcast::<gst::Pipeline>().unwrap());
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let frames = collect_frames(&sink);

    pipeline.into_completion();

    assert_eq!(
        frames.lock().unwrap().len(),
        NUM_BUFFERS as usize,
        "expected {NUM_BUFFERS} frames from raw-file round-trip"
    );
}

// Level 9 produces smaller output than level 1 for compressible data.
fn compression_level_impl(c: &Compressor) {
    // Cycling values between 0 and 99: enough repetition to be compressible,
    // but not constant (which would be trivially compressible at any level).
    let data = compressible_data(4096);

    let compressed_size = |level: u32| -> usize {
        let mut h = make_compress_harness(c);
        h.element().unwrap().set_property("level", level);
        h.set_src_caps(gst::Caps::builder("application").build());
        h.set_sink_caps(gst::Caps::builder(c.media_type).build());
        h.play();
        h.push(gst::Buffer::from_slice(data.clone())).unwrap();
        h.pull().unwrap().size()
    };

    let size_1 = compressed_size(1);
    let size_9 = compressed_size(9);

    assert!(
        size_9 <= size_1,
        "level 9 ({size_9} bytes) should not exceed level 1 ({size_1} bytes)"
    );
}

// Compressor srcpad caps carry `original-caps=(GstCaps)` containing
// the upstream video format.
fn original_caps_embedded_impl(c: &Compressor) {
    let raw_caps = gst::Caps::builder("video/x-raw")
        .field("format", "RGB")
        .field("width", 320i32)
        .field("height", 240i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .build();

    let mut h = make_compress_harness(c);
    h.set_src_caps(raw_caps.clone());
    h.play();

    // Push one buffer to trigger caps negotiation.
    let buf = {
        let mut b = gst::Buffer::with_size(50).unwrap();
        b.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
        b
    };
    h.push(buf).unwrap();

    // Drain events: StreamStart, Caps, Segment.
    let _stream_start = h.pull_event().unwrap();
    let caps_ev = h.pull_event().unwrap();
    assert_eq!(caps_ev.type_(), gst::EventType::Caps);

    let compressed_caps = if let gst::EventView::Caps(c) = caps_ev.view() {
        c.caps().to_owned()
    } else {
        panic!("expected Caps event");
    };

    let s = compressed_caps.structure(0).unwrap();
    assert_eq!(s.name(), c.media_type);

    let embedded = s
        .get::<gst::Caps>("original-caps")
        .expect("compressed caps must carry original-caps field");
    assert!(
        embedded.can_intersect(&raw_caps),
        "embedded original-caps {embedded:?} must be compatible with input caps {raw_caps:?}"
    );
}

// Decompressor srcpad caps are restored from the `original-caps` field
// in the incoming compressed caps.
fn srcpad_caps_restored_impl(c: &Compressor) {
    let raw_caps = gst::Caps::builder("video/x-raw")
        .field("format", "RGB")
        .field("width", 320i32)
        .field("height", 240i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .build();

    let compressed_caps = gst::Caps::builder(c.media_type)
        .field("original-caps", raw_caps.clone())
        .build();

    let mut h_compress = make_compress_harness(c);
    h_compress.set_src_caps(raw_caps.clone());
    h_compress.play();

    let buf = {
        let mut b = gst::Buffer::with_size(320 * 240 * 3).unwrap();
        b.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
        b
    };
    h_compress.push(buf).unwrap();
    // Drain compress-side events before reusing the compressed buffer.
    let _ = h_compress.pull_event();
    let _ = h_compress.pull_event();
    let _ = h_compress.pull_event();
    let compressed_buf = h_compress.pull().unwrap();

    let mut h = make_decompress_harness(c);
    h.set_src_caps(compressed_caps);
    h.play();
    h.push(compressed_buf).unwrap();

    let _stream_start = h.pull_event().unwrap();
    let caps_ev = h.pull_event().unwrap();
    assert_eq!(caps_ev.type_(), gst::EventType::Caps);

    let srcpad_caps = if let gst::EventView::Caps(c) = caps_ev.view() {
        c.caps().to_owned()
    } else {
        panic!("expected Caps event");
    };
    assert!(
        srcpad_caps.can_intersect(&raw_caps),
        "decompressor srcpad caps {srcpad_caps:?} must match original-caps {raw_caps:?}"
    );
}

// A compressed buffer split across two input buffers is correctly reassembled
// as part of decompression.
fn fragmented_input_reassembly_impl(c: &Compressor) {
    let fixed_caps = gst::Caps::builder("application").build();

    // Use cycling data so the compressed output is large enough to split
    // into two non-trivial halves.
    let data = compressible_data(4096);

    let mut h_compress = make_compress_harness(c);
    h_compress.set_src_caps(fixed_caps.clone());
    h_compress.set_sink_caps(gst::Caps::builder(c.media_type).build());
    h_compress.play();
    h_compress
        .push(gst::Buffer::from_slice(data.clone()))
        .unwrap();

    let compressed_buf = h_compress.pull().unwrap();
    let compressed_map = compressed_buf.map_readable().unwrap();
    let mid = compressed_map.len() / 2;

    let compressed_caps = gst::Caps::builder(c.media_type)
        .field("original-caps", fixed_caps.clone())
        .build();

    let mut h = make_decompress_harness(c);
    h.set_src_caps(compressed_caps);
    h.set_sink_caps(fixed_caps);
    h.play();

    // First half: stream is incomplete, no output buffer.
    h.push(gst::Buffer::from_slice(compressed_map[..mid].to_vec()))
        .unwrap();
    assert_eq!(
        h.buffers_in_queue(),
        0,
        "first half alone should not have produced output"
    );

    // Second half: stream is complete, one decompressed buffer.
    h.push(gst::Buffer::from_slice(compressed_map[mid..].to_vec()))
        .unwrap();
    assert_eq!(
        h.buffers_in_queue(),
        1,
        "second half must complete the stream"
    );

    // Also check data integrity.
    let decompressed = h.pull().unwrap();
    let decompressed_map = decompressed.map_readable().unwrap();
    assert_eq!(
        decompressed_map.as_slice(),
        data.as_slice(),
        "decompressed data must match original"
    );
}

// GstMeta attached to the original buffer must survive the compress/decompress
// round-trip. The compressor propagates them via BaseTransform::transform_meta
// and the decompressor restores them via adapter::buffer_fast + copy_into.
//
// GstReferenceTimestampMeta is used here for testing purpose only
fn meta_propagation_impl(c: &Compressor) {
    let fixed_caps = gst::Caps::builder("application").build();
    let reference_caps = gst::Caps::builder("timestamp/x-ntp").build();

    let mut h_compress = make_compress_harness(c);
    h_compress.set_src_caps(fixed_caps.clone());
    h_compress.set_sink_caps(gst::Caps::builder(c.media_type).build());
    h_compress.play();

    let mut buf = gst::Buffer::from_slice(compressible_data(10));
    gst::ReferenceTimestampMeta::add(
        buf.get_mut().unwrap(),
        &reference_caps,
        gst::ClockTime::from_seconds(42),
        gst::ClockTime::NONE,
    );
    h_compress.push(buf).unwrap();

    let compressed_buf = h_compress.pull().unwrap();
    assert!(
        compressed_buf
            .meta::<gst::ReferenceTimestampMeta>()
            .is_some(),
        "compressor must propagate metas to the compressed buffer"
    );

    let compressed_caps = gst::Caps::builder(c.media_type)
        .field("original-caps", fixed_caps.clone())
        .build();

    let mut h_decompress = make_decompress_harness(c);
    h_decompress.set_src_caps(compressed_caps);
    h_decompress.set_sink_caps(fixed_caps);
    h_decompress.play();

    h_decompress.push(compressed_buf).unwrap();

    let decompressed_buf = h_decompress.pull().unwrap();
    let meta = decompressed_buf
        .meta::<gst::ReferenceTimestampMeta>()
        .expect("decompressor must restore metas from the compressed buffer");
    assert_eq!(
        meta.timestamp(),
        gst::ClockTime::from_seconds(42),
        "restored meta timestamp must match the original"
    );
}

// Corrupted compressed data must be rejected and must not produce output
// buffers. This is specific to compressor with an integrity check like zlib
fn corruption_detected_impl(c: &Compressor) {
    let fixed_caps = gst::Caps::builder("application").build();
    let data = compressible_data(1024);

    let mut h_compress = make_compress_harness(c);
    h_compress.set_src_caps(fixed_caps.clone());
    h_compress.set_sink_caps(gst::Caps::builder(c.media_type).build());
    h_compress.play();
    h_compress.push(gst::Buffer::from_slice(data)).unwrap();

    let compressed_buf = h_compress.pull().unwrap();

    // Flip bytes in the middle of the compressed payload.
    let mut corrupted = compressed_buf.map_readable().unwrap().as_slice().to_vec();
    let mid = corrupted.len() / 2;
    corrupted[mid] ^= 0xff;
    corrupted[mid + 1] ^= 0xff;

    let compressed_caps = gst::Caps::builder(c.media_type)
        .field("original-caps", fixed_caps.clone())
        .build();

    let mut h = make_decompress_harness(c);
    h.set_src_caps(compressed_caps);
    h.set_sink_caps(fixed_caps);
    h.play();

    let result = h.push(gst::Buffer::from_slice(corrupted));
    assert!(result.is_err(), "decompressor must error on corrupted data");
    assert_eq!(
        h.buffers_in_queue(),
        0,
        "no buffer must be pushed downstream on corruption"
    );
}

// Seek events are refused on the compressor srcpad: independently-compressed
// frames cannot support byte-accurate seeking without an external index.
fn seek_refused_impl(c: &Compressor) {
    let mut h = make_compress_harness(c);
    h.set_src_caps(
        gst::Caps::builder("video/x-raw")
            .field("format", "RGB")
            .field("width", 320i32)
            .field("height", 240i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .build(),
    );
    h.play();

    let seek = gst::event::Seek::new(
        1.0_f64,
        gst::SeekFlags::FLUSH,
        gst::SeekType::Set,
        gst::ClockTime::ZERO,
        gst::SeekType::None,
        gst::ClockTime::NONE,
    );
    assert!(
        !h.push_upstream_event(seek),
        "compressor must refuse seek events"
    );
}

#[test]
fn test_zlib_frame_count() {
    init();
    frame_count_impl(&FLATE_ZLIB);
}

#[test]
fn test_deflate_frame_count() {
    init();
    frame_count_impl(&FLATE_DEFLATE);
}

#[test]
fn test_zlib_data_integrity() {
    init();
    data_integrity_impl(&FLATE_ZLIB);
}

#[test]
fn test_deflate_data_integrity() {
    init();
    data_integrity_impl(&FLATE_DEFLATE);
}

#[test]
fn test_zlib_gdp_file_roundtrip() {
    init();
    gdp_file_roundtrip_impl(&FLATE_ZLIB);
}

#[test]
fn test_deflate_gdp_file_roundtrip() {
    init();
    gdp_file_roundtrip_impl(&FLATE_DEFLATE);
}

#[test]
fn test_zlib_raw_file_roundtrip() {
    init();
    raw_file_roundtrip_impl(&FLATE_ZLIB);
}

#[test]
fn test_deflate_raw_file_roundtrip() {
    init();
    raw_file_roundtrip_impl(&FLATE_DEFLATE);
}

#[test]
fn test_zlib_compression_level() {
    init();
    compression_level_impl(&FLATE_ZLIB);
}

#[test]
fn test_deflate_compression_level() {
    init();
    compression_level_impl(&FLATE_DEFLATE);
}

#[test]
fn test_zlib_original_caps_embedded() {
    init();
    original_caps_embedded_impl(&FLATE_ZLIB);
}

#[test]
fn test_deflate_original_caps_embedded() {
    init();
    original_caps_embedded_impl(&FLATE_DEFLATE);
}

#[test]
fn test_zlib_srcpad_caps_restored() {
    init();
    srcpad_caps_restored_impl(&FLATE_ZLIB);
}

#[test]
fn test_deflate_srcpad_caps_restored() {
    init();
    srcpad_caps_restored_impl(&FLATE_DEFLATE);
}

#[test]
fn test_zlib_fragmented_input_reassembly() {
    init();
    fragmented_input_reassembly_impl(&FLATE_ZLIB);
}

#[test]
fn test_deflate_fragmented_input_reassembly() {
    init();
    fragmented_input_reassembly_impl(&FLATE_DEFLATE);
}

#[test]
fn test_zlib_meta_propagation() {
    init();
    meta_propagation_impl(&FLATE_ZLIB);
}

#[test]
fn test_deflate_meta_propagation() {
    init();
    meta_propagation_impl(&FLATE_DEFLATE);
}

#[test]
fn test_corruption_detected() {
    init();
    corruption_detected_impl(&FLATE_ZLIB);
}

#[test]
fn test_zlib_seek_refused() {
    init();
    seek_refused_impl(&FLATE_ZLIB);
}

#[test]
fn test_deflate_seek_refused() {
    init();
    seek_refused_impl(&FLATE_DEFLATE);
}
