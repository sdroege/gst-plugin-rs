// Copyright (C) 2024 Collabora Ltd
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use gst_analytics::AnalyticsMetaRefExt;
use gst_analytics::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstoriginalbuffer::plugin_register_static().unwrap();
    });
}

/// Pushes `buffer` through a fresh `originalbuffersave` element and returns
/// the resulting buffer, which carries an `OriginalBufferMeta`.
fn save(buffer: gst::Buffer, caps: &gst::Caps) -> gst::Buffer {
    let mut h = gst_check::Harness::new("originalbuffersave");
    h.set_src_caps(caps.clone());
    h.play();
    h.push(buffer).unwrap();
    h.pull().unwrap()
}

/// Pushes `buffer` (expected to carry an `OriginalBufferMeta`) through a
/// fresh `originalbufferrestore` element and returns the restored buffer.
fn restore(buffer: gst::Buffer, caps: &gst::Caps) -> gst::Buffer {
    let mut h = gst_check::Harness::new("originalbufferrestore");
    h.set_src_caps(caps.clone());
    h.play();
    h.push(buffer).unwrap();
    h.pull().unwrap()
}

/// Simulates what a real transforming element does to metadata: for every
/// meta on `src`, invoke its registered transform function with `mt`,
/// carrying it (repositioned) over onto a fresh destination buffer. This
/// drives `OriginalBufferMeta`'s own transform callback the same way a real
/// `videoscale`/`videocrop`-like element would, so it records the transform.
fn apply_transform<MT: gst::meta::MetaTransform>(src: &gst::Buffer, mt: &MT) -> gst::Buffer {
    let mut dst = gst::Buffer::new();
    {
        let dst_mut = dst.make_mut();
        for meta in src.iter_meta::<gst::Meta>() {
            let _ = meta.transform(dst_mut, mt);
        }
    }
    dst
}

fn full_frame_rect(info: &gst_video::VideoInfo) -> gst_video::VideoRectangle {
    gst_video::VideoRectangle::new(0, 0, info.width() as i32, info.height() as i32)
}

fn video_info(width: u32, height: u32) -> gst_video::VideoInfo {
    gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, width, height)
        .build()
        .unwrap()
}

fn od_type() -> glib::Quark {
    glib::Quark::from_str("test-object")
}

// A buffer with a `VideoRegionOfInterestMeta` saved, transformed with a
// no-op ("gst-copy") transform and restored, should come back unchanged.
#[test]
fn test_copy_roundtrip() {
    init();

    let mut buf = gst::Buffer::with_size(16).unwrap();
    gst_video::VideoRegionOfInterestMeta::add(buf.make_mut(), "face", (10, 20, 30, 40));

    let caps = video_info(320, 240).to_caps().unwrap();
    let saved = save(buf, &caps);
    let transformed = apply_transform(&saved, &gst::meta::MetaTransformCopy::new(..));
    let restored = restore(transformed, &caps);

    let roi = restored
        .meta::<gst_video::VideoRegionOfInterestMeta>()
        .expect("ROI meta should survive a copy-only round trip");
    assert_eq!(roi.rect(), (10, 20, 30, 40));
}

// A ROI added before a 2x upscale, and a keypoint added by a downstream
// "detector" after the upscale, should both land back in the original
// (pre-scale) coordinate space after restore.
#[test]
fn test_scale_roundtrip() {
    init();

    let in_info = video_info(320, 240);
    let out_info = video_info(640, 480);

    let mut buf = gst::Buffer::with_size(16).unwrap();
    gst_video::VideoRegionOfInterestMeta::add(buf.make_mut(), "face", (10, 20, 30, 40));

    let caps = in_info.to_caps().unwrap();
    let saved = save(buf, &caps);

    let scale = gst_video::video_meta::VideoMetaTransformScale::new(&in_info, &out_info);
    let mut transformed = apply_transform(&saved, &scale);

    // A downstream detector found a keypoint at (400, 300) in the upscaled
    // (640x480) frame.
    {
        let dst = transformed.make_mut();
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(dst);
        relation
            .add_keypoint_mtd(
                gst_analytics::KeypointDimensions::_2d,
                400,
                300,
                0,
                gst_analytics::KeypointVisibility::VISIBLE,
                0.9,
            )
            .unwrap();
    }

    let restored = restore(transformed, &caps);

    let roi = restored
        .meta::<gst_video::VideoRegionOfInterestMeta>()
        .expect("ROI meta should survive a scale round trip");
    assert_eq!(roi.rect(), (10, 20, 30, 40));

    let relation = restored
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .expect("relation meta should survive a scale round trip");
    let keypoint = relation
        .iter::<gst_analytics::AnalyticsKeypointMtd>()
        .next()
        .expect("keypoint mtd should survive a scale round trip");
    let pos = keypoint.position().unwrap();
    assert_eq!((pos.x, pos.y), (200, 150));
}

// An arbitrary (non-scale) homography matrix, e.g. a translation, applied
// between save and restore should still be correctly inverted.
#[test]
fn test_matrix_roundtrip() {
    init();

    let info = video_info(320, 240);
    let rect = full_frame_rect(&info);

    let buf = gst::Buffer::with_size(16).unwrap();

    let caps = info.to_caps().unwrap();
    let saved = save(buf, &caps);

    let mut matrix =
        gst_video::video_meta::VideoMetaTransformMatrix::new(&info, &rect, &info, &rect);
    // Translate by (+50, +30).
    *matrix.matrix_mut() = [[1.0, 0.0, 50.0], [0.0, 1.0, 30.0], [0.0, 0.0, 1.0]];

    let mut transformed = apply_transform(&saved, &matrix);

    {
        let dst = transformed.make_mut();
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(dst);
        relation
            .add_keypoint_mtd(
                gst_analytics::KeypointDimensions::_2d,
                150,
                130,
                0,
                gst_analytics::KeypointVisibility::VISIBLE,
                0.9,
            )
            .unwrap();
    }

    let restored = restore(transformed, &caps);

    let relation = restored
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .expect("relation meta should survive a matrix round trip");
    let keypoint = relation
        .iter::<gst_analytics::AnalyticsKeypointMtd>()
        .next()
        .expect("keypoint mtd should survive a matrix round trip");
    let pos = keypoint.position().unwrap();
    assert_eq!((pos.x, pos.y), (100, 100));
}

#[test]
fn test_matrix_scale_object_detection_roundtrip() {
    init();

    let in_info = video_info(1920, 1080);
    let out_info = video_info(320, 240);
    let original = gst::Buffer::with_size(16).unwrap();
    let saved = save(original.clone(), &in_info.to_caps().unwrap());
    let transform = gst_video::video_meta::VideoMetaTransformMatrix::new(
        &in_info,
        &full_frame_rect(&in_info),
        &out_info,
        &gst_video::VideoRectangle::new(0, 30, 320, 180),
    );
    let mut transformed = apply_transform(&saved, &transform);

    {
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(transformed.make_mut());
        relation
            .add_od_mtd(od_type(), 135, 20, 64, 88, 0.9)
            .unwrap();
    }

    let out_caps = out_info.to_caps().unwrap();
    let restored = restore(transformed, &out_caps);
    let relation = restored
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .expect("relation meta should survive a matrix scale round trip");
    let detection = relation
        .iter::<gst_analytics::AnalyticsODMtd>()
        .next()
        .expect("object detection should survive a matrix scale round trip");
    let location = detection.location().unwrap();

    assert_eq!(
        (location.x, location.y, location.w, location.h),
        (810, -59, 384, 527)
    );
}

// Cropping a frame so that a ROI ends up entirely outside of the cropped
// area must drop that ROI, rather than keep it at a clamped/wrong position.
#[test]
fn test_crop_discard() {
    init();

    let info = video_info(320, 240);
    // Crop the top-left 100x100 corner without resizing the frame: the rest
    // of the (still 320x240) downstream frame is not part of the crop.
    let crop_rect = gst_video::VideoRectangle::new(0, 0, 100, 100);

    let buf = gst::Buffer::with_size(16).unwrap();

    let caps = info.to_caps().unwrap();
    let saved = save(buf, &caps);

    // in_rectangle = out_rectangle = the crop area: this is how a
    // videocrop-like element that doesn't resize its output would build
    // the transform.
    let matrix =
        gst_video::video_meta::VideoMetaTransformMatrix::new(&info, &crop_rect, &info, &crop_rect);

    let mut transformed = apply_transform(&saved, &matrix);
    // A downstream detector found a ROI entirely outside the cropped area.
    gst_video::VideoRegionOfInterestMeta::add(transformed.make_mut(), "face", (200, 200, 30, 40));

    let restored = restore(transformed, &caps);

    assert!(
        restored
            .meta::<gst_video::VideoRegionOfInterestMeta>()
            .is_none(),
        "ROI outside the cropped area must be dropped"
    );
}

// Cropping then embedding the crop into a larger canvas (two chained
// transforms) must still correctly reposition a ROI added by a downstream
// detector that operated on the cropped image.
#[test]
fn test_crop_then_embed() {
    init();

    let info = video_info(320, 240);
    let embed_info = video_info(800, 600);

    // Crop out the (50,50)-(150,150) region into its own 100x100 buffer.
    let crop_rect = gst_video::VideoRectangle::new(50, 50, 100, 100);
    let cropped_full_rect = gst_video::VideoRectangle::new(0, 0, 100, 100);
    // Then place that 100x100 crop at (200, 100) in an 800x600 canvas.
    let embed_rect = gst_video::VideoRectangle::new(200, 100, 100, 100);

    let buf = gst::Buffer::with_size(16).unwrap();

    let caps = info.to_caps().unwrap();
    let saved = save(buf, &caps);

    let crop_matrix = gst_video::video_meta::VideoMetaTransformMatrix::new(
        &info,
        &crop_rect,
        &info,
        &cropped_full_rect,
    );
    let mut cropped = apply_transform(&saved, &crop_matrix);

    // A downstream detector found a ROI at (10,10)-(30,30) in the cropped
    // (100x100) image.
    gst_video::VideoRegionOfInterestMeta::add(cropped.make_mut(), "face", (10, 10, 20, 20));

    let embed_matrix = gst_video::video_meta::VideoMetaTransformMatrix::new(
        &info,
        &cropped_full_rect,
        &embed_info,
        &embed_rect,
    );
    let embedded = apply_transform(&cropped, &embed_matrix);

    let restored = restore(embedded, &caps);

    let roi = restored
        .meta::<gst_video::VideoRegionOfInterestMeta>()
        .expect("ROI inside the crop area should survive crop+embed+restore");
    // (50,50) crop offset + (10,10) ROI offset within the crop.
    assert_eq!(roi.rect(), (60, 60, 20, 20));
}

// An object-detection mtd added before save, and a keypoint mtd added by a
// downstream analytics element after save, must both be present (merged
// into a single relation meta) after restore.
#[test]
fn test_combination_merge() {
    init();

    let mut buf = gst::Buffer::with_size(16).unwrap();
    {
        let dst = buf.make_mut();
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(dst);
        relation.add_od_mtd(od_type(), 5, 5, 15, 15, 0.8).unwrap();
    }

    let caps = video_info(320, 240).to_caps().unwrap();
    let saved = save(buf, &caps);

    // No geometric transform happens here: the same buffer (still carrying
    // the pre-save OD mtd) keeps flowing downstream, and a downstream
    // analytics element adds a keypoint mtd to it in place, exactly as a
    // real metadata-preserving element would (no need to reallocate a
    // buffer just to add more metadata to an existing meta).
    let mut transformed = saved;

    {
        let dst = transformed.make_mut();
        // Note: relation meta already exists on `dst` (copied from the
        // pre-save meta above); adding to it merges the two.
        let mut relation = dst
            .meta_mut::<gst_analytics::AnalyticsRelationMeta>()
            .expect("relation meta should have been copied onto the transformed buffer");
        relation
            .add_keypoint_mtd(
                gst_analytics::KeypointDimensions::_2d,
                10,
                10,
                0,
                gst_analytics::KeypointVisibility::VISIBLE,
                0.9,
            )
            .unwrap();
    }

    let restored = restore(transformed, &caps);

    // Pre-save and post-save analytics content must merge into a single
    // relation meta (not end up as two separate relation metas), and both
    // mtd kinds must be present.
    assert_eq!(
        restored
            .iter_meta::<gst_analytics::AnalyticsRelationMeta>()
            .count(),
        1
    );
    let relation = restored
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .expect("relation meta should survive restore");
    assert!(
        relation
            .iter::<gst_analytics::AnalyticsODMtd>()
            .next()
            .is_some()
    );
    assert!(
        relation
            .iter::<gst_analytics::AnalyticsKeypointMtd>()
            .next()
            .is_some()
    );
}

// A recorded matrix that turns out to be singular (non-invertible) can't be
// used to reposition size-tagged metadata (e.g. a ROI); it must be dropped
// with a warning, while non-size-tagged metadata (e.g. a
// `ReferenceTimestampMeta`) is still carried through.
#[test]
fn test_singular_matrix_drops_size_tagged() {
    init();

    let info = video_info(320, 240);
    let rect = full_frame_rect(&info);

    let buf = gst::Buffer::with_size(16).unwrap();

    let caps = info.to_caps().unwrap();
    let saved = save(buf, &caps);

    let mut matrix =
        gst_video::video_meta::VideoMetaTransformMatrix::new(&info, &rect, &info, &rect);
    // Force a singular (rank-deficient) matrix.
    *matrix.matrix_mut() = [[0.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 1.0]];

    let mut transformed = apply_transform(&saved, &matrix);
    // Downstream metadata added after the (singular) transform.
    {
        let dst = transformed.make_mut();
        gst_video::VideoRegionOfInterestMeta::add(dst, "face", (10, 20, 30, 40));
        gst::meta::ReferenceTimestampMeta::add(
            dst,
            &gst::Caps::new_empty_simple("timestamp/x-test"),
            gst::ClockTime::from_seconds(1),
            None,
        );
    }

    let bus = gst::Bus::new();
    let h = {
        let mut h = gst_check::Harness::new("originalbufferrestore");
        h.element().unwrap().set_bus(Some(&bus));
        h.set_src_caps(caps.clone());
        h.play();
        h.push(transformed).unwrap();
        h
    };
    let restored = {
        let mut h = h;
        h.pull().unwrap()
    };

    assert!(
        restored
            .meta::<gst_video::VideoRegionOfInterestMeta>()
            .is_none(),
        "ROI must be dropped when the recorded matrix is singular"
    );
    assert!(
        restored
            .meta::<gst::meta::ReferenceTimestampMeta>()
            .is_some(),
        "non-size-tagged metadata must survive a singular matrix step"
    );

    // Dropping the ROI is now only reported via a `gst::warning!` debug log
    // on the element, not a message posted to the bus.
    let mut saw_warning = false;
    while let Some(msg) = bus.pop() {
        if let gst::MessageView::Warning(_) = msg.view() {
            saw_warning = true;
        }
    }
    assert!(
        !saw_warning,
        "dropping the ROI must no longer post a warning to the bus"
    );
}

/// Builds a `Gap` event carrying `original-buffer`/`original-caps` fields,
/// the same schema `originalbuffersave`'s `interval=` throttling uses (and
/// that `analyticsobjectcrop`'s size-based fallback also uses).
fn make_original_buffer_gap(
    pts: gst::ClockTime,
    duration: Option<gst::ClockTime>,
    original_buffer: gst::Buffer,
    original_caps: &gst::Caps,
) -> gst::Event {
    let mut gap = gst::event::Gap::new(pts, duration);
    let s = gap.make_mut().structure_mut();
    s.set("original-buffer", original_buffer);
    s.set("original-caps", original_caps.clone());
    gap
}

/// A `Gap` event carrying a nested `original-gap` (as pushed by
/// `originalbuffersave` when it receives a `Gap` event that already has an
/// `original-buffer`/`original-gap` field, i.e. forwarding through a second
/// `originalbuffersave` stage).
fn make_nested_gap(
    pts: gst::ClockTime,
    duration: Option<gst::ClockTime>,
    inner: gst::Event,
) -> gst::Event {
    let mut gap = gst::event::Gap::new(pts, duration);
    let s = gap.make_mut().structure_mut();
    s.set("original-gap", inner);
    gap
}

/// Drains events from the harness src pad until (and including) the first
/// `Segment` event is found, returning the views of the events that preceded
/// it (in order). Panics if no `Segment` event is found.
fn events_before_segment(h: &mut gst_check::Harness) -> Vec<gst::Event> {
    let mut before = Vec::new();
    loop {
        let event = h
            .try_pull_event()
            .expect("expected a Segment event among pulled events");
        if matches!(event.view(), gst::EventView::Segment(_)) {
            return before;
        }
        before.push(event);
    }
}

// When a GAP-restored buffer is the very first piece of data to flow
// through `originalbufferrestore` (before any normal buffer has gone
// through `sink_chain`), the stashed `Segment` event must still be
// forwarded before that data, not silently dropped. This is a regression
// test for a latent bug where the `Gap` event handler pushed downstream
// data without first flushing the pending segment.
#[test]
fn test_gap_restore_forwards_segment_first() {
    init();

    let info = video_info(320, 240);
    let caps = info.to_caps().unwrap();

    let mut h = gst_check::Harness::new("originalbufferrestore");
    h.play();
    // set_src_caps() pushes a Caps event (swallowed by originalbufferrestore,
    // which uses the *restored* caps instead) followed by a Segment event,
    // which gets stashed since caps/segment can only be sent once the
    // restored caps are known.
    h.set_src_caps(caps.clone());

    let mut original = gst::Buffer::with_size(16).unwrap();
    original.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    gst_video::VideoRegionOfInterestMeta::add(original.make_mut(), "face", (1, 2, 3, 4));

    let gap = make_original_buffer_gap(
        gst::ClockTime::ZERO,
        Some(33_333_333.nseconds()),
        original,
        &caps,
    );
    assert!(h.push_event(gap));

    // The Segment event must appear before any other downstream event (in
    // particular, before the restored-caps/GAP-derived data), confirming it
    // wasn't dropped or reordered. Only a leading StreamStart and the
    // restored Caps event are expected ahead of it.
    let before = events_before_segment(&mut h);
    assert!(
        before
            .iter()
            .any(|e| matches!(e.view(), gst::EventView::StreamStart(_))),
        "expected a StreamStart event before the Segment event"
    );
    assert!(
        before
            .iter()
            .any(|e| matches!(e.view(), gst::EventView::Caps(_))),
        "expected the restored Caps event before the Segment event"
    );
    assert_eq!(
        before.len(),
        2,
        "expected only StreamStart and Caps before the Segment event, got {before:?}"
    );

    let restored = h
        .try_pull()
        .expect("expected the restored original buffer after caps+segment");
    let roi = restored
        .meta::<gst_video::VideoRegionOfInterestMeta>()
        .unwrap();
    assert_eq!(roi.rect(), (1, 2, 3, 4));
}

// The stashed segment must only be forwarded once: after a GAP-restored
// buffer has already flushed it, a subsequent normal (OriginalBufferMeta)
// buffer must not trigger a second Segment event.
#[test]
fn test_gap_restore_segment_pushed_once() {
    init();

    let info = video_info(320, 240);
    let caps = info.to_caps().unwrap();

    let mut h = gst_check::Harness::new("originalbufferrestore");
    h.play();
    h.set_src_caps(caps.clone());

    let gap = make_original_buffer_gap(
        gst::ClockTime::ZERO,
        Some(33_333_333.nseconds()),
        gst::Buffer::with_size(16).unwrap(),
        &caps,
    );
    assert!(h.push_event(gap));

    // Drain the StreamStart + caps + segment + restored buffer from the
    // first GAP.
    let _ = events_before_segment(&mut h);
    let _ = h.try_pull().unwrap();

    // Now push a normal buffer with a saved OriginalBufferMeta.
    let saved = save(gst::Buffer::with_size(16).unwrap(), &caps);
    h.push(saved).unwrap();
    let _ = h.pull().unwrap();

    // No further Segment event should have been forwarded.
    let mut saw_segment = false;
    while let Some(event) = h.try_pull_event() {
        if matches!(event.view(), gst::EventView::Segment(_)) {
            saw_segment = true;
        }
    }
    assert!(
        !saw_segment,
        "the stashed segment must only be forwarded once"
    );
}

// A `Gap` event carrying a nested `original-gap` (no `original-buffer`)
// must also have the pending segment forwarded before the gap itself.
#[test]
fn test_gap_restore_nested_gap_forwards_segment_first() {
    init();

    let info = video_info(320, 240);
    let caps = info.to_caps().unwrap();

    let mut h = gst_check::Harness::new("originalbufferrestore");
    h.play();
    h.set_src_caps(caps.clone());

    let inner = gst::event::Gap::new(gst::ClockTime::ZERO, Some(33_333_333.nseconds()));
    let gap = make_nested_gap(gst::ClockTime::ZERO, Some(33_333_333.nseconds()), inner);
    assert!(h.push_event(gap));

    // Only a leading StreamStart is expected before the Segment on the
    // nested-gap path (no restored-caps event is pushed here, since there
    // is no `original-buffer`/`original-caps` to restore from).
    let before = events_before_segment(&mut h);
    assert!(
        before
            .iter()
            .any(|e| matches!(e.view(), gst::EventView::StreamStart(_))),
        "expected a StreamStart event before the Segment event"
    );
    assert_eq!(
        before.len(),
        1,
        "expected only StreamStart before the Segment event, got {before:?}"
    );

    let forwarded = h
        .try_pull_event()
        .expect("expected the forwarded nested Gap event");
    assert!(matches!(forwarded.view(), gst::EventView::Gap(_)));
}
