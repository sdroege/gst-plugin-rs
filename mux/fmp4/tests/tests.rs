// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//

use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstfmp4::plugin_register_static().unwrap();
    });
}

fn to_completion(pipeline: &gst::Pipeline) {
    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to the `Playing` state");

    for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            _ => (),
        }
    }

    pipeline
        .set_state(gst::State::Null)
        .expect("Unable to set the pipeline to the `Null` state");
}

fn test_buffer_flags_single_stream(cmaf: bool, set_dts: bool, caps: gst::Caps) {
    let mut h = if cmaf {
        gst_check::Harness::new("cmafmux")
    } else {
        gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"))
    };

    // 5s fragment duration
    h.element()
        .unwrap()
        .set_property("fragment-duration", 5.seconds());

    h.set_src_caps(caps);
    h.play();

    let output_offset = if cmaf {
        gst::ClockTime::ZERO
    } else {
        (60 * 60 * 1000).seconds()
    };

    // Push 7 buffers of 1s each, 1st and 6 buffer without DELTA_UNIT flag
    for i in 0..7 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i.seconds());
            if set_dts {
                buffer.set_dts(i.seconds());
            }
            buffer.set_duration(gst::ClockTime::SECOND);
            if i != 0 && i != 5 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i == 2 {
            let ev = loop {
                let ev = h.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(5.seconds()),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Crank the clock: this should bring us to the end of the first fragment
    h.crank_single_clock_wait().unwrap();

    let header = h.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO + output_offset));
    if set_dts {
        assert_eq!(header.dts(), Some(gst::ClockTime::ZERO + output_offset));
    }

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    if set_dts {
        assert_eq!(
            fragment_header.dts(),
            Some(gst::ClockTime::ZERO + output_offset)
        );
    }
    assert_eq!(fragment_header.duration(), Some(5.seconds()));

    for i in 0..5 {
        let buffer = h.pull().unwrap();
        if i == 4 {
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
        } else {
            assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
        }
        assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));
        if set_dts {
            assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
        }
        assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
    }

    h.push_event(gst::event::Eos::new());

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(5.seconds() + output_offset));
    if set_dts {
        assert_eq!(fragment_header.dts(), Some(5.seconds() + output_offset));
    }
    assert_eq!(fragment_header.duration(), Some(2.seconds()));

    for i in 5..7 {
        let buffer = h.pull().unwrap();
        if i == 6 {
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
        } else {
            assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
        }
        assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));
        if set_dts {
            assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
        }
        assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
    }

    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_buffer_flags_single_h264_stream_cmaf() {
    init();

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::with_size(1).unwrap())
        .build();

    test_buffer_flags_single_stream(true, true, caps);
}

#[test]
fn test_buffer_flags_single_h264_stream_iso() {
    init();

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::with_size(1).unwrap())
        .build();

    test_buffer_flags_single_stream(false, true, caps);
}

#[test]
fn test_buffer_flags_single_vp9_stream_iso() {
    init();

    let caps = gst::Caps::builder("video/x-vp9")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("profile", "0")
        .field("chroma-format", "4:2:0")
        .field("bit-depth-luma", 8u32)
        .field("bit-depth-chroma", 8u32)
        .field("colorimetry", "bt709")
        .build();

    test_buffer_flags_single_stream(false, false, caps);
}

#[test]
fn test_buffer_flags_single_av1_stream_cmaf() {
    init();

    let caps = gst::Caps::builder("video/x-av1")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("profile", "main")
        .field("tier", "main")
        .field("level", "4.1")
        .field("chroma-format", "4:2:0")
        .field("bit-depth-luma", 8u32)
        .field("bit-depth-chroma", 8u32)
        .field("colorimetry", "bt709")
        .build();

    test_buffer_flags_single_stream(true, false, caps);
}

#[test]
fn test_buffer_flags_multi_stream() {
    init();

    let mut h1 = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));
    let mut h2 = gst_check::Harness::with_element(&h1.element().unwrap(), Some("sink_1"), None);

    // 5s fragment duration
    h1.element()
        .unwrap()
        .set_property("fragment-duration", 5.seconds());

    h1.set_src_caps(
        gst::Caps::builder("video/x-h264")
            .field("width", 1920i32)
            .field("height", 1080i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .field("stream-format", "avc")
            .field("alignment", "au")
            .field("codec_data", gst::Buffer::with_size(1).unwrap())
            .build(),
    );
    h1.play();

    h2.set_src_caps(
        gst::Caps::builder("audio/mpeg")
            .field("mpegversion", 4i32)
            .field("channels", 1i32)
            .field("rate", 44100i32)
            .field("stream-format", "raw")
            .field("base-profile", "lc")
            .field("profile", "lc")
            .field("level", "2")
            .field(
                "codec_data",
                gst::Buffer::from_slice([0x12, 0x08, 0x56, 0xe5, 0x00]),
            )
            .build(),
    );
    h2.play();

    let output_offset = (60 * 60 * 1000).seconds();

    // Push 7 buffers of 1s each, 1st and last buffer without DELTA_UNIT flag
    for i in 0..7 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i.seconds());
            buffer.set_dts(i.seconds());
            buffer.set_duration(gst::ClockTime::SECOND);
            if i != 0 && i != 5 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h1.push(buffer), Ok(gst::FlowSuccess::Ok));

        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i.seconds());
            buffer.set_dts(i.seconds());
            buffer.set_duration(gst::ClockTime::SECOND);
        }
        assert_eq!(h2.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i == 2 {
            let ev = loop {
                let ev = h1.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(5.seconds()),
                    all_headers: true,
                    count: 0
                }
            );

            let ev = loop {
                let ev = h2.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(5.seconds()),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Crank the clock: this should bring us to the end of the first fragment
    h1.crank_single_clock_wait().unwrap();

    let header = h1.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO + output_offset));
    assert_eq!(header.dts(), Some(gst::ClockTime::ZERO + output_offset));

    let fragment_header = h1.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    assert_eq!(fragment_header.duration(), Some(5.seconds()));

    for i in 0..5 {
        for j in 0..2 {
            let buffer = h1.pull().unwrap();
            if i == 4 && j == 1 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }

            assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));

            if j == 0 {
                assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
            } else {
                assert!(buffer.dts().is_none());
            }
            assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
        }
    }

    h1.push_event(gst::event::Eos::new());
    h2.push_event(gst::event::Eos::new());

    let fragment_header = h1.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(5.seconds() + output_offset));
    assert_eq!(fragment_header.dts(), Some(5.seconds() + output_offset));
    assert_eq!(fragment_header.duration(), Some(2.seconds()));

    for i in 5..7 {
        for j in 0..2 {
            let buffer = h1.pull().unwrap();
            if i == 6 && j == 1 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }
            assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));
            if j == 0 {
                assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
            } else {
                assert!(buffer.dts().is_none());
            }
            assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
        }
    }

    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_live_timeout() {
    init();

    let mut h1 = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));
    let mut h2 = gst_check::Harness::with_element(&h1.element().unwrap(), Some("sink_1"), None);

    h1.use_testclock();

    // 5s fragment duration
    h1.element()
        .unwrap()
        .set_property("fragment-duration", 5.seconds());

    h1.set_src_caps(
        gst::Caps::builder("video/x-h264")
            .field("width", 1920i32)
            .field("height", 1080i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .field("stream-format", "avc")
            .field("alignment", "au")
            .field("codec_data", gst::Buffer::with_size(1).unwrap())
            .build(),
    );
    h1.play();

    h2.set_src_caps(
        gst::Caps::builder("audio/mpeg")
            .field("mpegversion", 4i32)
            .field("channels", 1i32)
            .field("rate", 44100i32)
            .field("stream-format", "raw")
            .field("base-profile", "lc")
            .field("profile", "lc")
            .field("level", "2")
            .field(
                "codec_data",
                gst::Buffer::from_slice([0x12, 0x08, 0x56, 0xe5, 0x00]),
            )
            .build(),
    );
    h2.play();

    let output_offset = (60 * 60 * 1000).seconds();

    // Push 7 buffers of 1s each, 1st and last buffer without DELTA_UNIT flag
    for i in 0..7 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i.seconds());
            buffer.set_dts(i.seconds());
            buffer.set_duration(gst::ClockTime::SECOND);
            if i != 0 && i != 5 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h1.push(buffer), Ok(gst::FlowSuccess::Ok));

        // Skip buffer 4th and 6th buffer (end of fragment / stream)
        if i == 4 || i == 6 {
            continue;
        } else {
            let mut buffer = gst::Buffer::with_size(1).unwrap();
            {
                let buffer = buffer.get_mut().unwrap();
                buffer.set_pts(i.seconds());
                buffer.set_dts(i.seconds());
                buffer.set_duration(gst::ClockTime::SECOND);
            }
            assert_eq!(h2.push(buffer), Ok(gst::FlowSuccess::Ok));
        }

        if i == 2 {
            let ev = loop {
                let ev = h1.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(5.seconds()),
                    all_headers: true,
                    count: 0
                }
            );

            let ev = loop {
                let ev = h2.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(5.seconds()),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Crank the clock: this should bring us to the end of the first fragment
    h1.crank_single_clock_wait().unwrap();

    let header = h1.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO + output_offset));
    assert_eq!(header.dts(), Some(gst::ClockTime::ZERO + output_offset));

    let fragment_header = h1.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    assert_eq!(fragment_header.duration(), Some(5.seconds()));

    for i in 0..5 {
        for j in 0..2 {
            // Skip gap events that don't result in buffers
            if j == 1 && i == 4 {
                // Advance time and crank the clock another time. This brings us at the end of the
                // EOS.
                h1.crank_single_clock_wait().unwrap();
                continue;
            }

            let buffer = h1.pull().unwrap();
            if i == 4 && j == 0 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else if i == 5 && j == 0 {
                assert_eq!(buffer.flags(), gst::BufferFlags::HEADER);
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }

            assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));

            if j == 0 {
                assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
            } else {
                assert!(buffer.dts().is_none());
            }
            assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
        }
    }

    h1.push_event(gst::event::Eos::new());
    h2.push_event(gst::event::Eos::new());

    let fragment_header = h1.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(5.seconds() + output_offset));
    assert_eq!(fragment_header.dts(), Some(5.seconds() + output_offset));
    assert_eq!(fragment_header.duration(), Some(2.seconds()));

    for i in 5..7 {
        for j in 0..2 {
            // Skip gap events that don't result in buffers
            if j == 1 && i == 6 {
                continue;
            }

            let buffer = h1.pull().unwrap();
            if i == 6 && j == 0 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }
            assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));
            if j == 0 {
                assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
            } else {
                assert!(buffer.dts().is_none());
            }
            assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
        }
    }

    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_gap_events() {
    init();

    let mut h1 = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));
    let mut h2 = gst_check::Harness::with_element(&h1.element().unwrap(), Some("sink_1"), None);

    h1.use_testclock();

    // 5s fragment duration
    h1.element()
        .unwrap()
        .set_property("fragment-duration", 5.seconds());

    h1.set_src_caps(
        gst::Caps::builder("video/x-h264")
            .field("width", 1920i32)
            .field("height", 1080i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .field("stream-format", "avc")
            .field("alignment", "au")
            .field("codec_data", gst::Buffer::with_size(1).unwrap())
            .build(),
    );
    h1.play();

    h2.set_src_caps(
        gst::Caps::builder("audio/mpeg")
            .field("mpegversion", 4i32)
            .field("channels", 1i32)
            .field("rate", 44100i32)
            .field("stream-format", "raw")
            .field("base-profile", "lc")
            .field("profile", "lc")
            .field("level", "2")
            .field(
                "codec_data",
                gst::Buffer::from_slice([0x12, 0x08, 0x56, 0xe5, 0x00]),
            )
            .build(),
    );
    h2.play();

    let output_offset = (60 * 60 * 1000).seconds();

    // Push 7 buffers of 1s each, 1st and last buffer without DELTA_UNIT flag
    for i in 0..7 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i.seconds());
            buffer.set_dts(i.seconds());
            buffer.set_duration(gst::ClockTime::SECOND);
            if i != 0 && i != 5 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h1.push(buffer), Ok(gst::FlowSuccess::Ok));

        // Replace buffer 3 and 6 with a gap event
        if i == 3 || i == 6 {
            let ev = gst::event::Gap::builder(i.seconds())
                .duration(gst::ClockTime::SECOND)
                .build();
            assert!(h2.push_event(ev));
        } else {
            let mut buffer = gst::Buffer::with_size(1).unwrap();
            {
                let buffer = buffer.get_mut().unwrap();
                buffer.set_pts(i.seconds());
                buffer.set_dts(i.seconds());
                buffer.set_duration(gst::ClockTime::SECOND);
            }
            assert_eq!(h2.push(buffer), Ok(gst::FlowSuccess::Ok));
        }

        if i == 2 {
            let ev = loop {
                let ev = h1.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(5.seconds()),
                    all_headers: true,
                    count: 0
                }
            );

            let ev = loop {
                let ev = h2.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(5.seconds()),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Crank the clock: this should bring us to the end of the first fragment
    h1.crank_single_clock_wait().unwrap();

    let header = h1.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO + output_offset));
    assert_eq!(header.dts(), Some(gst::ClockTime::ZERO + output_offset));

    let fragment_header = h1.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    assert_eq!(fragment_header.duration(), Some(5.seconds()));

    for i in 0..5 {
        for j in 0..2 {
            // Skip gap events that don't result in buffers
            if j == 1 && i == 3 {
                continue;
            }

            let buffer = h1.pull().unwrap();
            if i == 4 && j == 1 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }

            assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));

            if j == 0 {
                assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
            } else {
                assert!(buffer.dts().is_none());
            }
            assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
        }
    }

    h1.push_event(gst::event::Eos::new());
    h2.push_event(gst::event::Eos::new());

    let fragment_header = h1.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(5.seconds() + output_offset));
    assert_eq!(fragment_header.dts(), Some(5.seconds() + output_offset));
    assert_eq!(fragment_header.duration(), Some(2.seconds()));

    for i in 5..7 {
        for j in 0..2 {
            // Skip gap events that don't result in buffers
            if j == 1 && i == 6 {
                continue;
            }

            let buffer = h1.pull().unwrap();
            if i == 6 && j == 0 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }
            assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));
            if j == 0 {
                assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
            } else {
                assert!(buffer.dts().is_none());
            }
            assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
        }
    }

    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_single_stream_short_gops() {
    init();

    let mut h = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));

    // 5s fragment duration
    h.element()
        .unwrap()
        .set_property("fragment-duration", 5.seconds());

    h.set_src_caps(
        gst::Caps::builder("video/x-h264")
            .field("width", 1920i32)
            .field("height", 1080i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .field("stream-format", "avc")
            .field("alignment", "au")
            .field("codec_data", gst::Buffer::with_size(1).unwrap())
            .build(),
    );
    h.play();

    let output_offset = (60 * 60 * 1000).seconds();

    // Push 8 buffers of 1s each, 1st, 4th and 7th buffer without DELTA_UNIT flag
    for i in 0..8 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i.seconds());
            buffer.set_dts(i.seconds());
            buffer.set_duration(gst::ClockTime::SECOND);
            if i != 0 && i != 3 && i != 6 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i == 2 || i == 7 {
            let ev = loop {
                let ev = h.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            let fku_time = if i == 2 { 5.seconds() } else { 8.seconds() };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(fku_time),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    let header = h.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO + output_offset));
    assert_eq!(header.dts(), Some(gst::ClockTime::ZERO + output_offset));

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    assert_eq!(fragment_header.duration(), Some(3.seconds()));

    for i in 0..3 {
        let buffer = h.pull().unwrap();
        if i == 2 {
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
        } else {
            assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
        }
        assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));
        assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
        assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
    }

    h.push_event(gst::event::Eos::new());

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(3.seconds() + output_offset));
    assert_eq!(fragment_header.dts(), Some(3.seconds() + output_offset));
    assert_eq!(fragment_header.duration(), Some(5.seconds()));

    for i in 3..8 {
        let buffer = h.pull().unwrap();
        if i == 7 {
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
        } else {
            assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
        }
        assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));
        assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
        assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
    }

    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_single_stream_long_gops() {
    init();

    let mut h = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));

    // 5s fragment duration
    h.element()
        .unwrap()
        .set_property("fragment-duration", 5.seconds());

    h.set_src_caps(
        gst::Caps::builder("video/x-h264")
            .field("width", 1920i32)
            .field("height", 1080i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .field("stream-format", "avc")
            .field("alignment", "au")
            .field("codec_data", gst::Buffer::with_size(1).unwrap())
            .build(),
    );
    h.play();

    let output_offset = (60 * 60 * 1000).seconds();

    // Push 10 buffers of 1s each, 1st and 7th buffer without DELTA_UNIT flag
    for i in 0..10 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i.seconds());
            buffer.set_dts(i.seconds());
            buffer.set_duration(gst::ClockTime::SECOND);
            if i != 0 && i != 6 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i == 2 || i == 7 {
            let ev = loop {
                let ev = h.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            let fku_time = if i == 2 { 5.seconds() } else { 11.seconds() };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(fku_time),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Crank the clock: this should bring us to the end of the first fragment
    h.crank_single_clock_wait().unwrap();

    let header = h.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO + output_offset));
    assert_eq!(header.dts(), Some(gst::ClockTime::ZERO + output_offset));

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    assert_eq!(fragment_header.duration(), Some(6.seconds()));

    for i in 0..6 {
        let buffer = h.pull().unwrap();
        if i == 5 {
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
        } else {
            assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
        }
        assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));
        assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
        assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
    }

    h.push_event(gst::event::Eos::new());

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(6.seconds() + output_offset));
    assert_eq!(fragment_header.dts(), Some(6.seconds() + output_offset));
    assert_eq!(fragment_header.duration(), Some(4.seconds()));

    for i in 6..10 {
        let buffer = h.pull().unwrap();
        if i == 9 {
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
        } else {
            assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
        }
        assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));
        assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
        assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
    }

    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_buffer_multi_stream_short_gops() {
    init();

    let mut h1 = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));
    let mut h2 = gst_check::Harness::with_element(&h1.element().unwrap(), Some("sink_1"), None);

    // 5s fragment duration
    h1.element()
        .unwrap()
        .set_property("fragment-duration", 5.seconds());

    h1.set_src_caps(
        gst::Caps::builder("video/x-h264")
            .field("width", 1920i32)
            .field("height", 1080i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .field("stream-format", "avc")
            .field("alignment", "au")
            .field("codec_data", gst::Buffer::with_size(1).unwrap())
            .build(),
    );
    h1.play();

    h2.set_src_caps(
        gst::Caps::builder("audio/mpeg")
            .field("mpegversion", 4i32)
            .field("channels", 1i32)
            .field("rate", 44100i32)
            .field("stream-format", "raw")
            .field("base-profile", "lc")
            .field("profile", "lc")
            .field("level", "2")
            .field(
                "codec_data",
                gst::Buffer::from_slice([0x12, 0x08, 0x56, 0xe5, 0x00]),
            )
            .build(),
    );
    h2.play();

    let output_offset = (60 * 60 * 1000).seconds();

    // Push 9 buffers of 1s each, 1st, 4th and 7th buffer without DELTA_UNIT flag
    for i in 0..9 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i.seconds());
            buffer.set_dts(i.seconds());
            buffer.set_duration(gst::ClockTime::SECOND);
            if i != 0 && i != 3 && i != 6 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h1.push(buffer), Ok(gst::FlowSuccess::Ok));

        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i.seconds());
            buffer.set_dts(i.seconds());
            buffer.set_duration(gst::ClockTime::SECOND);
        }
        assert_eq!(h2.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i == 2 || i == 8 {
            let ev = loop {
                let ev = h1.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            let fku_time = if i == 2 { 5.seconds() } else { 8.seconds() };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(fku_time),
                    all_headers: true,
                    count: 0
                }
            );

            let ev = loop {
                let ev = h2.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(fku_time),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    let header = h1.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO + output_offset));
    assert_eq!(header.dts(), Some(gst::ClockTime::ZERO + output_offset));

    let fragment_header = h1.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::ZERO + output_offset)
    );
    assert_eq!(fragment_header.duration(), Some(3.seconds()));

    for i in 0..3 {
        for j in 0..2 {
            let buffer = h1.pull().unwrap();
            if i == 2 && j == 1 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }

            assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));

            if j == 0 {
                assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
            } else {
                assert!(buffer.dts().is_none());
            }
            assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
        }
    }

    h1.push_event(gst::event::Eos::new());
    h2.push_event(gst::event::Eos::new());

    let fragment_header = h1.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(3.seconds() + output_offset));
    assert_eq!(fragment_header.dts(), Some(3.seconds() + output_offset));
    assert_eq!(fragment_header.duration(), Some(6.seconds()));

    for i in 3..9 {
        for j in 0..2 {
            let buffer = h1.pull().unwrap();
            if i == 8 && j == 1 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }
            assert_eq!(buffer.pts(), Some(i.seconds() + output_offset));
            if j == 0 {
                assert_eq!(buffer.dts(), Some(i.seconds() + output_offset));
            } else {
                assert!(buffer.dts().is_none());
            }
            assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
        }
    }

    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_single_stream_manual_fragment() {
    init();

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::with_size(1).unwrap())
        .build();

    let mut h = gst_check::Harness::new("cmafmux");

    // fragment duration long enough to be ignored, 1s chunk duration
    h.element()
        .unwrap()
        .set_property("fragment-duration", 1.hours());

    h.set_src_caps(caps);
    h.play();

    // request fragment at 4 seconds, should be created at 11th buffer
    h.element()
        .unwrap()
        .emit_by_name::<()>("split-at-running-time", &[&4.seconds()]);

    // Push 15 buffers of 0.5s each, 1st, 11th and 16th buffer without DELTA_UNIT flag
    for i in 0..20 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 500.mseconds());
            buffer.set_dts(i * 500.mseconds());
            buffer.set_duration(500.mseconds());
            if i != 0 && i != 10 && i != 15 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i == 2 {
            let ev = loop {
                let ev = h.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(4.seconds()),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Crank the clock: this should bring us to the end of the first fragment
    h.crank_single_clock_wait().unwrap();

    let header = h.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(header.dts(), Some(gst::ClockTime::ZERO));

    // first fragment
    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(fragment_header.dts(), Some(gst::ClockTime::ZERO));
    assert_eq!(fragment_header.duration(), Some(5.seconds()));

    for buffer_idx in 0..10 {
        let buffer = h.pull().unwrap();
        if buffer_idx == 9 {
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
        } else {
            assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
        }
        assert_eq!(buffer.pts(), Some(buffer_idx * 500.mseconds()));
        assert_eq!(buffer.dts(), Some(buffer_idx * 500.mseconds()));
        assert_eq!(buffer.duration(), Some(500.mseconds()));
    }

    // second manual fragment
    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(5.seconds()));
    assert_eq!(fragment_header.dts(), Some(5.seconds()));
    assert_eq!(fragment_header.duration(), Some(2500.mseconds()));

    for buffer_idx in 0..5 {
        let buffer = h.pull().unwrap();
        if buffer_idx == 4 {
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
        } else {
            assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
        }
        assert_eq!(
            buffer.pts(),
            Some(5.seconds() + buffer_idx * 500.mseconds())
        );
        assert_eq!(
            buffer.dts(),
            Some(5.seconds() + buffer_idx * 500.mseconds())
        );
        assert_eq!(buffer.duration(), Some(500.mseconds()));
    }

    h.push_event(gst::event::Eos::new());

    // There should be the second fragment now
    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(7500.mseconds()));
    assert_eq!(fragment_header.dts(), Some(7500.mseconds()));
    assert_eq!(fragment_header.duration(), Some(2500.mseconds()));

    for buffer_idx in 0..5 {
        let buffer = h.pull().unwrap();
        if buffer_idx == 4 {
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
        } else {
            assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
        }
        assert_eq!(
            buffer.pts(),
            Some(7500.mseconds() + buffer_idx * 500.mseconds())
        );
        assert_eq!(
            buffer.dts(),
            Some(7500.mseconds() + buffer_idx * 500.mseconds())
        );
        assert_eq!(buffer.duration(), Some(500.mseconds()));
    }

    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_chunking_single_stream_manual_fragment() {
    init();

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::with_size(1).unwrap())
        .build();

    let mut h = gst_check::Harness::new("cmafmux");

    // fragment duration long enough to be ignored, 1s chunk duration
    h.element()
        .unwrap()
        .set_property("fragment-duration", 1.hours());
    h.element()
        .unwrap()
        .set_property("chunk-duration", 1.seconds());

    h.set_src_caps(caps);
    h.play();

    // request fragment at 4 seconds, should be created at 11th buffer
    h.element()
        .unwrap()
        .emit_by_name::<()>("split-at-running-time", &[&4.seconds()]);

    // Push 15 buffers of 0.5s each, 1st and 11th buffer without DELTA_UNIT flag
    for i in 0..15 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 500.mseconds());
            buffer.set_dts(i * 500.mseconds());
            buffer.set_duration(500.mseconds());
            if i != 0 && i != 10 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i == 2 {
            let ev = loop {
                let ev = h.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(4.seconds()),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Crank the clock: this should bring us to the end of the first fragment
    h.crank_single_clock_wait().unwrap();

    let header = h.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(header.dts(), Some(gst::ClockTime::ZERO));

    // There should be 7 chunks now, and the 1st and 6th are starting a fragment.
    // Each chunk should have two buffers.
    for chunk in 0..7 {
        let chunk_header = h.pull().unwrap();
        if chunk == 0 || chunk == 5 {
            assert_eq!(chunk_header.flags(), gst::BufferFlags::HEADER);
        } else {
            assert_eq!(
                chunk_header.flags(),
                gst::BufferFlags::HEADER | gst::BufferFlags::DELTA_UNIT
            );
        }
        assert_eq!(chunk_header.pts(), Some(chunk * 1.seconds()));
        assert_eq!(chunk_header.dts(), Some(chunk * 1.seconds()));
        assert_eq!(chunk_header.duration(), Some(1.seconds()));

        for buffer_idx in 0..2 {
            let buffer = h.pull().unwrap();
            if buffer_idx == 1 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }
            assert_eq!(
                buffer.pts(),
                Some((chunk * 2 + buffer_idx) * 500.mseconds())
            );
            assert_eq!(
                buffer.dts(),
                Some((chunk * 2 + buffer_idx) * 500.mseconds())
            );
            assert_eq!(buffer.duration(), Some(500.mseconds()));
        }
    }

    h.push_event(gst::event::Eos::new());

    // There should be the remaining chunk now, containing one 500ms buffer.
    for chunk in 7..8 {
        let chunk_header = h.pull().unwrap();
        assert_eq!(
            chunk_header.flags(),
            gst::BufferFlags::HEADER | gst::BufferFlags::DELTA_UNIT
        );
        assert_eq!(chunk_header.pts(), Some(chunk * 1.seconds()));
        assert_eq!(chunk_header.dts(), Some(chunk * 1.seconds()));
        assert_eq!(chunk_header.duration(), Some(500.mseconds()));

        for buffer_idx in 0..1 {
            let buffer = h.pull().unwrap();
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
            assert_eq!(
                buffer.pts(),
                Some((chunk * 2 + buffer_idx) * 500.mseconds())
            );
            assert_eq!(
                buffer.dts(),
                Some((chunk * 2 + buffer_idx) * 500.mseconds())
            );
            assert_eq!(buffer.duration(), Some(500.mseconds()));
        }
    }

    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_chunking_single_stream() {
    init();

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::with_size(1).unwrap())
        .build();

    let mut h = gst_check::Harness::new("cmafmux");

    // 5s fragment duration, 1s chunk duration
    h.element()
        .unwrap()
        .set_property("fragment-duration", 5.seconds());
    h.element()
        .unwrap()
        .set_property("chunk-duration", 1.seconds());

    h.set_src_caps(caps);
    h.play();

    // Push 15 buffers of 0.5s each, 1st and 11th buffer without DELTA_UNIT flag
    for i in 0..15 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 500.mseconds());
            buffer.set_dts(i * 500.mseconds());
            buffer.set_duration(500.mseconds());
            if i != 0 && i != 10 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i == 2 {
            let ev = loop {
                let ev = h.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(5.seconds()),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Crank the clock: this should bring us to the end of the first fragment
    h.crank_single_clock_wait().unwrap();

    let header = h.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(header.dts(), Some(gst::ClockTime::ZERO));

    // There should be 7 chunks now, and the 1st and 6th are starting a fragment.
    // Each chunk should have two buffers.
    for chunk in 0..7 {
        let chunk_header = h.pull().unwrap();
        if chunk == 0 || chunk == 5 {
            assert_eq!(chunk_header.flags(), gst::BufferFlags::HEADER);
        } else {
            assert_eq!(
                chunk_header.flags(),
                gst::BufferFlags::HEADER | gst::BufferFlags::DELTA_UNIT
            );
        }
        assert_eq!(chunk_header.pts(), Some(chunk * 1.seconds()));
        assert_eq!(chunk_header.dts(), Some(chunk * 1.seconds()));
        assert_eq!(chunk_header.duration(), Some(1.seconds()));

        for buffer_idx in 0..2 {
            let buffer = h.pull().unwrap();
            if buffer_idx == 1 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }
            assert_eq!(
                buffer.pts(),
                Some((chunk * 2 + buffer_idx) * 500.mseconds())
            );
            assert_eq!(
                buffer.dts(),
                Some((chunk * 2 + buffer_idx) * 500.mseconds())
            );
            assert_eq!(buffer.duration(), Some(500.mseconds()));
        }
    }

    h.push_event(gst::event::Eos::new());

    // There should be the remaining chunk now, containing one 500ms buffer.
    for chunk in 7..8 {
        let chunk_header = h.pull().unwrap();
        assert_eq!(
            chunk_header.flags(),
            gst::BufferFlags::HEADER | gst::BufferFlags::DELTA_UNIT
        );
        assert_eq!(chunk_header.pts(), Some(chunk * 1.seconds()));
        assert_eq!(chunk_header.dts(), Some(chunk * 1.seconds()));
        assert_eq!(chunk_header.duration(), Some(500.mseconds()));

        for buffer_idx in 0..1 {
            let buffer = h.pull().unwrap();
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
            assert_eq!(
                buffer.pts(),
                Some((chunk * 2 + buffer_idx) * 500.mseconds())
            );
            assert_eq!(
                buffer.dts(),
                Some((chunk * 2 + buffer_idx) * 500.mseconds())
            );
            assert_eq!(buffer.duration(), Some(500.mseconds()));
        }
    }

    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_chunking_multi_stream() {
    init();

    let mut h1 = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));
    let mut h2 = gst_check::Harness::with_element(&h1.element().unwrap(), Some("sink_1"), None);

    // 5s fragment duration, 1s chunk duration
    h1.element()
        .unwrap()
        .set_property("fragment-duration", 5.seconds());
    h1.element()
        .unwrap()
        .set_property("chunk-duration", 1.seconds());

    h1.set_src_caps(
        gst::Caps::builder("video/x-h264")
            .field("width", 1920i32)
            .field("height", 1080i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .field("stream-format", "avc")
            .field("alignment", "au")
            .field("codec_data", gst::Buffer::with_size(1).unwrap())
            .build(),
    );
    h1.play();

    h2.set_src_caps(
        gst::Caps::builder("audio/mpeg")
            .field("mpegversion", 4i32)
            .field("channels", 1i32)
            .field("rate", 44100i32)
            .field("stream-format", "raw")
            .field("base-profile", "lc")
            .field("profile", "lc")
            .field("level", "2")
            .field(
                "codec_data",
                gst::Buffer::from_slice([0x12, 0x08, 0x56, 0xe5, 0x00]),
            )
            .build(),
    );
    h2.play();

    let output_offset = (60 * 60 * 1000).seconds();

    // Push 15 buffers of 0.5s each, 1st and 11th buffer without DELTA_UNIT flag
    for i in 0..15 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 500.mseconds());
            buffer.set_dts(i * 500.mseconds());
            buffer.set_duration(500.mseconds());
            if i != 0 && i != 10 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h1.push(buffer), Ok(gst::FlowSuccess::Ok));

        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 500.mseconds());
            buffer.set_dts(i * 500.mseconds());
            buffer.set_duration(500.mseconds());
        }
        assert_eq!(h2.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i == 2 {
            let ev = loop {
                let ev = h1.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(5.seconds()),
                    all_headers: true,
                    count: 0
                }
            );

            let ev = loop {
                let ev = h2.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(5.seconds()),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Crank the clock: this should bring us to the end of the first fragment
    h1.crank_single_clock_wait().unwrap();

    let header = h1.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO + output_offset));
    assert_eq!(header.dts(), Some(gst::ClockTime::ZERO + output_offset));

    // There should be 7 chunks now, and the 1st and 6th are starting a fragment.
    // Each chunk should have two buffers.
    for chunk in 0..7 {
        let chunk_header = h1.pull().unwrap();
        if chunk == 0 || chunk == 5 {
            assert_eq!(chunk_header.flags(), gst::BufferFlags::HEADER);
        } else {
            assert_eq!(
                chunk_header.flags(),
                gst::BufferFlags::HEADER | gst::BufferFlags::DELTA_UNIT
            );
        }
        assert_eq!(
            chunk_header.pts(),
            Some(chunk * 1.seconds() + output_offset)
        );
        assert_eq!(
            chunk_header.dts(),
            Some(chunk * 1.seconds() + output_offset)
        );
        assert_eq!(chunk_header.duration(), Some(1.seconds()));

        for buffer_idx in 0..2 {
            for stream_idx in 0..2 {
                let buffer = h1.pull().unwrap();
                if buffer_idx == 1 && stream_idx == 1 {
                    assert_eq!(
                        buffer.flags(),
                        gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                    );
                } else {
                    assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
                }
                assert_eq!(
                    buffer.pts(),
                    Some((chunk * 2 + buffer_idx) * 500.mseconds() + output_offset)
                );

                if stream_idx == 0 {
                    assert_eq!(
                        buffer.dts(),
                        Some((chunk * 2 + buffer_idx) * 500.mseconds() + output_offset)
                    );
                } else {
                    assert!(buffer.dts().is_none());
                }
                assert_eq!(buffer.duration(), Some(500.mseconds()));
            }
        }
    }

    h1.push_event(gst::event::Eos::new());
    h2.push_event(gst::event::Eos::new());

    // There should be the remaining chunk now, containing one 500ms buffer.
    for chunk in 7..8 {
        let chunk_header = h1.pull().unwrap();
        assert_eq!(
            chunk_header.flags(),
            gst::BufferFlags::HEADER | gst::BufferFlags::DELTA_UNIT
        );
        assert_eq!(
            chunk_header.pts(),
            Some(chunk * 1.seconds() + output_offset)
        );
        assert_eq!(
            chunk_header.dts(),
            Some(chunk * 1.seconds() + output_offset)
        );
        assert_eq!(chunk_header.duration(), Some(500.mseconds()));

        for buffer_idx in 0..1 {
            for stream_idx in 0..2 {
                let buffer = h1.pull().unwrap();
                if buffer_idx == 0 && stream_idx == 1 {
                    assert_eq!(
                        buffer.flags(),
                        gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                    );
                } else {
                    assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
                }

                assert_eq!(
                    buffer.pts(),
                    Some((chunk * 2 + buffer_idx) * 500.mseconds() + output_offset)
                );
                if stream_idx == 0 {
                    assert_eq!(
                        buffer.dts(),
                        Some((chunk * 2 + buffer_idx) * 500.mseconds() + output_offset)
                    );
                } else {
                    assert!(buffer.dts().is_none());
                }
                assert_eq!(buffer.duration(), Some(500.mseconds()));
            }
        }
    }

    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h1.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_chunking_single_stream_gops_after_fragment_end_before_next_chunk_end() {
    init();

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::with_size(1).unwrap())
        .build();

    let mut h = gst_check::Harness::new("cmafmux");

    // 5s fragment duration, 1s chunk duration
    h.element()
        .unwrap()
        .set_property("fragment-duration", 5.seconds());
    h.element()
        .unwrap()
        .set_property("chunk-duration", 1.seconds());

    h.set_src_caps(caps);
    h.play();

    // Push 15 buffers of 0.5s each, 1st and 12th buffer without DELTA_UNIT flag
    for i in 0..15 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 500.mseconds());
            buffer.set_dts(i * 500.mseconds());
            buffer.set_duration(500.mseconds());
            if i != 0 && i != 11 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i == 2 {
            let ev = loop {
                let ev = h.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(5.seconds()),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Crank the clock: this should bring us to the end of the first fragment
    h.crank_single_clock_wait().unwrap();

    let mut expected_ts = gst::ClockTime::ZERO;
    let mut num_buffers = 0;

    let header = h.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(expected_ts));
    assert_eq!(header.dts(), Some(expected_ts));

    // There should be 7 chunks now, and the 1st and 7th are starting a fragment.
    // Each chunk should have two buffers except for the 6th.
    for chunk in 0..7 {
        let chunk_header = h.pull().unwrap();
        if chunk == 0 || chunk == 6 {
            assert_eq!(chunk_header.flags(), gst::BufferFlags::HEADER);
        } else {
            assert_eq!(
                chunk_header.flags(),
                gst::BufferFlags::HEADER | gst::BufferFlags::DELTA_UNIT
            );
        }
        assert_eq!(chunk_header.pts(), Some(expected_ts));
        assert_eq!(chunk_header.dts(), Some(expected_ts));
        if chunk == 5 {
            assert_eq!(chunk_header.duration(), Some(500.mseconds()));
        } else {
            assert_eq!(chunk_header.duration(), Some(1.seconds()));
        }

        for buffer_idx in 0..2 {
            let buffer = h.pull().unwrap();
            num_buffers += 1;
            if buffer_idx == 1 || (chunk == 5 && buffer_idx == 0) {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }
            assert_eq!(buffer.pts(), Some(expected_ts));
            assert_eq!(buffer.dts(), Some(expected_ts));
            assert_eq!(buffer.duration(), Some(500.mseconds()));

            expected_ts += 500.mseconds();

            // Only one buffer in this chunk
            if chunk == 5 && buffer_idx == 0 {
                break;
            }
        }
    }

    h.push_event(gst::event::Eos::new());

    // There should be one remaining chunk now, containing two 500ms buffer.
    for _chunk in 7..8 {
        let chunk_header = h.pull().unwrap();
        assert_eq!(
            chunk_header.flags(),
            gst::BufferFlags::HEADER | gst::BufferFlags::DELTA_UNIT
        );
        assert_eq!(chunk_header.pts(), Some(expected_ts));
        assert_eq!(chunk_header.dts(), Some(expected_ts));
        assert_eq!(chunk_header.duration(), Some(1.seconds()));

        for buffer_idx in 0..2 {
            let buffer = h.pull().unwrap();
            num_buffers += 1;
            if buffer_idx == 1 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }
            assert_eq!(buffer.pts(), Some(expected_ts));
            assert_eq!(buffer.dts(), Some(expected_ts));
            assert_eq!(buffer.duration(), Some(500.mseconds()));
            expected_ts += 500.mseconds();
        }
    }

    assert_eq!(num_buffers, 15);

    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_chunking_single_stream_gops_after_fragment_end_after_next_chunk_end() {
    init();

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::with_size(1).unwrap())
        .build();

    let mut h = gst_check::Harness::new("cmafmux");

    // 5s fragment duration, 1s chunk duration
    h.element()
        .unwrap()
        .set_property("fragment-duration", 5.seconds());
    h.element()
        .unwrap()
        .set_property("chunk-duration", 1.seconds());

    h.set_src_caps(caps);
    h.play();

    // Push 15 buffers of 0.5s each, 1st and 14th buffer without DELTA_UNIT flag
    for i in 0..15 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 500.mseconds());
            buffer.set_dts(i * 500.mseconds());
            buffer.set_duration(500.mseconds());
            if i != 0 && i != 13 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i == 2 {
            let ev = loop {
                let ev = h.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
            assert_eq!(
                gst_video::UpstreamForceKeyUnitEvent::parse(&ev).unwrap(),
                gst_video::UpstreamForceKeyUnitEvent {
                    running_time: Some(5.seconds()),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Crank the clock: this should bring us to the end of the first fragment
    h.crank_single_clock_wait().unwrap();

    let mut expected_ts = gst::ClockTime::ZERO;
    let mut num_buffers = 0;

    let header = h.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(expected_ts));
    assert_eq!(header.dts(), Some(expected_ts));

    // There should be 7 chunks now, and the 1st is starting a fragment.
    // Each chunk should have two buffers except for the 7th.
    for chunk in 0..7 {
        let chunk_header = h.pull().unwrap();
        if chunk == 0 {
            assert_eq!(chunk_header.flags(), gst::BufferFlags::HEADER);
        } else {
            assert_eq!(
                chunk_header.flags(),
                gst::BufferFlags::HEADER | gst::BufferFlags::DELTA_UNIT
            );
        }
        assert_eq!(chunk_header.pts(), Some(expected_ts));
        assert_eq!(chunk_header.dts(), Some(expected_ts));
        if chunk == 6 {
            assert_eq!(chunk_header.duration(), Some(500.mseconds()));
        } else {
            assert_eq!(chunk_header.duration(), Some(1.seconds()));
        }

        for buffer_idx in 0..2 {
            let buffer = h.pull().unwrap();
            num_buffers += 1;
            if buffer_idx == 1 || (chunk == 6 && buffer_idx == 0) {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }
            assert_eq!(buffer.pts(), Some(expected_ts));
            assert_eq!(buffer.dts(), Some(expected_ts));
            assert_eq!(buffer.duration(), Some(500.mseconds()));

            expected_ts += 500.mseconds();

            // Only one buffer in this chunk
            if chunk == 6 && buffer_idx == 0 {
                break;
            }
        }
    }

    h.push_event(gst::event::Eos::new());

    // There should be two remaining chunks now, containing two 500ms buffers.
    // This should start a new fragment.
    for _chunk in 7..8 {
        let chunk_header = h.pull().unwrap();
        assert_eq!(chunk_header.flags(), gst::BufferFlags::HEADER);
        assert_eq!(chunk_header.pts(), Some(expected_ts));
        assert_eq!(chunk_header.dts(), Some(expected_ts));
        assert_eq!(chunk_header.duration(), Some(1.seconds()));

        for buffer_idx in 0..2 {
            let buffer = h.pull().unwrap();
            num_buffers += 1;
            if buffer_idx == 1 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }
            assert_eq!(buffer.pts(), Some(expected_ts));
            assert_eq!(buffer.dts(), Some(expected_ts));
            assert_eq!(buffer.duration(), Some(500.mseconds()));
            expected_ts += 500.mseconds();
        }
    }

    assert_eq!(num_buffers, 15);

    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}

#[test]
fn test_early_eos() {
    init();

    let mut h = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::with_size(1).unwrap())
        .build();

    h.element()
        .unwrap()
        .set_property("fragment-duration", 1.seconds());

    h.set_src_caps(caps);
    h.play();

    for i in 0..5 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 100.mseconds());
            buffer.set_dts(i * 100.mseconds());
            buffer.set_duration(100.mseconds());

            buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
        }
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    }

    h.push_event(gst::event::Eos::new());
    assert_eq!(h.buffers_in_queue(), 0);
}

#[test]
fn test_roundtrip_vp9_flac() {
    init();

    let pipeline = gst::parse::launch(
        r#"
        videotestsrc num-buffers=99 ! vp9enc ! vp9parse ! mux.
        audiotestsrc num-buffers=149 ! flacenc ! flacparse ! mux.
        isofmp4mux name=mux ! qtdemux name=demux
        demux.audio_0 ! queue ! flacdec ! fakesink
        demux.video_0 ! queue ! vp9dec ! fakesink
        "#,
    )
    .unwrap();
    let pipeline = pipeline.downcast().unwrap();
    to_completion(&pipeline);
}

#[track_caller]
fn test_caps_changed_verify(
    h: &mut gst_check::Harness,
    num_bufs: usize,
    caps_changed: bool,
    chunk: bool,
) {
    for i in 0..num_bufs {
        let b = h.pull().unwrap();
        match (caps_changed, i, chunk) {
            (true, 0, _) => assert_eq!(
                b.flags(),
                gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
            ),
            (false, 0, false) | (true, 1, false) => assert_eq!(b.flags(), gst::BufferFlags::HEADER),
            (false, 0, true) | (true, 1, true) => assert_eq!(
                b.flags(),
                gst::BufferFlags::HEADER | gst::BufferFlags::DELTA_UNIT
            ),
            (false, 1, _) | (_, 2.., _) => {
                if i == num_bufs - 1 {
                    assert_eq!(
                        b.flags(),
                        gst::BufferFlags::MARKER | gst::BufferFlags::DELTA_UNIT
                    );
                } else {
                    assert_eq!(b.flags(), gst::BufferFlags::DELTA_UNIT);
                }
            }
        }
    }
}

#[track_caller]
fn test_caps_changed_buffers(
    h: &mut gst_check::Harness,
    num_bufs: u64,
    gop_size: u64,
    caps_change: u64,
    duration: u64,
    key_frame_on_caps_change: bool,
    drop_first_buffer: bool,
) {
    for i in 0..num_bufs {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * duration.mseconds());
            buffer.set_dts(i * duration.mseconds());
            buffer.set_duration(duration.mseconds());

            if i % gop_size != 0 && (i != caps_change || !key_frame_on_caps_change) {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }

        if i == 0 && drop_first_buffer {
            continue;
        }

        if i == caps_change {
            let caps = gst::Caps::builder("video/x-h264")
                .field("width", 1280i32)
                .field("height", 720i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .field("stream-format", "avc")
                .field("alignment", "au")
                .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
                .build();
            h.push_event(gst::event::Caps::new(&caps));
        }
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    }
}

#[test]
fn test_caps_change_at_gop_boundary() {
    init();

    let mut h = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));

    h.element()
        .unwrap()
        .set_property_from_str("header-update-mode", "caps");

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
        .build();

    h.element()
        .unwrap()
        .set_property("fragment-duration", 1.seconds());

    h.set_src_caps(caps);
    h.play();

    test_caps_changed_buffers(&mut h, 30, 10, 10, 100, true, false);

    h.crank_single_clock_wait().unwrap();
    // Initial fragment with HEADER and DISCONT
    test_caps_changed_verify(&mut h, 1 + 1 + 10, true, false);

    h.crank_single_clock_wait().unwrap();
    // Full GOP with HEADER and DISCONT due to caps change
    test_caps_changed_verify(&mut h, 1 + 1 + 10, true, false);

    h.crank_single_clock_wait().unwrap();
    h.push_event(gst::event::Eos::new());
    // Full GOP with HEADER but no DISCONT because no caps change
    test_caps_changed_verify(&mut h, 1 + 10, false, false);

    assert_eq!(h.buffers_in_queue(), 0);
}

#[test]
fn test_language_change_at_gop_boundary() {
    init();

    let mut h = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));

    h.element()
        .unwrap()
        .set_property_from_str("header-update-mode", "caps");

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
        .build();

    h.element()
        .unwrap()
        .set_property("fragment-duration", 1.seconds());

    h.set_src_caps(caps);
    h.play();

    for i in 0..30 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            if i == 10 {
                let mut tl = gst::TagList::new();
                tl.make_mut()
                    .add::<gst::tags::LanguageCode>(&"eng", gst::TagMergeMode::Append);
                let ev = gst::event::Tag::builder(tl).build();
                h.push_event(ev);
            }

            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 100.mseconds());
            buffer.set_dts(i * 100.mseconds());
            buffer.set_duration(100.mseconds());

            if i % 10 != 0 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    }

    h.crank_single_clock_wait().unwrap();
    // Initial fragment with HEADER and DISCONT
    test_caps_changed_verify(&mut h, 1 + 1 + 10, true, false);

    h.crank_single_clock_wait().unwrap();
    // Full GOP with HEADER due to language change
    test_caps_changed_verify(&mut h, 1 + 1 + 10, true, false);

    h.crank_single_clock_wait().unwrap();
    h.push_event(gst::event::Eos::new());
    // Full GOP with HEADER but no DISCONT because no caps change
    test_caps_changed_verify(&mut h, 1 + 10, false, false);

    assert_eq!(h.buffers_in_queue(), 0);
}

#[test]
fn test_caps_change_at_gop_boundary_multi_stream() {
    init();

    let mut h1 = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));
    let mut h2 = gst_check::Harness::with_element(&h1.element().unwrap(), Some("sink_1"), None);

    h1.element()
        .unwrap()
        .set_property_from_str("header-update-mode", "caps");

    let caps1 = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
        .build();

    let caps2 = gst::Caps::builder("video/x-h264")
        .field("width", 640)
        .field("height", 480)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([4, 3, 2, 1]))
        .build();

    h1.element()
        .unwrap()
        .set_property("fragment-duration", 330.mseconds());

    h1.set_src_caps(caps1);
    h1.play();
    h2.set_src_caps(caps2);
    h2.play();

    for i in 0..21 {
        // caps change on 5th and 20th buffer
        if let Some(caps) = match i {
            5 => Some(
                gst::Caps::builder("video/x-h264")
                    .field("width", 1280i32)
                    .field("height", 720i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .field("stream-format", "avc")
                    .field("alignment", "au")
                    .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
                    .build(),
            ),
            20 => Some(
                gst::Caps::builder("video/x-h264")
                    .field("width", 1024)
                    .field("height", 800)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .field("stream-format", "avc")
                    .field("alignment", "au")
                    .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
                    .build(),
            ),
            _ => None,
        } {
            h1.push_event(gst::event::Caps::new(&caps));
        }

        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 33.mseconds());
            buffer.set_dts(i * 33.mseconds());
            buffer.set_duration(33.mseconds());

            // GOP size of 5
            if i % 5 != 0 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h1.push(buffer), Ok(gst::FlowSuccess::Ok));

        let mut buffer = gst::Buffer::with_size(2).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 33.mseconds());
            buffer.set_dts(i * 33.mseconds());
            buffer.set_duration(33.mseconds());

            // GOP size of 7
            if i % 7 != 0 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h2.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i != 5 {
            continue;
        }

        // reconfigure event
        h1.try_pull_upstream_event().unwrap();
        h2.try_pull_upstream_event().unwrap();
        let ev1 = h1.pull_upstream_event().unwrap();
        assert_eq!(
            gst_video::UpstreamForceKeyUnitEvent::parse(&ev1).unwrap(),
            gst_video::UpstreamForceKeyUnitEvent {
                running_time: Some(330.mseconds()),
                all_headers: true,
                count: 0
            }
        );
        let ev2 = h2.pull_upstream_event().unwrap();
        assert_eq!(
            gst_video::UpstreamForceKeyUnitEvent::parse(&ev1).unwrap(),
            gst_video::UpstreamForceKeyUnitEvent::parse(&ev2).unwrap()
        );
        let ev1 = h1.pull_upstream_event().unwrap();
        assert_eq!(
            gst_video::UpstreamForceKeyUnitEvent::parse(&ev1).unwrap(),
            gst_video::UpstreamForceKeyUnitEvent {
                running_time: Some(495.mseconds()),
                all_headers: true,
                count: 0
            }
        );
        let ev2 = h2.pull_upstream_event().unwrap();
        assert_eq!(
            gst_video::UpstreamForceKeyUnitEvent::parse(&ev1).unwrap(),
            gst_video::UpstreamForceKeyUnitEvent::parse(&ev2).unwrap()
        );
        let ev1 = h1.pull_upstream_event().unwrap();
        assert_eq!(
            gst_video::UpstreamForceKeyUnitEvent::parse(&ev1).unwrap(),
            gst_video::UpstreamForceKeyUnitEvent {
                running_time: Some(165.mseconds()),
                all_headers: true,
                count: 0
            }
        );
        let ev2 = h2.pull_upstream_event().unwrap();
        assert_eq!(
            gst_video::UpstreamForceKeyUnitEvent::parse(&ev1).unwrap(),
            gst_video::UpstreamForceKeyUnitEvent::parse(&ev2).unwrap()
        );
    }

    h1.crank_single_clock_wait().unwrap();
    // Caps change on 6th buffer, 1st stream has complete GOP but 2nd
    // stream GOP is incomplete. Still 2nd stream buffers are pushed
    // because the stream would be missing in the fragment otherwise.
    test_caps_changed_verify(&mut h1, 1 + 1 + 5 + 4, true, false);

    h1.crank_single_clock_wait().unwrap();
    // Caps change on 20th buffer, both streams have complete GOPs, 3
    // from stream #1 and the rest of the old GOP and one complete GOP
    // from stream #2.
    test_caps_changed_verify(&mut h1, 1 + 1 + 5 + 5 + 5 + 3 + 7, true, false);

    // On stream #1 a FKU event for partial GOP and next GOP and on
    // stream #2 just the next GOP FKU because no partial GOP has been
    // pushed.
    assert_eq!(h1.upstream_events_in_queue(), 2);
    assert_eq!(h2.upstream_events_in_queue(), 1);

    h1.crank_single_clock_wait().unwrap();
    h1.push_event(gst::event::Eos::new());
    h2.push_event(gst::event::Eos::new());
    test_caps_changed_verify(&mut h1, 1 + 1 + 8, true, false);

    assert_eq!(h1.buffers_in_queue(), 0);
}

#[test]
fn test_caps_change_at_gop_boundary_chunked_multi_stream() {
    init();

    let mut h1 = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));
    let mut h2 = gst_check::Harness::with_element(&h1.element().unwrap(), Some("sink_1"), None);

    h1.element()
        .unwrap()
        .set_property_from_str("header-update-mode", "caps");

    let caps1 = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
        .build();

    let caps2 = gst::Caps::builder("video/x-h264")
        .field("width", 640)
        .field("height", 480)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([4, 3, 2, 1]))
        .build();

    h1.element()
        .unwrap()
        .set_property("fragment-duration", 1.seconds());
    h1.element()
        .unwrap()
        .set_property("chunk-duration", 250.mseconds());

    h1.set_src_caps(caps1);
    h1.play();
    h2.set_src_caps(caps2);
    h2.play();

    for i in 0..19 {
        // caps change on 10th and 20th buffer
        if let Some(caps) = match i {
            10 => Some(
                gst::Caps::builder("video/x-h264")
                    .field("width", 1280i32)
                    .field("height", 720i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .field("stream-format", "avc")
                    .field("alignment", "au")
                    .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
                    .build(),
            ),
            _ => None,
        } {
            h1.push_event(gst::event::Caps::new(&caps));
        }

        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 33.mseconds());
            buffer.set_dts(i * 33.mseconds());
            buffer.set_duration(33.mseconds());

            // GOP size of 5
            if i % 5 != 0 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h1.push(buffer), Ok(gst::FlowSuccess::Ok));

        let mut buffer = gst::Buffer::with_size(2).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i * 33.mseconds());
            buffer.set_dts(i * 33.mseconds());
            buffer.set_duration(33.mseconds());

            // GOP size of 7
            if i % 7 != 0 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h2.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i != 5 {
            continue;
        }

        // reconfigure event
        h1.try_pull_upstream_event().unwrap();
        h2.try_pull_upstream_event().unwrap();
        let ev1 = h1.pull_upstream_event().unwrap();
        assert_eq!(
            gst_video::UpstreamForceKeyUnitEvent::parse(&ev1).unwrap(),
            gst_video::UpstreamForceKeyUnitEvent {
                running_time: Some(1.seconds()),
                all_headers: true,
                count: 0
            }
        );
        let ev2 = h2.pull_upstream_event().unwrap();
        assert_eq!(
            gst_video::UpstreamForceKeyUnitEvent::parse(&ev1).unwrap(),
            gst_video::UpstreamForceKeyUnitEvent::parse(&ev2).unwrap()
        );
    }

    h1.crank_single_clock_wait().unwrap();
    // Fragment start chunk
    test_caps_changed_verify(&mut h1, 1 + 1 + 8 + 8, true, false);

    h1.crank_single_clock_wait().unwrap();
    // Early end of chunk due to caps change
    test_caps_changed_verify(&mut h1, 1 + 2 + 1, false, true);

    // Signalling new keyunit for next fragment
    let ev1 = h1.pull_upstream_event().unwrap();
    assert_eq!(
        gst_video::UpstreamForceKeyUnitEvent::parse(&ev1).unwrap(),
        gst_video::UpstreamForceKeyUnitEvent {
            running_time: Some(1330.mseconds()),
            all_headers: true,
            count: 0
        }
    );

    // Signalling new keyunit on stream with caps change
    let ev1 = h1.pull_upstream_event().unwrap();
    assert_eq!(
        gst_video::UpstreamForceKeyUnitEvent::parse(&ev1).unwrap(),
        gst_video::UpstreamForceKeyUnitEvent {
            running_time: Some(330.mseconds()),
            all_headers: true,
            count: 0
        }
    );

    h1.crank_single_clock_wait().unwrap();
    // The first chunk of the new fragment
    test_caps_changed_verify(&mut h1, 1 + 1 + 8 + 9, true, false);

    h1.crank_single_clock_wait().unwrap();
    h1.push_event(gst::event::Eos::new());
    h2.push_event(gst::event::Eos::new());
    // The final chunk from EOS
    test_caps_changed_verify(&mut h1, 1 + 1 + 1, false, true);

    assert_eq!(h1.buffers_in_queue(), 0);
}

#[test]
fn test_caps_change_at_gop_boundary_compatible() {
    init();

    let mut h = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));

    h.element()
        .unwrap()
        .set_property_from_str("header-update-mode", "caps");

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1280i32)
        .field("height", 720i32)
        .field("framerate", gst::Fraction::new(10, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
        .build();

    h.element()
        .unwrap()
        .set_property("fragment-duration", 1.seconds());

    h.set_src_caps(caps);
    h.play();

    test_caps_changed_buffers(&mut h, 30, 10, 10, 100, true, false);

    h.crank_single_clock_wait().unwrap();
    // Initial fragment with HEADER and DISCONT
    test_caps_changed_verify(&mut h, 1 + 1 + 10, true, false);

    h.crank_single_clock_wait().unwrap();
    // Full GOP with HEADER but no DISCONT because compatible caps
    // change
    test_caps_changed_verify(&mut h, 1 + 10, false, false);

    h.crank_single_clock_wait().unwrap();
    h.push_event(gst::event::Eos::new());
    // Full GOP with HEADER but no DISCONT because no caps change
    test_caps_changed_verify(&mut h, 1 + 10, false, false);

    assert_eq!(h.buffers_in_queue(), 0);
}

#[test]
fn test_caps_change_at_gop_boundary_not_allowed() {
    init();

    let mut h = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));

    h.element()
        .unwrap()
        .set_property_from_str("header-update-mode", "caps");

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
        .build();

    h.element()
        .unwrap()
        .set_property("fragment-duration", 1.seconds());

    h.element()
        .unwrap()
        .set_property_from_str("header-update-mode", "rewrite");

    h.set_src_caps(caps);
    h.play();

    test_caps_changed_buffers(&mut h, 30, 10, 10, 100, true, false);

    h.crank_single_clock_wait().unwrap();
    // Initial fragment with HEADER and DISCONT
    test_caps_changed_verify(&mut h, 1 + 1 + 10, true, false);

    h.crank_single_clock_wait().unwrap();
    // Full GOP with HEADER but no DISCONT because caps change not
    // allowed from header-update-modex
    test_caps_changed_verify(&mut h, 1 + 10, false, false);

    h.crank_single_clock_wait().unwrap();
    h.push_event(gst::event::Eos::new());
    // Full GOP with HEADER but no DISCONT because no caps change
    test_caps_changed_verify(&mut h, 1 + 10, false, false);

    assert_eq!(h.buffers_in_queue(), 0);
}

#[test]
fn test_caps_change_within_gop() {
    init();

    let mut h = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));

    h.element()
        .unwrap()
        .set_property_from_str("header-update-mode", "caps");

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
        .build();

    h.element()
        .unwrap()
        .set_property("fragment-duration", 1.seconds());

    h.set_src_caps(caps);
    h.play();

    test_caps_changed_buffers(&mut h, 20, 10, 5, 100, true, false);

    h.crank_single_clock_wait().unwrap();
    // Initial fragment with HEADER and DISCONT
    test_caps_changed_verify(&mut h, 1 + 1 + 5, true, false);

    h.crank_single_clock_wait().unwrap();
    // Reduced GOP with HEADER and DISCONT due to caps change
    test_caps_changed_verify(&mut h, 1 + 1 + 5, true, false);

    h.crank_single_clock_wait().unwrap();
    h.push_event(gst::event::Eos::new());
    // Full GOP with HEADER but no DISCONT because no caps change
    test_caps_changed_verify(&mut h, 1 + 10, false, false);

    assert_eq!(h.buffers_in_queue(), 0);
}

#[test]
fn test_caps_change_within_gop_start_without_key() {
    init();

    let mut h = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));

    h.element()
        .unwrap()
        .set_property_from_str("header-update-mode", "caps");

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
        .build();

    h.element()
        .unwrap()
        .set_property("fragment-duration", 1.seconds());

    h.set_src_caps(caps);
    h.play();

    test_caps_changed_buffers(&mut h, 20, 10, 5, 100, true, true);

    // Same as test_caps_change_within_gop() but without the first
    // fragment since all frames are dropped due to missing key frame

    h.crank_single_clock_wait().unwrap();
    // Reduced GOP with HEADER and DISCONT due to caps change
    test_caps_changed_verify(&mut h, 1 + 1 + 5, true, false);

    h.crank_single_clock_wait().unwrap();
    h.push_event(gst::event::Eos::new());
    // Full GOP with HEADER but no DISCONT because no caps change
    test_caps_changed_verify(&mut h, 1 + 10, false, false);

    assert_eq!(h.buffers_in_queue(), 0);
}

#[test]
fn test_caps_change_within_gop_chunked() {
    init();

    let mut h = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));

    h.element()
        .unwrap()
        .set_property_from_str("header-update-mode", "caps");

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
        .build();

    h.element()
        .unwrap()
        .set_property("fragment-duration", 1.seconds());
    h.element()
        .unwrap()
        .set_property("chunk-duration", 300.mseconds());

    h.set_src_caps(caps);
    h.play();

    test_caps_changed_buffers(&mut h, 22, 10, 5, 30, true, false);

    h.crank_single_clock_wait().unwrap();
    // Initial fragment with HEADER and DISCONT
    test_caps_changed_verify(&mut h, 1 + 1 + 5, true, false);

    h.crank_single_clock_wait().unwrap();
    // Fragment with HEADER and DISCONT due to caps change
    test_caps_changed_verify(&mut h, 1 + 1 + 10, true, false);

    h.crank_single_clock_wait().unwrap();
    // Reduced chunk due to GOP end inbetween
    test_caps_changed_verify(&mut h, 1 + 5, false, true);

    h.crank_single_clock_wait().unwrap();
    h.push_event(gst::event::Eos::new());
    // Everything left until EOS
    test_caps_changed_verify(&mut h, 1 + 2, false, true);

    assert_eq!(h.buffers_in_queue(), 0);
}

#[test]
fn test_caps_change_within_gop_no_key() {
    init();

    let mut h = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));

    h.element()
        .unwrap()
        .set_property_from_str("header-update-mode", "caps");

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
        .build();

    h.element()
        .unwrap()
        .set_property("fragment-duration", 1.seconds());

    h.set_src_caps(caps);
    h.play();

    test_caps_changed_buffers(&mut h, 22, 10, 5, 100, false, false);

    h.crank_single_clock_wait().unwrap();
    // Initial fragment with HEADER and DISCONT
    test_caps_changed_verify(&mut h, 1 + 1 + 5, true, false);

    h.crank_single_clock_wait().unwrap();
    // Reduced GOP with HEADER and DISCONT due to caps change
    test_caps_changed_verify(&mut h, 1 + 1 + 5, true, false);
    test_caps_changed_verify(&mut h, 1 + 10, false, false);

    h.crank_single_clock_wait().unwrap();
    h.push_event(gst::event::Eos::new());
    // Everything left until EOS
    test_caps_changed_verify(&mut h, 1 + 2, false, false);

    assert_eq!(h.buffers_in_queue(), 0);
}

#[test]
fn test_caps_change_before_first_frame() {
    init();

    let mut h = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));

    h.element()
        .unwrap()
        .set_property_from_str("header-update-mode", "caps");

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::from_slice([1, 2, 3, 4]))
        .build();

    h.element()
        .unwrap()
        .set_property("fragment-duration", 1.seconds());

    h.set_src_caps(caps);
    h.play();

    test_caps_changed_buffers(&mut h, 22, 10, 0, 100, true, false);

    h.crank_single_clock_wait().unwrap();
    // Initial fragment with HEADER and DISCONT
    test_caps_changed_verify(&mut h, 1 + 1 + 10, true, false);

    h.crank_single_clock_wait().unwrap();
    // 2nd fragment with HEADER
    test_caps_changed_verify(&mut h, 1 + 10, false, false);

    assert_eq!(h.buffers_in_queue(), 0);
}

#[test]
fn test_cmaf_manual_split() {
    init();

    let caps = gst::Caps::builder("video/x-h264")
        .field("width", 1920i32)
        .field("height", 1080i32)
        .field("framerate", gst::Fraction::new(30, 1))
        .field("stream-format", "avc")
        .field("alignment", "au")
        .field("codec_data", gst::Buffer::with_size(1).unwrap())
        .build();

    let mut h = gst_check::Harness::new("cmafmux");

    // 5s fragment duration
    // manual-split mode
    h.element()
        .unwrap()
        .set_properties(&[("fragment-duration", &5.seconds()), ("manual-split", &true)]);

    h.set_src_caps(caps);
    h.play();

    // Push 7 buffers of 1s each, 1st and 6 buffer without DELTA_UNIT flag
    for i in 0..7 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(i.seconds());
            buffer.set_dts(i.seconds());
            buffer.set_duration(gst::ClockTime::SECOND);
            if i != 0 && i != 5 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            } else if i == 5 {
                h.push_event(
                    gst::event::CustomDownstream::builder(
                        gst::Structure::builder("FMP4MuxSplitNow").build(),
                    )
                    .build(),
                );
            }
        }
        assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    }

    // Crank the clock: this should bring us to the end of the first fragment
    h.crank_single_clock_wait().unwrap();

    let header = h.pull().unwrap();
    assert_eq!(
        header.flags(),
        gst::BufferFlags::HEADER | gst::BufferFlags::DISCONT
    );
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(header.dts(), Some(gst::ClockTime::ZERO));

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(fragment_header.dts(), Some(gst::ClockTime::ZERO));
    assert_eq!(fragment_header.duration(), Some(5.seconds()));

    for i in 0..5 {
        let buffer = h.pull().unwrap();
        if i == 4 {
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
        } else {
            assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
        }
        assert_eq!(buffer.pts(), Some(i.seconds()));
        assert_eq!(buffer.dts(), Some(i.seconds()));
        assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
    }

    h.push_event(gst::event::Eos::new());

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(5.seconds()));
    assert_eq!(fragment_header.dts(), Some(5.seconds()));
    assert_eq!(fragment_header.duration(), Some(2.seconds()));

    for i in 5..7 {
        let buffer = h.pull().unwrap();
        if i == 6 {
            assert_eq!(
                buffer.flags(),
                gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
            );
        } else {
            assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
        }
        assert_eq!(buffer.pts(), Some(i.seconds()));
        assert_eq!(buffer.dts(), Some(i.seconds()));
        assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
    }

    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::StreamStart);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Caps);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Segment);
    let ev = h.pull_event().unwrap();
    assert_eq!(ev.type_(), gst::EventType::Eos);
}
