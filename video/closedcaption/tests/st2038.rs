// Copyright (C) 2025 Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use gst_video::video_meta::AncillaryMeta;

// ST2038 packet with a single CEA708 CC ANC packet. Taking one of the
// 100 byte ST-2038 buffers by using the pipeline.
// filesrc location=video/closedcaption/tests/captions-test_708.mcc ! mccparse ! ccconverter ! cctost2038anc ! fakesink dump=1 silent=false -v
//
// When parsed should give,
// AncDataHeader { c_not_y_channel_flag: false, did: 97, sdid: 1, line_number: 9, horizontal_offset: 0, data_count: 73, checksum: 427, len: 100 }
const ST2038_PACKET_CHECKSUM: u16 = 427;
const ST2038_PACKET: &[u8; 100] = &[
    0x00, 0x02, 0x40, 0x02, 0x61, 0x80, 0x64, 0x96, 0x59, 0x69, 0x92, 0x64, 0xf9, 0x0d, 0x00, 0x8f,
    0x97, 0x2b, 0xd1, 0xfc, 0xa0, 0x28, 0x0b, 0xf6, 0x80, 0xa0, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90,
    0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04,
    0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01,
    0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa,
    0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0x74, 0x40,
    0x23, 0xe9, 0x0d, 0xab,
];
/// Second valid ST-2038 packet; ANC user-data/checksum differ from `ST2038_PACKET`.
///
/// When parsed should give,
/// AncDataHeader { c_not_y_channel_flag: false, did: 97, sdid: 1, line_number: 9, horizontal_offset: 0, data_count: 73, checksum: 683, len: 100 }
const ST2038_PACKET_ALT_CHECKSUM: u16 = 683;
const ST2038_PACKET_ALT: &[u8; 100] = &[
    0x00, 0x02, 0x40, 0x02, 0x61, 0x80, 0x64, 0x96, 0x59, 0x69, 0x92, 0x64, 0xf9, 0x0e, 0x02, 0x8f,
    0x97, 0x2b, 0xd1, 0xfc, 0xa0, 0x28, 0x0b, 0xf6, 0x80, 0xa0, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90,
    0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04,
    0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01,
    0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa,
    0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0x74, 0x80,
    0xa3, 0xe4, 0xfe, 0xab,
];

const FRAME_DURATION_NS: u64 = gst::ClockTime::SECOND.nseconds() / 30;

const NUM_ST2038_BUFFERS: usize = 6;
const BUFFERS_PER_FRAME: usize = 2;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsclosedcaption::plugin_register_static().unwrap();
    });
}

fn st2038_buffers(use_same_pts: bool) -> Vec<gst::Buffer> {
    (0..NUM_ST2038_BUFFERS)
        .map(|idx| {
            let mut buffer = gst::Buffer::from_slice(ST2038_PACKET);
            let buffer_mut = buffer.get_mut().unwrap();

            let pts = if use_same_pts {
                gst::ClockTime::ZERO
            } else {
                // Evenly space 2 buffers within each frame
                let frame_num = idx / BUFFERS_PER_FRAME;
                let buffer_in_frame = idx % BUFFERS_PER_FRAME;
                let frame_start = frame_num as u64 * FRAME_DURATION_NS;
                let offset_in_frame =
                    buffer_in_frame as u64 * FRAME_DURATION_NS / BUFFERS_PER_FRAME as u64;

                gst::ClockTime::from_nseconds(frame_start + offset_in_frame)
            };

            buffer_mut.set_pts(pts);
            buffer_mut.set_duration(gst::ClockTime::from_nseconds(FRAME_DURATION_NS));

            buffer
        })
        .collect()
}

fn test_video_caps() -> gst::Caps {
    gst_video::VideoInfo::builder(gst_video::VideoFormat::I420, 320, 240)
        .fps(gst::Fraction::new(30, 1))
        .build()
        .unwrap()
        .to_caps()
        .unwrap()
}

fn st2038_alignment_caps(alignment: &str) -> gst::Caps {
    gst::Caps::builder("meta/x-st-2038")
        .field("alignment", alignment)
        .build()
}

/// The combiner is an aggregator with two sink pads, so it is driven through a
/// pipeline with a separate `appsrc` per pad rather than a `Harness`: a `Harness`
/// pushes on the calling thread, and pushing both ST-2038 buffers for a picture
/// before the video buffer that lets them be consumed deadlocks on the aggregator
/// pad's single-buffer slot.
///
/// The appsrc inputs are not live, so the aggregator never schedules a latency
/// timeout; it finalises a picture only once every pad has a buffer (or is EOS).
/// A picture's ST-2038 is therefore always fully collected before the picture is
/// finalised by the next picture's boundary buffer or by EOS, independent of how
/// the per-pad streaming threads are scheduled.
struct CombinerPipeline {
    pipeline: gst::Pipeline,
}

impl std::ops::Deref for CombinerPipeline {
    type Target = gst::Pipeline;

    fn deref(&self) -> &gst::Pipeline {
        &self.pipeline
    }
}

/// Builds a video (and optionally ST-2038) appsrc feeding the combiner into an
/// appsink. `st2038_alignment` is `None` to omit the ST-2038 pad entirely.
fn setup_combiner_pipeline(
    st2038_alignment: Option<&str>,
    drop_late_st2038: bool,
) -> CombinerPipeline {
    init();

    let pipeline = gst::Pipeline::default();

    let video_src = gst_app::AppSrc::builder()
        .name("videosrc")
        .format(gst::Format::Time)
        .caps(&test_video_caps())
        .build();

    let combiner = gst::ElementFactory::make("st2038combiner")
        .name("combiner")
        .property("drop-late-st2038", drop_late_st2038)
        .build()
        .unwrap();

    let sink = gst_app::AppSink::builder().name("sink").sync(false).build();

    pipeline
        .add_many([video_src.upcast_ref(), &combiner, sink.upcast_ref()])
        .unwrap();
    video_src
        .link_pads(Some("src"), &combiner, Some("sink"))
        .unwrap();
    combiner
        .link_pads(Some("src"), &sink, Some("sink"))
        .unwrap();

    if let Some(alignment) = st2038_alignment {
        let st2038_src = gst_app::AppSrc::builder()
            .name("st2038src")
            .format(gst::Format::Time)
            .caps(&st2038_alignment_caps(alignment))
            .build();
        pipeline.add(&st2038_src).unwrap();
        let st2038_pad = combiner.request_pad_simple("st2038").unwrap();
        st2038_src
            .static_pad("src")
            .unwrap()
            .link(&st2038_pad)
            .unwrap();
    }

    pipeline.set_state(gst::State::Playing).unwrap();

    CombinerPipeline { pipeline }
}

impl CombinerPipeline {
    fn appsrc(&self, name: &str) -> gst_app::AppSrc {
        self.by_name(name)
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap()
    }

    fn push_video(&self, pts: gst::ClockTime) {
        self.appsrc("videosrc")
            .push_buffer(video_buffer_at(pts))
            .unwrap();
    }

    fn push_st2038(&self, buffer: gst::Buffer) {
        self.appsrc("st2038src").push_buffer(buffer).unwrap();
    }

    /// Signal end-of-stream on every input so the final picture is flushed.
    fn eos(&self) {
        if let Some(st2038_src) = self.by_name("st2038src") {
            st2038_src
                .downcast::<gst_app::AppSrc>()
                .unwrap()
                .end_of_stream()
                .unwrap();
        }
        self.appsrc("videosrc").end_of_stream().unwrap();
    }

    fn pull(&self) -> gst::Buffer {
        let sink = self
            .by_name("sink")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        sink.pull_sample().unwrap().buffer_owned().unwrap()
    }

    fn stop(self) {
        self.pipeline.set_state(gst::State::Null).unwrap();
    }
}

fn video_buffer_at(pts: gst::ClockTime) -> gst::Buffer {
    let mut video_buffer = gst::Buffer::with_size(1).unwrap();
    {
        let buffer = video_buffer.get_mut().unwrap();
        buffer.set_pts(pts);
        buffer.set_dts(pts);
        buffer.set_duration(gst::ClockTime::from_nseconds(FRAME_DURATION_NS));
    }
    video_buffer
}

fn test_st2038_combiner_extractor(
    with_meta: bool,
    remove_meta: bool,
    same_pts: bool,
    combiner_meta_count: usize,
    extractor_meta_count: usize,
) {
    init();

    let st2038_buffers = st2038_buffers(same_pts);

    let frame_limit = if same_pts {
        1
    } else {
        NUM_ST2038_BUFFERS / BUFFERS_PER_FRAME
    };

    let pipeline = setup_combiner_pipeline((with_meta || remove_meta).then_some("packet"), false);

    let mut combiner_buffers = Vec::<gst::Buffer>::new();

    for (frame_num, pair) in st2038_buffers
        .chunks_exact(BUFFERS_PER_FRAME)
        .enumerate()
        .take(frame_limit)
    {
        let video_pts = gst::ClockTime::from_nseconds(frame_num as u64 * FRAME_DURATION_NS);

        // Push 2 ST-2038 buffer for every video frame.
        if with_meta || remove_meta {
            pipeline.push_st2038(pair[0].clone());
            pipeline.push_st2038(pair[1].clone());
        }

        pipeline.push_video(video_pts);
    }

    pipeline.eos();

    for _ in 0..frame_limit {
        combiner_buffers.push(pipeline.pull());
    }

    assert_eq!(combiner_buffers.len(), frame_limit);

    for cb in &combiner_buffers {
        assert_eq!(cb.iter_meta::<AncillaryMeta>().count(), combiner_meta_count);
        for meta in cb.iter_meta::<AncillaryMeta>() {
            assert_eq!(meta.data_count() & 0xFF, 73);
            // Below values are default with `cctost2038anc`
            assert!(!meta.c_not_y_channel());
            assert_eq!(meta.line(), 9);
            assert_eq!(meta.offset(), 0);
            assert_eq!(meta.did() & 0xFF, 97);
            assert_eq!(meta.sdid_block_number() & 0xFF, 1);
        }
    }

    pipeline.stop();

    let video_caps = test_video_caps();
    let in_segment = gst::FormattedSegment::<gst::ClockTime>::new();

    let extractor = gst::ElementFactory::make("st2038extractor")
        .property("remove-ancillary-meta", remove_meta)
        .build()
        .unwrap();
    let mut harness = gst_check::Harness::with_element(&extractor, Some("sink"), Some("src"));

    harness.set_sink_caps(video_caps.clone());
    harness.set_src_caps(video_caps);

    extractor.connect_closure(
        "pad-added",
        false,
        glib::closure!(move |_extractor: &gst::Element, pad: &gst::Pad| {
            assert_eq!(pad.name(), "st2038");

            pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buffer) = info.buffer() {
                    let num_buffers = buffer.size() / 100;

                    let map = buffer.map_readable().unwrap();
                    let slice = map.as_slice();

                    for idx in 0..num_buffers {
                        assert_eq!(ST2038_PACKET, &slice[(idx * 100)..((idx + 1) * 100)]);
                    }
                }

                gst::PadProbeReturn::Ok
            });
        }),
    );

    harness.play();

    assert!(harness.push_event(gst::event::Segment::builder(&in_segment).build()));

    for cb in combiner_buffers {
        assert_eq!(harness.push(cb), Ok(gst::FlowSuccess::Ok));
    }
    harness.push_event(gst::event::Eos::new());

    let mut extractor_buffers = Vec::<gst::Buffer>::new();
    while let Some(buffer) = harness.try_pull() {
        extractor_buffers.push(buffer);
    }
    assert_eq!(extractor_buffers.len(), frame_limit);

    for eb in extractor_buffers {
        assert_eq!(
            eb.iter_meta::<AncillaryMeta>().count(),
            extractor_meta_count
        );
    }

    drop(harness);
    let _ = extractor.set_state(gst::State::Null);
}

#[test]
fn test_st2038_extractor_meta_removal() {
    test_st2038_combiner_extractor(true, true, false, 2, 0);
}

#[test]
fn test_st2038_extractor_combiner_with_st2038() {
    test_st2038_combiner_extractor(true, false, false, 2, 2);
}

#[test]
fn test_st2038_extractor_combiner_without_st2038() {
    test_st2038_combiner_extractor(false, false, false, 0, 0);
}

#[test]
fn test_st2038_extractor_combiner_with_multiple_st2038_same_pts() {
    test_st2038_combiner_extractor(true, false, true, 2, 2);
}

fn st2038_buffer(packet: [u8; 100], pts: gst::ClockTime) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_slice(packet);
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(pts);
        buffer.set_duration(gst::ClockTime::from_nseconds(FRAME_DURATION_NS));
    }
    buffer
}

/// Late ST-2038 is collected by default and attached with in-window data for the same picture.
#[test]
fn test_st2038_combiner_collects_late_by_default() {
    let pipeline = setup_combiner_pipeline(Some("frame"), false);

    let in_window_pts = gst::ClockTime::from_nseconds(FRAME_DURATION_NS);

    pipeline.push_st2038(st2038_buffer(*ST2038_PACKET, gst::ClockTime::ZERO));
    pipeline.push_st2038(st2038_buffer(*ST2038_PACKET_ALT, in_window_pts));
    pipeline.push_video(in_window_pts);
    pipeline.eos();

    let output = pipeline.pull();
    assert_eq!(output.iter_meta::<AncillaryMeta>().count(), 2);

    let checksums: std::collections::BTreeSet<_> = output
        .iter_meta::<AncillaryMeta>()
        .map(|meta| meta.checksum())
        .collect();
    assert_eq!(
        checksums,
        std::collections::BTreeSet::from([ST2038_PACKET_CHECKSUM, ST2038_PACKET_ALT_CHECKSUM,])
    );

    pipeline.stop();
}

/// With drop-late-st2038, buffers before the current video window are not collected.
#[test]
fn test_st2038_combiner_drop_late_st2038_property() {
    let pipeline = setup_combiner_pipeline(Some("frame"), true);

    let in_window_pts = gst::ClockTime::from_nseconds(FRAME_DURATION_NS);

    pipeline.push_st2038(st2038_buffer(*ST2038_PACKET, gst::ClockTime::ZERO));
    pipeline.push_st2038(st2038_buffer(*ST2038_PACKET_ALT, in_window_pts));
    pipeline.push_video(in_window_pts);
    pipeline.eos();

    let output = pipeline.pull();
    assert_eq!(output.iter_meta::<AncillaryMeta>().count(), 1);
    let meta = output.iter_meta::<AncillaryMeta>().next().unwrap();
    assert_ne!(meta.checksum(), ST2038_PACKET_CHECKSUM);
    assert_eq!(meta.checksum(), ST2038_PACKET_ALT_CHECKSUM);

    pipeline.stop();
}
