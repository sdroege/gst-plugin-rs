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

fn test_buffer_flags_single_stream(cmaf: bool) {
    let mut h = if cmaf {
        gst_check::Harness::new("cmafmux")
    } else {
        gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"))
    };

    // 5s fragment duration
    h.element()
        .unwrap()
        .set_property("fragment-duration", gst::ClockTime::from_seconds(5));

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

    let output_offset = if cmaf {
        gst::ClockTime::ZERO
    } else {
        gst::ClockTime::from_seconds(60 * 60 * 1000)
    };

    // Push 7 buffers of 1s each, 1st and 6 buffer without DELTA_UNIT flag
    for i in 0..7 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_seconds(i));
            buffer.set_dts(gst::ClockTime::from_seconds(i));
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
                    running_time: Some(gst::ClockTime::from_seconds(5)),
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
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(5))
    );

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
        assert_eq!(
            buffer.pts(),
            Some(gst::ClockTime::from_seconds(i) + output_offset)
        );
        assert_eq!(
            buffer.dts(),
            Some(gst::ClockTime::from_seconds(i) + output_offset)
        );
        assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
    }

    h.push_event(gst::event::Eos::new());

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::from_seconds(5) + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::from_seconds(5) + output_offset)
    );
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(2))
    );

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
        assert_eq!(
            buffer.pts(),
            Some(gst::ClockTime::from_seconds(i) + output_offset)
        );
        assert_eq!(
            buffer.dts(),
            Some(gst::ClockTime::from_seconds(i) + output_offset)
        );
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
fn test_buffer_flags_single_stream_cmaf() {
    init();

    test_buffer_flags_single_stream(true);
}

#[test]
fn test_buffer_flags_single_stream_iso() {
    init();

    test_buffer_flags_single_stream(false);
}

#[test]
fn test_buffer_flags_multi_stream() {
    init();

    let mut h1 = gst_check::Harness::with_padnames("isofmp4mux", Some("sink_0"), Some("src"));
    let mut h2 = gst_check::Harness::with_element(&h1.element().unwrap(), Some("sink_1"), None);

    // 5s fragment duration
    h1.element()
        .unwrap()
        .set_property("fragment-duration", gst::ClockTime::from_seconds(5));

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

    let output_offset = gst::ClockTime::from_seconds(60 * 60 * 1000);

    // Push 7 buffers of 1s each, 1st and last buffer without DELTA_UNIT flag
    for i in 0..7 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_seconds(i));
            buffer.set_dts(gst::ClockTime::from_seconds(i));
            buffer.set_duration(gst::ClockTime::SECOND);
            if i != 0 && i != 5 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h1.push(buffer), Ok(gst::FlowSuccess::Ok));

        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_seconds(i));
            buffer.set_dts(gst::ClockTime::from_seconds(i));
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
                    running_time: Some(gst::ClockTime::from_seconds(5)),
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
                    running_time: Some(gst::ClockTime::from_seconds(5)),
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
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(5))
    );

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

            assert_eq!(
                buffer.pts(),
                Some(gst::ClockTime::from_seconds(i) + output_offset)
            );

            if j == 0 {
                assert_eq!(
                    buffer.dts(),
                    Some(gst::ClockTime::from_seconds(i) + output_offset)
                );
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
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::from_seconds(5) + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::from_seconds(5) + output_offset)
    );
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(2))
    );

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
            assert_eq!(
                buffer.pts(),
                Some(gst::ClockTime::from_seconds(i) + output_offset)
            );
            if j == 0 {
                assert_eq!(
                    buffer.dts(),
                    Some(gst::ClockTime::from_seconds(i) + output_offset)
                );
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
        .set_property("fragment-duration", gst::ClockTime::from_seconds(5));

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

    let output_offset = gst::ClockTime::from_seconds(60 * 60 * 1000);

    // Push 7 buffers of 1s each, 1st and last buffer without DELTA_UNIT flag
    for i in 0..7 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_seconds(i));
            buffer.set_dts(gst::ClockTime::from_seconds(i));
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
                buffer.set_pts(gst::ClockTime::from_seconds(i));
                buffer.set_dts(gst::ClockTime::from_seconds(i));
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
                    running_time: Some(gst::ClockTime::from_seconds(5)),
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
                    running_time: Some(gst::ClockTime::from_seconds(5)),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Advance time and crank the clock: this should bring us to the end of the first fragment
    h1.set_time(gst::ClockTime::from_seconds(5)).unwrap();
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
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(5))
    );

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

            assert_eq!(
                buffer.pts(),
                Some(gst::ClockTime::from_seconds(i) + output_offset)
            );

            if j == 0 {
                assert_eq!(
                    buffer.dts(),
                    Some(gst::ClockTime::from_seconds(i) + output_offset)
                );
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
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::from_seconds(5) + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::from_seconds(5) + output_offset)
    );
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(2))
    );

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
            assert_eq!(
                buffer.pts(),
                Some(gst::ClockTime::from_seconds(i) + output_offset)
            );
            if j == 0 {
                assert_eq!(
                    buffer.dts(),
                    Some(gst::ClockTime::from_seconds(i) + output_offset)
                );
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
        .set_property("fragment-duration", gst::ClockTime::from_seconds(5));

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

    let output_offset = gst::ClockTime::from_seconds(60 * 60 * 1000);

    // Push 7 buffers of 1s each, 1st and last buffer without DELTA_UNIT flag
    for i in 0..7 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_seconds(i));
            buffer.set_dts(gst::ClockTime::from_seconds(i));
            buffer.set_duration(gst::ClockTime::SECOND);
            if i != 0 && i != 5 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h1.push(buffer), Ok(gst::FlowSuccess::Ok));

        // Replace buffer 3 and 6 with a gap event
        if i == 3 || i == 6 {
            let ev = gst::event::Gap::builder(gst::ClockTime::from_seconds(i))
                .duration(gst::ClockTime::SECOND)
                .build();
            assert!(h2.push_event(ev));
        } else {
            let mut buffer = gst::Buffer::with_size(1).unwrap();
            {
                let buffer = buffer.get_mut().unwrap();
                buffer.set_pts(gst::ClockTime::from_seconds(i));
                buffer.set_dts(gst::ClockTime::from_seconds(i));
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
                    running_time: Some(gst::ClockTime::from_seconds(5)),
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
                    running_time: Some(gst::ClockTime::from_seconds(5)),
                    all_headers: true,
                    count: 0
                }
            );
        }
    }

    // Advance time and crank the clock: this should bring us to the end of the first fragment
    h1.set_time(gst::ClockTime::from_seconds(5)).unwrap();
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
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(5))
    );

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

            assert_eq!(
                buffer.pts(),
                Some(gst::ClockTime::from_seconds(i) + output_offset)
            );

            if j == 0 {
                assert_eq!(
                    buffer.dts(),
                    Some(gst::ClockTime::from_seconds(i) + output_offset)
                );
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
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::from_seconds(5) + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::from_seconds(5) + output_offset)
    );
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(2))
    );

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
            assert_eq!(
                buffer.pts(),
                Some(gst::ClockTime::from_seconds(i) + output_offset)
            );
            if j == 0 {
                assert_eq!(
                    buffer.dts(),
                    Some(gst::ClockTime::from_seconds(i) + output_offset)
                );
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
        .set_property("fragment-duration", gst::ClockTime::from_seconds(5));

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

    let output_offset = gst::ClockTime::from_seconds(60 * 60 * 1000);

    // Push 8 buffers of 1s each, 1st, 4th and 7th buffer without DELTA_UNIT flag
    for i in 0..8 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_seconds(i));
            buffer.set_dts(gst::ClockTime::from_seconds(i));
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

            let fku_time = if i == 2 {
                gst::ClockTime::from_seconds(5)
            } else {
                gst::ClockTime::from_seconds(8)
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
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(3))
    );

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
        assert_eq!(
            buffer.pts(),
            Some(gst::ClockTime::from_seconds(i) + output_offset)
        );
        assert_eq!(
            buffer.dts(),
            Some(gst::ClockTime::from_seconds(i) + output_offset)
        );
        assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
    }

    h.push_event(gst::event::Eos::new());

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::from_seconds(3) + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::from_seconds(3) + output_offset)
    );
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(5))
    );

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
        assert_eq!(
            buffer.pts(),
            Some(gst::ClockTime::from_seconds(i) + output_offset)
        );
        assert_eq!(
            buffer.dts(),
            Some(gst::ClockTime::from_seconds(i) + output_offset)
        );
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
        .set_property("fragment-duration", gst::ClockTime::from_seconds(5));

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

    let output_offset = gst::ClockTime::from_seconds(60 * 60 * 1000);

    // Push 10 buffers of 1s each, 1st and 7th buffer without DELTA_UNIT flag
    for i in 0..10 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_seconds(i));
            buffer.set_dts(gst::ClockTime::from_seconds(i));
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

            let fku_time = if i == 2 {
                gst::ClockTime::from_seconds(5)
            } else {
                gst::ClockTime::from_seconds(11)
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
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(6))
    );

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
        assert_eq!(
            buffer.pts(),
            Some(gst::ClockTime::from_seconds(i) + output_offset)
        );
        assert_eq!(
            buffer.dts(),
            Some(gst::ClockTime::from_seconds(i) + output_offset)
        );
        assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
    }

    h.push_event(gst::event::Eos::new());

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::from_seconds(6) + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::from_seconds(6) + output_offset)
    );
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(4))
    );

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
        assert_eq!(
            buffer.pts(),
            Some(gst::ClockTime::from_seconds(i) + output_offset)
        );
        assert_eq!(
            buffer.dts(),
            Some(gst::ClockTime::from_seconds(i) + output_offset)
        );
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
        .set_property("fragment-duration", gst::ClockTime::from_seconds(5));

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

    let output_offset = gst::ClockTime::from_seconds(60 * 60 * 1000);

    // Push 8 buffers of 1s each, 1st, 4th and 7th buffer without DELTA_UNIT flag
    for i in 0..8 {
        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_seconds(i));
            buffer.set_dts(gst::ClockTime::from_seconds(i));
            buffer.set_duration(gst::ClockTime::SECOND);
            if i != 0 && i != 3 && i != 6 {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        assert_eq!(h1.push(buffer), Ok(gst::FlowSuccess::Ok));

        let mut buffer = gst::Buffer::with_size(1).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_seconds(i));
            buffer.set_dts(gst::ClockTime::from_seconds(i));
            buffer.set_duration(gst::ClockTime::SECOND);
        }
        assert_eq!(h2.push(buffer), Ok(gst::FlowSuccess::Ok));

        if i == 2 || i == 7 {
            let ev = loop {
                let ev = h1.pull_upstream_event().unwrap();
                if ev.type_() != gst::EventType::Reconfigure
                    && ev.type_() != gst::EventType::Latency
                {
                    break ev;
                }
            };

            let fku_time = if i == 2 {
                gst::ClockTime::from_seconds(5)
            } else {
                gst::ClockTime::from_seconds(8)
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
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(3))
    );

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

            assert_eq!(
                buffer.pts(),
                Some(gst::ClockTime::from_seconds(i) + output_offset)
            );

            if j == 0 {
                assert_eq!(
                    buffer.dts(),
                    Some(gst::ClockTime::from_seconds(i) + output_offset)
                );
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
    assert_eq!(
        fragment_header.pts(),
        Some(gst::ClockTime::from_seconds(3) + output_offset)
    );
    assert_eq!(
        fragment_header.dts(),
        Some(gst::ClockTime::from_seconds(3) + output_offset)
    );
    assert_eq!(
        fragment_header.duration(),
        Some(gst::ClockTime::from_seconds(5))
    );

    for i in 3..8 {
        for j in 0..2 {
            let buffer = h1.pull().unwrap();
            if i == 7 && j == 1 {
                assert_eq!(
                    buffer.flags(),
                    gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
                );
            } else {
                assert_eq!(buffer.flags(), gst::BufferFlags::DELTA_UNIT);
            }
            assert_eq!(
                buffer.pts(),
                Some(gst::ClockTime::from_seconds(i) + output_offset)
            );
            if j == 0 {
                assert_eq!(
                    buffer.dts(),
                    Some(gst::ClockTime::from_seconds(i) + output_offset)
                );
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
