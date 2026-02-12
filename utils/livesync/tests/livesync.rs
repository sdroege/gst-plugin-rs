// Copyright (C) 2022 LTN Global Communications, Inc.
// Contact: Jan Alexander Steffens (heftig) <jan.steffens@ltnglobal.com>
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
        gstlivesync::plugin_register_static().expect("Failed to register livesync plugin");
    });
}

const DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(100);
const LATENCY: gst::ClockTime = gst::ClockTime::from_mseconds(200);

fn crank_pull(harness: &mut gst_check::Harness) -> gst::Buffer {
    harness.crank_single_clock_wait().unwrap();
    harness.pull().unwrap()
}

#[track_caller]
fn assert_buf(
    buf: &gst::BufferRef,
    offset: u64,
    pts: gst::ClockTime,
    duration: gst::ClockTime,
    flags: gst::BufferFlags,
) {
    assert_eq!(buf.offset(), offset, "Bad offset");
    assert_eq!(buf.pts(), Some(pts), "Bad PTS");
    assert_eq!(buf.duration(), Some(duration), "Bad duration");
    assert_eq!(
        buf.flags() - gst::BufferFlags::TAG_MEMORY,
        flags,
        "Bad flags",
    );
}

#[track_caller]
fn assert_crank_pull(
    harness: &mut gst_check::Harness,
    offset_per_buffer: u64,
    src_buffer_number: u64,
    sink_buffer_number: u64,
    flags: gst::BufferFlags,
    singlesegment: bool,
) {
    let pts = if singlesegment {
        LATENCY + DURATION * sink_buffer_number
    } else {
        DURATION * sink_buffer_number
    };
    assert_buf(
        &crank_pull(harness),
        offset_per_buffer * src_buffer_number,
        pts,
        DURATION,
        flags,
    );
}

#[test]
fn test_video_singlesegment() {
    test_video(true);
}

#[test]
// FIXME: racy: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/issues/328
#[ignore]
fn test_audio_singlesegment() {
    test_audio(true);
}

#[test]
// FIXME: racy: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/issues/357
#[ignore]
fn test_video_nonsinglesegment() {
    test_video(false);
}

#[test]
// FIXME: racy: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/issues/328
#[ignore]
fn test_audio_nonsinglesegment() {
    test_audio(false);
}

fn test_video(singlesegment: bool) {
    init();

    let mut h = gst_check::Harness::new("livesync");
    h.add_src_parse(
        r"videotestsrc is-live=1
          ! capsfilter caps=video/x-raw,framerate=10/1
        ",
        true,
    );

    let element = h.element().unwrap();
    element.set_property("latency", LATENCY);
    element.set_property("single-segment", singlesegment);

    test_livesync(&mut h, 1, singlesegment);
}

fn test_audio(singlesegment: bool) {
    init();

    let mut h = gst_check::Harness::new("livesync");
    h.add_src_parse(
        r"audiotestsrc is-live=1 samplesperbuffer=4800
          ! capsfilter caps=audio/x-raw,rate=48000
        ",
        true,
    );

    let element = h.element().unwrap();
    element.set_property("latency", LATENCY);
    element.set_property("single-segment", singlesegment);

    test_livesync(&mut h, 4800, singlesegment);
}

fn test_livesync(h: &mut gst_check::Harness, o: u64, singlesegment: bool) {
    // Normal operation ------------------------------

    // Push frames 0-1, pull frame 0
    h.push_from_src().unwrap();
    h.push_from_src().unwrap();
    assert_eq!(h.pull_event().unwrap().type_(), gst::EventType::StreamStart);
    // Caps are only output once waiting for the first buffer has finished
    h.crank_single_clock_wait().unwrap();
    assert_eq!(h.pull_event().unwrap().type_(), gst::EventType::Caps);
    assert_eq!(h.pull_event().unwrap().type_(), gst::EventType::Segment);
    assert_crank_pull(h, o, 0, 0, gst::BufferFlags::DISCONT, singlesegment);

    // Push frames 2-10, pull frames 1-9
    for i in 1..=9 {
        h.push_from_src().unwrap();
        assert_crank_pull(h, o, i, i, gst::BufferFlags::empty(), singlesegment);
    }

    // Pull frame 10
    assert_crank_pull(h, o, 10, 10, gst::BufferFlags::empty(), singlesegment);

    // Bridging gap ----------------------------------

    // Pull frames 11-19
    for i in 11..=19 {
        assert_crank_pull(h, o, 10, i, gst::BufferFlags::GAP, singlesegment);
    }

    // Push frames 11-19
    for _ in 11..=19 {
        h.push_from_src().unwrap();
    }

    // Normal operation ------------------------------

    // Push frames 20-21, pull frame 20
    for _ in 1..=2 {
        let mut src_h = h.src_harness_mut().unwrap();
        src_h.crank_single_clock_wait().unwrap();
        let mut buf = src_h.pull().unwrap();
        let buf_mut = buf.make_mut();
        buf_mut.set_flags(gst::BufferFlags::MARKER);
        h.push(buf).unwrap();
    }
    assert_crank_pull(h, o, 10, 20, gst::BufferFlags::GAP, singlesegment);

    // Push frame 22, pull frame 21
    h.push_from_src().unwrap();
    assert_crank_pull(
        h,
        o,
        21,
        21,
        gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER,
        singlesegment,
    );

    // Push frames 23-30, pull frames 22-29
    for i in 22..=29 {
        h.push_from_src().unwrap();
        assert_crank_pull(h, o, i, i, gst::BufferFlags::empty(), singlesegment);
    }

    // EOS -------------------------------------------
    assert!(h.push_event(gst::event::Eos::new()));

    // Pull frame 30
    assert_crank_pull(h, o, 30, 30, gst::BufferFlags::empty(), singlesegment);

    assert_eq!(h.pull_event().unwrap().type_(), gst::EventType::Eos);
    assert_eq!(h.try_pull(), None);
}

#[test]
// FIXME: can be racy on CI: another repeated buffer might go through
// between the moment when we push buf2 and the next try_pull_object
#[ignore]
fn segment_change_non_single_segment() {
    segment_change(false, gst::SegmentFlags::empty());
}

#[test]
// FIXME: can be racy on CI: another repeated buffer might go through
// between the moment when we push buf2 and the next try_pull_object
#[ignore]
fn segment_change_non_single_segment_flag() {
    segment_change(false, gst::SegmentFlags::SEGMENT);
}

#[test]
// FIXME: can be racy on CI: another repeated buffer might go through
// between the moment when we push buf2 and the next try_pull_object
#[ignore]
fn segment_change_single_segment() {
    segment_change(true, gst::SegmentFlags::empty());
}

fn segment_change(single_segment: bool, segment_flags: gst::SegmentFlags) {
    const RATE: i32 = 44_100;
    const BUF_DURATION_MS: u64 = 20;
    const BUF_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(BUF_DURATION_MS);
    const LATENCY: gst::ClockTime = BUF_DURATION;
    const BUF_BPF: usize = BUF_DURATION_MS as usize * RATE as usize / 1_000;
    const LAST_BUFFER_START: gst::ClockTime = gst::ClockTime::from_mseconds(4 * BUF_DURATION_MS);
    const SECOND_SEG_START: gst::ClockTime = gst::ClockTime::from_mseconds(BUF_DURATION_MS / 4);
    const SECOND_SEG_STOP: gst::ClockTime =
        gst::ClockTime::from_mseconds(LAST_BUFFER_START.mseconds() + BUF_DURATION_MS / 4);

    gst::init().unwrap();
    gstlivesync::plugin_register_static().unwrap();

    let pipe = gst::parse::launch(&format!(
        "appsrc name=src format=time emit-signals=false
            ! livesync single-segment={single_segment} sync=false latency={}
            ! appsink name=sink sync=false
            ",
        BUF_DURATION.nseconds(),
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();

    let src = pipe
        .by_name("src")
        .and_downcast::<gst_app::AppSrc>()
        .unwrap();

    let sink = pipe
        .by_name("sink")
        .and_downcast::<gst_app::AppSink>()
        .unwrap();

    let clock = gst::SystemClock::obtain();
    pipe.use_clock(Some(&clock));

    pipe.set_state(gst::State::Playing).unwrap();

    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S8")
        .field("channels", 1)
        .field("rate", RATE)
        .field("layout", "interleaved")
        .build();

    src.set_caps(Some(&caps));

    let mut buf1 = gst::Buffer::from_slice([1; BUF_BPF]);
    {
        let buf = buf1.make_mut();
        buf.set_pts(gst::ClockTime::ZERO);
        buf.set_duration(BUF_DURATION);
    }
    let seg1 = gst::FormattedSegment::<gst::format::Time>::new();
    src.send_event(gst::event::Segment::new(&seg1));

    {
        let map = buf1.map_readable().unwrap();
        println!(
            ":: pushing buf0 {}, rt {:?}",
            map[0],
            seg1.to_running_time(buf1.pts()).unwrap(),
        );
    }
    src.push_buffer(buf1).unwrap();

    let obj = sink.try_pull_object(gst::ClockTime::NONE).unwrap();
    let event = obj.downcast_ref::<gst::Event>().unwrap();
    println!("* {event:?}");

    let obj = sink.try_pull_object(gst::ClockTime::NONE).unwrap();
    let event = obj.downcast_ref::<gst::Event>().unwrap();
    println!("* {event:?}\n");

    let mut seg_evt_seqnum = None;
    let mut seg2 = gst::FormattedSegment::<gst::format::Time>::new();
    let mut buf2_rt = gst::ClockTime::NONE;
    let mut count = 0;
    while let Some(obj) = sink.try_pull_object(gst::ClockTime::NONE) {
        if obj.type_() == gst::Sample::static_type() {
            let Some(sample) = obj.downcast_ref::<gst::Sample>() else {
                unreachable!();
            };

            let cur_seg = sample
                .segment()
                .unwrap()
                .downcast_ref::<gst::format::Time>()
                .unwrap();
            let buf = sample.buffer().unwrap();
            let map = buf.map_readable().unwrap();
            // in single-segment mode, segment is offset by latency
            let rt = cur_seg
                .to_running_time_full(buf.pts())
                .and_then(|rt| rt.positive())
                .unwrap()
                - if single_segment {
                    LATENCY
                } else {
                    gst::ClockTime::ZERO
                };
            println!(
                "* pts {} byte {}, rt {rt:?}, duration {}",
                buf.pts().unwrap(),
                map[0],
                buf.duration().unwrap(),
            );

            if count == 2 {
                // Push another segment and another buffer with a different byte sentinel

                let mut buf2 = gst::Buffer::from_slice([2; BUF_BPF]);
                {
                    let buf = buf2.make_mut();
                    buf.set_pts(gst::ClockTime::ZERO);
                    buf.set_duration(BUF_DURATION);
                }
                // Clip buffer on start
                seg2.set_start(SECOND_SEG_START);
                seg2.set_stop(SECOND_SEG_STOP);
                let base = pipe.current_running_time().unwrap();
                seg2.set_base(base);
                seg2.set_flags(segment_flags);
                src.send_event(gst::event::Segment::new(&seg2));

                // in singl-segment mode, buffer will be clipped to comply with start
                buf2_rt = if single_segment {
                    seg2.to_running_time(SECOND_SEG_START)
                } else {
                    seg2.to_running_time_full(buf2.pts())
                        .and_then(|rt| rt.positive())
                };

                {
                    let map = buf2.map_readable().unwrap();
                    println!(
                        ":: pushing buf1 byte {}, rt {buf2_rt:?}, base {base}",
                        map[0]
                    );
                }
                src.push_buffer(buf2).unwrap();

                count += 1;
                continue;
            }

            let pts = buf.pts().unwrap();
            let first_byte = map[0] as i8;
            if count == 0 {
                assert_eq!(first_byte, 1);
            } else if let Some(buf2_rt) = buf2_rt
                && buf2_rt == rt
            {
                // buf2
                assert_eq!(first_byte, 2);
                // clipped in single-segment mode
                if single_segment {
                    assert_eq!(buf.duration().unwrap(), BUF_DURATION - SECOND_SEG_START);
                } else {
                    assert_eq!(buf.duration().unwrap(), BUF_DURATION);
                }
            } else {
                assert_eq!(
                    first_byte, 0,
                    "buffer missmatch @ count {count}, pts {pts}, rt {rt:?}: expected byte 0, got {first_byte}"
                );
                if pts >= LAST_BUFFER_START {
                    // buffers were repeated from buf2
                    if single_segment {
                        assert_eq!(buf.duration().unwrap(), BUF_DURATION - SECOND_SEG_START);
                    } else {
                        assert_eq!(buf.duration().unwrap(), BUF_DURATION);
                    }

                    let event = if segment_flags.contains(gst::SegmentFlags::SEGMENT) {
                        gst::event::SegmentDone::new(LAST_BUFFER_START)
                    } else {
                        gst::event::Eos::new()
                    };

                    println!(":: last buffer => sending {event:?}");
                    assert!(src.send_event(event));
                }
            }

            count += 1;
        } else {
            let Some(event) = obj.downcast_ref::<gst::Event>() else {
                panic!();
            };

            use gst::EventView::*;
            match event.view() {
                Segment(evt) => {
                    println!("* {evt:?}");
                    seg_evt_seqnum = Some(evt.seqnum());
                }
                SegmentDone(evt) => {
                    if segment_flags.contains(gst::SegmentFlags::SEGMENT) {
                        println!("* {evt:?}");
                        assert_eq!(
                            evt.seqnum(),
                            seg_evt_seqnum.expect("second event must be sent at this point")
                        );
                        break;
                    }
                    panic!("Unexpected SegmentDone event");
                }
                _ => (),
            }
        }
    }

    seg_evt_seqnum.expect("second event must be sent at this point");
    pipe.set_state(gst::State::Null).unwrap();
}
