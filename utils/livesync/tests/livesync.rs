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
const REVERSE_STOP: gst::ClockTime = gst::ClockTime::from_seconds(100);
const LATE_THRESHOLD: gst::ClockTime = gst::ClockTime::from_mseconds(200);

// Duration limits copied from livesync/imp.rs
const MINIMUM_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(8);
const DEFAULT_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(100);
const MAXIMUM_DURATION: gst::ClockTime = gst::ClockTime::from_seconds(10);

struct TestAudio {
    rate: u64,
}

impl TestAudio {
    fn for_rate(rate: u64) -> Self {
        TestAudio { rate }
    }

    fn caps(&self) -> String {
        format!(
            "audio/x-raw,format=S8,rate={},channels=1,layout=interleaved",
            self.rate
        )
    }

    fn samples(&self, duration: gst::ClockTime) -> usize {
        let second = gst::ClockTime::SECOND.nseconds();
        let samples = duration.nseconds() * self.rate;
        assert_eq!(samples % second, 0, "{duration} isn't whole samples");
        (samples / second) as usize
    }

    fn buffer(&self, duration: gst::ClockTime) -> gst::Buffer {
        audio_buffer(self.samples(duration), duration)
    }
}

fn audio_buffer(samples: usize, duration: gst::ClockTime) -> gst::Buffer {
    let mut buf = gst::Buffer::from_slice(vec![0x11; samples]);
    {
        let buf = buf.make_mut();
        buf.set_pts(gst::ClockTime::ZERO);
        buf.set_duration(duration);
    }
    buf
}

struct TestVideo {
    framerate: gst::Fraction,
}

impl TestVideo {
    fn for_framerate(numer: i32, denom: i32) -> Self {
        TestVideo {
            framerate: gst::Fraction::new(numer, denom),
        }
    }

    fn for_duration(frame_duration: gst::ClockTime) -> Self {
        let video = Self::for_framerate(1_000_000, frame_duration.useconds() as i32);
        assert_eq!(
            video.pts(1),
            frame_duration,
            "{frame_duration} isn't whole microseconds"
        );
        video
    }

    fn caps(&self) -> String {
        format!(
            "video/x-raw,format=GRAY8,width=4,height=4,framerate={}/{}",
            self.framerate.numer(),
            self.framerate.denom(),
        )
    }

    // Same pts for frame n as what videotestsrc would output
    fn pts(&self, n: u64) -> gst::ClockTime {
        let numer = self.framerate.numer() as u64;
        let denom = self.framerate.denom() as u64;
        gst::ClockTime::SECOND
            .mul_div_floor(n * denom, numer)
            .unwrap()
    }

    // Frame with pts and duration matching what videotestsrc would generate
    fn frame(&self, n: u64) -> gst::Buffer {
        let pts = self.pts(n);
        let mut buf = video_buffer(self.pts(n + 1) - pts);
        {
            let buf = buf.make_mut();
            buf.set_pts(pts);
            buf.set_offset(n);
        }
        buf
    }
}

fn video_buffer(duration: gst::ClockTime) -> gst::Buffer {
    let mut buf = gst::Buffer::from_slice(vec![0x11; 16]);
    {
        let buf = buf.make_mut();
        buf.set_pts(gst::ClockTime::ZERO);
        buf.set_duration(duration);
    }
    buf
}

fn crank_pull(harness: &mut gst_check::Harness) -> gst::Buffer {
    harness.crank_single_clock_wait().unwrap();
    harness.pull().unwrap()
}

fn livesync_harness(caps: &str, single_segment: bool, rate: f64) -> gst_check::Harness {
    init();

    let mut h = gst_check::Harness::new("livesync");
    h.element()
        .unwrap()
        .set_property("single-segment", single_segment);
    h.set_src_caps_str(caps);

    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.set_rate(rate);
    if rate < 0.0 {
        segment.set_stop(REVERSE_STOP);
    }
    assert!(h.push_event(gst::event::Segment::new(&segment)));

    h
}

fn push_and_pull_repeats(
    h: &mut gst_check::Harness,
    buffer: gst::Buffer,
    repeats: usize,
) -> Vec<gst::Buffer> {
    h.push(buffer).unwrap();
    (0..=repeats).map(|_| crank_pull(h)).collect()
}

fn with_pts(mut buffer: gst::Buffer, pts: gst::ClockTime) -> gst::Buffer {
    buffer.make_mut().set_pts(pts);
    buffer
}

fn without_duration(mut buffer: gst::Buffer) -> gst::Buffer {
    buffer.make_mut().set_duration(gst::ClockTime::NONE);
    buffer
}

fn filled_with(buf: &gst::BufferRef, byte: u8) -> bool {
    buf.map_readable().unwrap().iter().all(|&b| b == byte)
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

#[track_caller]
fn assert_contiguous(buffers: &[gst::Buffer], is_forward: bool) {
    for pair in buffers.windows(2) {
        let (earlier, later) = if is_forward {
            (&pair[0], &pair[1])
        } else {
            (&pair[1], &pair[0])
        };
        assert_eq!(
            earlier.pts().unwrap() + earlier.duration().unwrap(),
            later.pts().unwrap(),
            "{earlier:?} and {later:?} don't line up"
        );
    }
}

#[track_caller]
fn assert_repeats_last(buffers: &[gst::Buffer], duration: gst::ClockTime) {
    for buf in &buffers[1..] {
        assert_eq!(buf.duration(), Some(duration), "{buf:?}");
    }
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

#[test]
fn repeat_short_video_frames() {
    // Frames shorter than the minimum are repeated for the minimum duration
    let video = TestVideo::for_duration(MINIMUM_DURATION / 2);
    let mut h = livesync_harness(&video.caps(), false, 1.0);
    let buffers = push_and_pull_repeats(&mut h, video.frame(0), 3);

    assert_repeats_last(&buffers, MINIMUM_DURATION);
}

#[test]
fn repeat_long_video_frames() {
    // Frames longer than the maximum are repeated for the maximum duration
    let video = TestVideo::for_duration(MAXIMUM_DURATION * 2);
    let mut h = livesync_harness(&video.caps(), false, 1.0);
    let buffers = push_and_pull_repeats(&mut h, video.frame(0), 3);

    assert_repeats_last(&buffers, MAXIMUM_DURATION);
}

#[test]
fn repeat_video_without_framerate() {
    // Frames without a framerate are repeated for their own duration
    let video = TestVideo::for_framerate(0, 1);
    let mut h = livesync_harness(&video.caps(), false, 1.0);
    let duration = gst::ClockTime::from_mseconds(40);
    let buffers = push_and_pull_repeats(&mut h, video_buffer(duration), 3);

    assert_repeats_last(&buffers, duration);
}

#[test]
fn repeat_video_without_framerate_outside_duration_limits() {
    // Frames without a framerate outside limits are repeated for the default duration
    let video = TestVideo::for_framerate(0, 1);

    for duration in [MINIMUM_DURATION / 2, MAXIMUM_DURATION * 2] {
        let mut h = livesync_harness(&video.caps(), false, 1.0);
        let buffers = push_and_pull_repeats(&mut h, video_buffer(duration), 3);

        assert_repeats_last(&buffers, DEFAULT_DURATION);
    }
}

#[test]
fn repeat_video_frame() {
    let duration = gst::ClockTime::from_mseconds(40);
    let video = TestVideo::for_duration(duration);
    let mut h = livesync_harness(&video.caps(), false, 1.0);
    let buffers = push_and_pull_repeats(&mut h, video.frame(0), 3);

    // Original buffer goes out as is...
    assert_buf(
        &buffers[0],
        0,
        gst::ClockTime::ZERO,
        duration,
        gst::BufferFlags::DISCONT,
    );

    // ...and repeats copy its offset and contents + add GAP flags
    for (n, buf) in buffers.iter().enumerate().skip(1) {
        assert_buf(buf, 0, duration * n as u64, duration, gst::BufferFlags::GAP);
        assert!(filled_with(buf, 0x11), "{buf:?}");
    }
}

#[test]
fn repeat_audio_silence() {
    let audio = TestAudio::for_rate(48_000);
    let mut h = livesync_harness(&audio.caps(), false, 1.0);
    let duration = gst::ClockTime::from_mseconds(10);
    let buffers = push_and_pull_repeats(&mut h, audio.buffer(duration), 3);

    // Original buffer keeps its audio...
    let original = &buffers[0];
    assert!(!filled_with(original, 0), "{original:?}");
    assert!(
        !original.flags().contains(gst::BufferFlags::GAP),
        "{original:?}"
    );

    // ...while repeats are silence flagged as gaps
    for buf in &buffers[1..] {
        assert!(buf.flags().contains(gst::BufferFlags::GAP), "{buf:?}");
        assert!(filled_with(buf, 0), "{buf:?}");
    }
}

#[test]
fn repeat_audio_outside_duration_limits() {
    // Audio outside duration limits is repeated as silence of the default duration
    let audio = TestAudio::for_rate(48_000);

    for duration in [MINIMUM_DURATION / 2, MAXIMUM_DURATION * 2] {
        let mut h = livesync_harness(&audio.caps(), false, 1.0);
        let buffers = push_and_pull_repeats(&mut h, audio.buffer(duration), 3);

        assert_repeats_last(&buffers, DEFAULT_DURATION);
        for buf in &buffers[1..] {
            assert_eq!(buf.size(), audio.samples(DEFAULT_DURATION), "{buf:?}");
        }
    }
}

#[test]
fn audio_buffer_duration_more_than_one_sample_off() {
    // 480 samples last 10ms, so a 20ms duration is surely more than 1 sample off
    // and livesync should correct the output duration
    let audio = TestAudio::for_rate(48_000);
    let mut h = livesync_harness(&audio.caps(), false, 1.0);
    let duration = gst::ClockTime::from_mseconds(10);
    let original = audio_buffer(audio.samples(duration), duration * 2);
    let buffers = push_and_pull_repeats(&mut h, original, 3);

    // Original buffer's duration was changed to match samples...
    assert_eq!(buffers[0].duration(), Some(duration), "{buffers:?}");

    // ...and repeats also use the same duration and don't leave gaps
    assert_contiguous(&buffers, true);
    assert_repeats_last(&buffers, duration);
}

#[test]
fn repeat_single_segment_rate_video() {
    // Video repeats last one frame from caps whatever the rate
    let duration = gst::ClockTime::from_mseconds(40);
    let video = TestVideo::for_duration(duration);

    for rate in [2.0, 0.5, -2.0, -0.5] {
        let mut h = livesync_harness(&video.caps(), true, rate);
        let buffers = push_and_pull_repeats(&mut h, video.frame(0), 3);

        assert_repeats_last(&buffers, duration);
    }
}

#[test]
fn late_threshold_video() {
    let duration = gst::ClockTime::from_mseconds(40);
    let video = TestVideo::for_duration(duration);

    for single_segment in [false, true] {
        let mut h = livesync_harness(&video.caps(), single_segment, 1.0);
        h.element()
            .unwrap()
            .set_property("late-threshold", LATE_THRESHOLD.nseconds());

        // Source stalls after frame 0 and livesync repeats it up to 440ms
        let mut buffers = push_and_pull_repeats(&mut h, video.frame(0), 10);
        let after_stall = buffers.len();

        // Source resumes with frames 1 to 8, all already behind livesync's output
        for n in 1..=8 {
            h.push(video.frame(n)).unwrap();
        }
        buffers.extend((0..3).map(|_| crank_pull(&mut h)));

        // Frame 7 is the first one more than late-threshold (200ms) after frame 0
        assert_eq!(
            h.element().unwrap().property::<u64>("drop"),
            7,
            "single-segment {single_segment}"
        );
        assert_buf(
            &buffers[0],
            0,
            gst::ClockTime::ZERO,
            duration,
            gst::BufferFlags::DISCONT,
        );

        // Repeats copy frame 0 until frame 7 shows up
        for (n, buf) in buffers.iter().enumerate().skip(1) {
            let offset = if n < after_stall { 0 } else { 7 };
            assert_buf(
                buf,
                offset,
                duration * n as u64,
                duration,
                gst::BufferFlags::GAP,
            );
        }
    }
}

#[test]
fn late_threshold_video_without_framerate() {
    // Without a fixed framerate, we repeat only based on frame durations,
    // so when a late frame with a new duration should make livesync
    // change its repeat duration as well.
    let video = TestVideo::for_framerate(0, 1);
    let duration = gst::ClockTime::from_mseconds(40);
    let late_duration = gst::ClockTime::from_mseconds(20);

    for single_segment in [false, true] {
        let mode = format!("single-segment {single_segment}");
        let mut h = livesync_harness(&video.caps(), single_segment, 1.0);
        h.element()
            .unwrap()
            .set_property("late-threshold", LATE_THRESHOLD.nseconds());

        // Source stalls after a 40ms frame and livesync repeats it up to 440ms
        let mut buffers = push_and_pull_repeats(&mut h, video_buffer(duration), 10);
        let after_stall = buffers.len();

        // Source resumes with 20ms frames from 40ms to 320ms, all already behind livesync's output
        for ms in (40..=300).step_by(20) {
            let pts = gst::ClockTime::from_mseconds(ms);
            h.push(with_pts(video_buffer(late_duration), pts)).unwrap();
        }
        buffers.extend((0..3).map(|_| crank_pull(&mut h)));

        // The frame at 260ms is the first more than late-threshold (200ms) after the first one,
        // so livesync starts repeating it with its new duration
        assert_contiguous(&buffers, true);
        for buf in &buffers[after_stall..] {
            assert_eq!(buf.duration(), Some(late_duration), "{mode}: {buf:?}");
        }
    }
}

#[test]
fn late_threshold_audio() {
    let audio = TestAudio::for_rate(48_000);

    for single_segment in [false, true] {
        let mut h = livesync_harness(&audio.caps(), single_segment, 1.0);
        h.element()
            .unwrap()
            .set_property("late-threshold", LATE_THRESHOLD.nseconds());

        // Source stalls after a 20ms buffer and livesync repeats it up to 320ms
        let first = audio.buffer(gst::ClockTime::from_mseconds(20));
        let mut buffers = push_and_pull_repeats(&mut h, first, 15);
        let after_stall = buffers.len();

        // Source resumes with 10ms buffers from 20ms to 260ms, all already behind livesync's output
        let duration = gst::ClockTime::from_mseconds(10);
        for ms in (20..=250).step_by(10) {
            let pts = gst::ClockTime::from_mseconds(ms);
            h.push(with_pts(audio.buffer(duration), pts)).unwrap();
        }
        buffers.extend((0..3).map(|_| crank_pull(&mut h)));

        // The buffer at 230ms is the first more than late-threshold (200ms) after the first one
        let mode = format!("single-segment {single_segment}");
        assert_eq!(h.element().unwrap().property::<u64>("drop"), 23, "{mode}");
        assert_contiguous(&buffers, true);

        // After the stall, repeats take the late buffer's length but stay silent
        for buf in &buffers[after_stall..] {
            assert!(buf.flags().contains(gst::BufferFlags::GAP), "{mode}");
            assert_eq!(buf.size(), audio.samples(duration), "{mode}");
            assert_eq!(buf.duration(), Some(duration), "{mode}");
            assert!(filled_with(buf, 0), "{mode}");
        }
    }
}

#[test]
fn video_frame_without_duration() {
    // Frames without a duration last one frame from caps
    let duration = gst::ClockTime::from_mseconds(40);
    let video = TestVideo::for_duration(duration);
    let mut h = livesync_harness(&video.caps(), false, 1.0);
    let buffers = push_and_pull_repeats(&mut h, without_duration(video.frame(0)), 3);

    assert_eq!(buffers[0].duration(), Some(duration), "{buffers:?}");
    assert_contiguous(&buffers, true);
    assert_repeats_last(&buffers, duration);
}

#[test]
fn video_frame_without_duration_or_framerate() {
    // Frames without a duration or framerate last the default duration
    let video = TestVideo::for_framerate(0, 1);
    let mut h = livesync_harness(&video.caps(), false, 1.0);
    let original = without_duration(video_buffer(gst::ClockTime::ZERO));
    let buffers = push_and_pull_repeats(&mut h, original, 3);

    assert_eq!(buffers[0].duration(), Some(DEFAULT_DURATION), "{buffers:?}");
    assert_contiguous(&buffers, true);
    assert_repeats_last(&buffers, DEFAULT_DURATION);
}

#[test]
fn audio_buffer_without_duration() {
    // Audio without a duration lasts as long as its samples
    let audio = TestAudio::for_rate(48_000);
    let mut h = livesync_harness(&audio.caps(), false, 1.0);
    let duration = gst::ClockTime::from_mseconds(10);
    let buffers = push_and_pull_repeats(&mut h, without_duration(audio.buffer(duration)), 3);

    assert_eq!(buffers[0].duration(), Some(duration), "{buffers:?}");
    assert_contiguous(&buffers, true);
    assert_repeats_last(&buffers, duration);
}
