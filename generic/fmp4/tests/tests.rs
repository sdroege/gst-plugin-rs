//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstfmp4::plugin_register_static().unwrap();
    });
}

#[test]
fn test_buffer_flags() {
    init();

    // 5s fragment duration
    let mut h = gst_check::Harness::new_parse("isofmp4mux fragment-duration=5000000000");
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
    assert_eq!(header.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(header.dts(), Some(gst::ClockTime::ZERO));

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(fragment_header.dts(), Some(gst::ClockTime::ZERO));
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
        assert_eq!(buffer.pts(), Some(gst::ClockTime::from_seconds(i)));
        assert_eq!(buffer.dts(), Some(gst::ClockTime::from_seconds(i)));
        assert_eq!(buffer.duration(), Some(gst::ClockTime::SECOND));
    }

    h.push_event(gst::event::Eos::new());

    let fragment_header = h.pull().unwrap();
    assert_eq!(fragment_header.flags(), gst::BufferFlags::HEADER);
    assert_eq!(fragment_header.pts(), Some(gst::ClockTime::from_seconds(5)));
    assert_eq!(fragment_header.dts(), Some(gst::ClockTime::from_seconds(5)));
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
        assert_eq!(buffer.pts(), Some(gst::ClockTime::from_seconds(i)));
        assert_eq!(buffer.dts(), Some(gst::ClockTime::from_seconds(i)));
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
