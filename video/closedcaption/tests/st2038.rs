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
const ST2038_PACKET: &[u8; 100] = &[
    0x00, 0x02, 0x40, 0x02, 0x61, 0x80, 0x64, 0x96, 0x59, 0x69, 0x92, 0x64, 0xf9, 0x0d, 0x00, 0x8f,
    0x97, 0x2b, 0xd1, 0xfc, 0xa0, 0x28, 0x0b, 0xf6, 0x80, 0xa0, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90,
    0x04, 0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04,
    0x01, 0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01,
    0xfa, 0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0xfa,
    0x40, 0x10, 0x07, 0xe9, 0x00, 0x40, 0x1f, 0xa4, 0x01, 0x00, 0x7e, 0x90, 0x04, 0x01, 0x74, 0x40,
    0x23, 0xe9, 0x0d, 0xab,
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

fn test_st2038_combiner_extractor(
    with_meta: bool,
    remove_meta: bool,
    same_pts: bool,
    combiner_meta_count: usize,
    extractor_meta_count: usize,
) {
    init();

    let st2038_buffers = st2038_buffers(same_pts);
    let in_segment = gst::FormattedSegment::<gst::ClockTime>::new();

    let video_caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::I420, 320, 240)
        .fps(gst::Fraction::new(30, 1))
        .build()
        .unwrap()
        .to_caps()
        .unwrap();
    let st2038_caps = gst::Caps::builder("meta/x-st-2038")
        .field("alignment", "frame")
        .build();

    let combiner = gst::ElementFactory::make("st2038combiner").build().unwrap();

    let mut h0 = gst_check::Harness::with_element(&combiner, Some("sink"), Some("src"));
    let mut h1 = gst_check::Harness::with_element(&combiner, None, None);

    h0.set_sink_caps(video_caps.clone());
    h0.set_src_caps(video_caps.clone());
    h0.play();
    assert!(h0.push_event(gst::event::Segment::builder(&in_segment).build()));

    if with_meta || remove_meta {
        h1.add_element_sink_pad(&combiner.request_pad_simple("st2038").unwrap());
        h1.set_sink_caps(st2038_caps);
        h1.play();
        assert!(h1.push_event(gst::event::Segment::builder(&in_segment).build()));
    }

    let mut combiner_buffers = Vec::<gst::Buffer>::new();

    for (frame_num, pair) in st2038_buffers.chunks_exact(2).enumerate() {
        let video_pts = gst::ClockTime::from_nseconds((frame_num + 1) as u64 * FRAME_DURATION_NS);
        let mut video_buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = video_buffer.get_mut().unwrap();
            buffer.set_pts(video_pts);
            buffer.set_dts(video_pts);
            buffer.set_duration(gst::ClockTime::from_nseconds(FRAME_DURATION_NS));
        }

        // Push 2 ST-2038 buffer for every video frame.
        if with_meta || remove_meta {
            assert_eq!(h1.push(pair[0].clone()), Ok(gst::FlowSuccess::Ok));
            assert_eq!(h1.push(pair[1].clone()), Ok(gst::FlowSuccess::Ok));
        }

        assert_eq!(h0.push(video_buffer), Ok(gst::FlowSuccess::Ok));

        let buffer = h0.pull().unwrap();
        combiner_buffers.push(buffer);
    }

    assert_eq!(combiner_buffers.len(), NUM_ST2038_BUFFERS / 2);

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

    let extractor = gst::ElementFactory::make("st2038extractor")
        .property("remove-ancillary-meta", remove_meta)
        .build()
        .unwrap();
    let mut h2 = gst_check::Harness::with_element(&extractor, Some("sink"), Some("src"));

    h2.set_sink_caps(video_caps.clone());
    h2.set_src_caps(video_caps);

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

    h2.play();

    assert!(h2.push_event(gst::event::Segment::builder(&in_segment).build()));

    for cb in combiner_buffers {
        assert_eq!(h2.push(cb), Ok(gst::FlowSuccess::Ok));
    }
    h2.push_event(gst::event::Eos::new());

    let mut extractor_buffers = Vec::<gst::Buffer>::new();
    while let Some(buffer) = h2.try_pull() {
        extractor_buffers.push(buffer);
    }
    assert_eq!(extractor_buffers.len(), NUM_ST2038_BUFFERS / 2);

    for eb in extractor_buffers {
        assert_eq!(
            eb.iter_meta::<AncillaryMeta>().count(),
            extractor_meta_count
        );
    }
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
