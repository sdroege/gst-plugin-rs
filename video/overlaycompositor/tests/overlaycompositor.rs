// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use std::sync::Once;

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstoverlaycompositor::plugin_register_static().unwrap();
    });
}

fn caps(meta: bool) -> gst::Caps {
    let mut caps = gst::Caps::builder("video/x-raw")
        .field("format", "BGRA")
        .field("width", 2i32)
        .field("height", 1i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .build();
    if meta {
        caps.make_mut()
            .features_mut(0)
            .unwrap()
            .add(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
    }
    caps
}

fn harness(input: gst::Caps, output: gst::Caps) -> gst_check::Harness {
    init();

    let mut h = gst_check::Harness::new("overlaycompositor");
    h.set_sink_caps(output);
    h.set_src_caps(input);
    h
}

fn gpu_caps(meta: bool) -> gst::Caps {
    let mut caps = caps(meta);
    let features = caps.make_mut().features_mut(0).unwrap();
    features.remove(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY);
    features.add("memory:D3D12Memory");
    caps
}

fn overlay_buffer() -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(vec![0, 0, 0, 255, 0, 0, 0, 255]);
    add_overlay(buffer.get_mut().unwrap(), 0, [0, 0, 255, 255]);
    add_overlay(buffer.get_mut().unwrap(), 1, [0, 255, 0, 255]);
    buffer
}

fn add_overlay(buffer: &mut gst::BufferRef, x: i32, pixel: [u8; 4]) {
    let mut pixels = gst::Buffer::from_mut_slice(pixel.to_vec());
    gst_video::VideoMeta::add(
        pixels.get_mut().unwrap(),
        gst_video::VideoFrameFlags::empty(),
        gst_video::VideoFormat::Bgra,
        1,
        1,
    )
    .unwrap();

    let rect = gst_video::VideoOverlayRectangle::new_raw(
        &pixels,
        x,
        0,
        1,
        1,
        gst_video::VideoOverlayFormatFlags::empty(),
    );

    let overlay = gst_video::VideoOverlayComposition::new([&rect]).unwrap();
    gst_video::VideoOverlayCompositionMeta::add(buffer, &overlay);
}

#[test]
fn passthrough_without_meta() {
    init();

    let mut h = harness(caps(false), caps(false));
    let pixels = [0, 0, 0, 255, 0, 0, 0, 255];
    let buffer = gst::Buffer::from_mut_slice(pixels.to_vec());
    let original = buffer.clone();
    let output = h.push_and_pull(buffer).unwrap();
    assert_eq!(output.as_ptr(), original.as_ptr());
    assert_eq!(&*output.map_readable().unwrap(), &pixels);
}

#[test]
fn blends_multiple_metas_and_preserves_shared_input() {
    init();
    let mut h = harness(caps(true), caps(false));
    let mut buffer = overlay_buffer();
    let b = buffer.get_mut().unwrap();
    b.set_pts(gst::ClockTime::SECOND);
    gst_video::VideoMeta::add(
        b,
        gst_video::VideoFrameFlags::empty(),
        gst_video::VideoFormat::Bgra,
        2,
        1,
    )
    .unwrap();

    let original = buffer.clone();
    let output = h.push_and_pull(buffer).unwrap();
    assert_eq!(
        &*output.map_readable().unwrap(),
        &[0, 0, 255, 255, 0, 255, 0, 255]
    );
    assert!(
        output
            .meta::<gst_video::VideoOverlayCompositionMeta>()
            .is_none()
    );
    assert!(output.meta::<gst_video::VideoMeta>().is_some());
    assert_eq!(output.pts(), original.pts());
    assert_eq!(
        &*original.map_readable().unwrap(),
        &[0, 0, 0, 255, 0, 0, 0, 255]
    );
    assert_eq!(
        original
            .iter_meta::<gst_video::VideoOverlayCompositionMeta>()
            .count(),
        2
    );
}

#[test]
fn passthrough_with_overlay_meta() {
    init();
    let mut h = harness(caps(true), caps(true));
    let buffer = overlay_buffer();
    let original = buffer.clone();
    let output = h.push_and_pull(buffer).unwrap();
    assert_eq!(output.as_ptr(), original.as_ptr());
    assert_eq!(
        &*output.map_readable().unwrap(),
        &[0, 0, 0, 255, 0, 0, 0, 255]
    );
    assert_eq!(
        output
            .iter_meta::<gst_video::VideoOverlayCompositionMeta>()
            .count(),
        2
    );
}

#[test]
fn non_system_memory_passthrough_without_overlay_feature() {
    init();

    let mut h = harness(gpu_caps(false), gpu_caps(false));
    let buffer = overlay_buffer();
    let original = buffer.clone();
    let output = h.push_and_pull(buffer).unwrap();
    assert_eq!(output.as_ptr(), original.as_ptr());
    assert_eq!(
        output
            .iter_meta::<gst_video::VideoOverlayCompositionMeta>()
            .count(),
        2
    );
}

#[test]
fn caps_transformation() {
    init();

    let h = harness(caps(true), caps(false));
    let element = h.element().unwrap();
    let sink_caps = element.static_pad("sink").unwrap().query_caps(None);
    let src_caps = element.static_pad("src").unwrap().query_caps(None);
    let mut expected = caps(false);

    expected.make_mut().append(caps(true));
    assert_eq!(sink_caps, expected);
    assert_eq!(src_caps, expected);

    for meta in [false, true] {
        let original = gpu_caps(meta);
        let h = harness(original.clone(), original.clone());
        let element = h.element().unwrap();
        assert_eq!(
            element.static_pad("sink").unwrap().query_caps(None),
            original
        );
        assert_eq!(
            element.static_pad("src").unwrap().query_caps(None),
            original
        );
    }
}

#[test]
fn rejects_non_system_memory_overlay_caps_without_downstream_support() {
    init();

    let mut h = harness(gpu_caps(true), gpu_caps(true));
    h.push_and_pull(overlay_buffer()).unwrap();

    h.set_sink_caps(gpu_caps(false));
    let element = h.element().unwrap();
    element.static_pad("src").unwrap().mark_reconfigure();
    assert_eq!(h.push(overlay_buffer()), Err(gst::FlowError::NotNegotiated));
    assert_eq!(h.buffers_in_queue(), 0);
}
