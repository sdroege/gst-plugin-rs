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

// Output timestamps, especially repeats, should never be earlier than what an actual
// upstream source would produce (e.g. videotestsrc). Due to that, livesync rounds up,
// making frames start up to 1ns late. A buffer without a duration also gets one that's
// rounded up, which can add another 1ns. That's the max shift we tolerate in tests.
const MAX_TIMESTAMP_DELAY: gst::ClockTime = gst::ClockTime::from_nseconds(2);

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

    // Buffer n's pts with `samples` per buffer, rounded down like audiotestsrc does
    fn pts(&self, n: u64, samples: usize) -> gst::ClockTime {
        gst::ClockTime::SECOND
            .mul_div_floor(n * samples as u64, self.rate)
            .unwrap()
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

fn repeats_duration(buffers: &[gst::Buffer]) -> gst::ClockTime {
    buffers[1..].iter().map(|buf| buf.duration().unwrap()).sum()
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
fn assert_timestamps(
    buffers: &[gst::Buffer],
    expected_pts: impl Fn(u64) -> gst::ClockTime,
    mode: &str,
) {
    for (n, buf) in buffers.iter().enumerate() {
        let expected = expected_pts(n as u64);
        assert!(
            (expected..=expected + MAX_TIMESTAMP_DELAY).contains(&buf.pts().unwrap()),
            "{mode}: {buf:?} should start at {expected}"
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
    const BUF_DURATION_MS: u64 = 20;
    const BUF_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(BUF_DURATION_MS);
    const LATENCY: gst::ClockTime = BUF_DURATION;
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

    // With 48 kHz clipping SECOND_SEG_START removes whole samples,
    // otherwise we'd hit the scenario from repeat_drift_audio_clipped()
    let audio = TestAudio::for_rate(48_000);
    let caps = audio.caps().parse::<gst::Caps>().unwrap();
    src.set_caps(Some(&caps));

    let mut buf1 = gst::Buffer::from_slice(vec![1; audio.samples(BUF_DURATION)]);
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

                let mut buf2 = gst::Buffer::from_slice(vec![2; audio.samples(BUF_DURATION)]);
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
fn repeat_drift_video_30fps() {
    init();

    // 30 frames at 30 fps
    let video = TestVideo::for_framerate(30, 1);
    repeat_drift(
        &video.caps(),
        video.frame(0),
        30,
        gst::ClockTime::from_seconds(1),
    );
}

#[test]
fn repeat_drift_video_29_97fps() {
    init();

    // 30 frames at 29.97 fps
    let video = TestVideo::for_framerate(30_000, 1001);
    repeat_drift(
        &video.caps(),
        video.frame(0),
        30,
        gst::ClockTime::from_mseconds(1001),
    );
}

#[test]
fn repeat_drift_audio() {
    init();

    // 30 * 1600 samples at 48 kHz
    let audio = TestAudio::for_rate(48_000);
    let original = audio_buffer(1600, gst::ClockTime::SECOND / 30);
    repeat_drift(&audio.caps(), original, 30, gst::ClockTime::from_seconds(1));
}

#[test]
fn repeat_drift_audio_clipped() {
    init();

    // Edge case of clipping a 20ms buffer with 882 samples inside to 15ms,
    // which equals cutting off 220.5 samples, and that we can't really do
    // so we end up removing 220 instead. We're then left with a 15ms buffer
    // that has an extra half sample which over time accumulates a drift if we
    // timestamp repeated buffers based on the duration instead of sample count.
    let audio = TestAudio::for_rate(44_100);
    let original = audio_buffer(662, gst::ClockTime::from_mseconds(15));
    repeat_drift(
        &audio.caps(),
        original,
        441,
        gst::ClockTime::from_mseconds(6620),
    );
}

/// Makes sure repeated buffers' timestamps don't drift over time,
/// for example with video at 30fps with its 0.0(3)s buffer durations
fn repeat_drift(caps: &str, original: gst::Buffer, repeats: usize, expected: gst::ClockTime) {
    for (single_segment, rate) in [(false, 1.0), (true, 1.0), (false, -1.0), (true, -1.0)] {
        let mut h = livesync_harness(caps, single_segment, rate);
        let original = if rate > 0.0 {
            original.clone()
        } else {
            let duration = original.duration().unwrap();
            with_pts(original.clone(), REVERSE_STOP - duration)
        };
        let buffers = push_and_pull_repeats(&mut h, original, repeats);

        assert_contiguous(&buffers, single_segment || rate > 0.0);
        assert_eq!(
            repeats_duration(&buffers),
            expected,
            "single-segment {single_segment}, rate {rate}"
        );
    }
}

/// Makes sure repeated buffers start where videotestsrc would've placed them,
/// or at most MAX_TIMESTAMP_DELAY later due to expected rounding differences
#[test]
fn repeat_video_timing() {
    let video = TestVideo::for_framerate(30, 1);

    // Upstream (e.g. videotestsrc) rounds each frame's pts down on its own already,
    // so at 30fps the durations are 33,333,333, 33,333,333, 33,333,334.

    // The source sends frame 1 with its duration and then stops, so livesync repeats frame 1
    // in place of frames 2, 3 and 4. Each repeat must start where upstream would have put
    // that frame, never earlier, otherwise e.g. videorate will just drop those.
    // At this position, a repeat timestamp rounded down would put frame 3's repeat
    // at 99,999,999, 1ns before upstream's 100,000,000.
    let mut h = livesync_harness(&video.caps(), false, 1.0);
    let buffers = push_and_pull_repeats(&mut h, video.frame(1), 3);
    assert_timestamps(&buffers, |k| video.pts(1 + k), "frame 1");

    // The source sends frame 2 without a duration and then stops, so livesync has to guess
    // how long it lasts before repeating it in place of frames 3, 4 and 5.
    // At 30fps the usual guess is 33,333,333, which is only right at some positions:
    //   frame 0: 0 + 33,333,333 = 33,333,333, exactly upstream's frame 1
    //   frame 1: 33,333,333 + 33,333,333 = 66,666,666, exactly upstream's frame 2
    //   frame 2: 66,666,666 + 33,333,333 = 99,999,999, 1ns before upstream's frame 3
    // That's why livesync rounds the guessed duration up to 33,333,334, the same way
    // it rounds up repeats, so they always start no earlier than real buffers would.
    let mut h = livesync_harness(&video.caps(), false, 1.0);
    let buffers = push_and_pull_repeats(&mut h, without_duration(video.frame(2)), 3);
    assert_timestamps(&buffers, |k| video.pts(2 + k), "frame 2 without a duration");
}

#[test]
fn repeat_audio_timing() {
    // 1024 samples at 48kHz last 21,333,333.33ns, following the same
    // pattern as 30fps video, so this mirrors repeat_video_timing()
    let audio = TestAudio::for_rate(48_000);
    let samples = 1024;

    let mut h = livesync_harness(&audio.caps(), false, 1.0);
    let start = audio.pts(1, samples);
    let end = audio.pts(2, samples);
    let buffer = with_pts(audio_buffer(samples, end - start), start);
    let buffers = push_and_pull_repeats(&mut h, buffer, 3);
    assert_timestamps(&buffers, |k| audio.pts(1 + k, samples), "buffer 1");

    let mut h = livesync_harness(&audio.caps(), false, 1.0);
    let start = audio.pts(2, samples);
    let end = audio.pts(3, samples);
    let buffer = with_pts(audio_buffer(samples, end - start), start);
    let buffers = push_and_pull_repeats(&mut h, without_duration(buffer), 3);
    assert_timestamps(
        &buffers,
        |k| audio.pts(2 + k, samples),
        "buffer 2 without a duration",
    );
}

#[test]
fn repeat_video_timing_after_late_buffers() {
    // Accepting a new late frame without any framerate changes etc.
    // shouldn't mess up our output timing, in particular we need to be sure
    // we don't add any rounding errors when switching.
    let video = TestVideo::for_framerate(30, 1);

    for single_segment in [false, true] {
        let mode = format!("single-segment {single_segment}");
        let mut h = livesync_harness(&video.caps(), single_segment, 1.0);
        h.element()
            .unwrap()
            .set_property("late-threshold", LATE_THRESHOLD.nseconds());

        // Source stalls after frame 0 and livesync repeats it for a second
        let mut buffers = push_and_pull_repeats(&mut h, video.frame(0), 30);

        // Every 8th frame arrives late, each more than late-threshold (200ms) after the last,
        // so its contents go out with the next repeat's timing
        for n in (8..=80).step_by(8) {
            let takeover = buffers.len();
            h.push(video.frame(n)).unwrap();
            buffers.extend((0..8).map(|_| crank_pull(&mut h)));

            assert_eq!(
                buffers[takeover].offset(),
                n,
                "{mode}: late frame not output"
            );
        }

        assert_timestamps(&buffers, |k| video.pts(k), &mode);
    }
}

#[test]
fn repeat_audio_timing_after_late_buffers() {
    // Late buffers arriving with varying sample counts can't cause
    // any timestamp drift in livesync's silence output
    init();

    let audio = TestAudio::for_rate(48_000);

    // Simulates timestamps output by audiotestsrc
    let position = |sample: u64| {
        gst::ClockTime::SECOND
            .mul_div_floor(sample, audio.rate)
            .unwrap()
    };

    // Source buffers alternate between 1024 and 1000 samples
    // so that every late accepted buffer causes a change in output buffer size
    let mut offset = 0;
    let source: Vec<_> = (0..=110)
        .map(|i| {
            let samples = if i % 2 == 0 { 1024 } else { 1000 };
            let pts = position(offset);
            offset += samples as u64;
            with_pts(audio_buffer(samples, position(offset) - pts), pts)
        })
        .collect();

    for single_segment in [false, true] {
        let mode = format!("single-segment {single_segment}");
        let mut h = livesync_harness(&audio.caps(), single_segment, 1.0);
        h.element()
            .unwrap()
            .set_property("late-threshold", LATE_THRESHOLD.nseconds());

        // Source stalls after buffer 0 and livesync repeats it for a second
        let mut buffers = push_and_pull_repeats(&mut h, source[0].clone(), 47);

        // Each buffer is ~21ms, so every 11th buffer gets accepted as late,
        // because it's more than late-threshold (200ms) after the last accepted one,
        // and livesync should then use its size for any silence repeats
        for late in source.iter().skip(11).step_by(11) {
            let takeover = buffers.len();
            h.push(late.clone()).unwrap();
            buffers.extend((0..11).map(|_| crank_pull(&mut h)));

            assert_eq!(
                buffers[takeover].size(),
                late.size(),
                "{mode}: late buffer not output"
            );
        }

        // No rounding errors etc. accumulating after output size changes
        let mut samples = 0;
        let starts: Vec<_> = buffers
            .iter()
            .map(|buf| {
                let start = position(samples);
                samples += buf.size() as u64;
                start
            })
            .collect();
        assert_timestamps(&buffers, |k| starts[k as usize], &mode);
    }
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
fn audio_buffer_duration_within_one_sample() {
    // Clipping can keep a partial sample, e.g. clipping a buffer to 10.05ms
    // can still make it keep 483 samples (=10.0625ms). livesync shouldn't adjust
    // the initial buffer unless it's more than 1 sample off, but repeats
    // should be more precise to avoid accumulating drift.
    let audio = TestAudio::for_rate(48_000);
    let mut h = livesync_harness(&audio.caps(), false, 1.0);
    let duration = gst::ClockTime::from_useconds(10_050);
    let buffers = push_and_pull_repeats(&mut h, audio_buffer(483, duration), 3);

    // Original buffer is left as is...
    assert_eq!(buffers[0].duration(), Some(duration), "{buffers:?}");

    // ...while repeats are retimestamped to avoid drift...
    assert_repeats_last(&buffers, gst::ClockTime::from_nseconds(10_062_500));

    // ...and everything keeps the same amount of samples
    for buf in &buffers {
        assert_eq!(buf.size(), 483, "{buf:?}");
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
fn repeat_reverse_video() {
    // Repeats line up in reverse when they last longer or shorter than the frame
    for duration in [MINIMUM_DURATION / 2, MAXIMUM_DURATION * 2] {
        let video = TestVideo::for_duration(duration);
        let mut h = livesync_harness(&video.caps(), false, -1.0);
        let original = with_pts(video.frame(0), REVERSE_STOP - duration);
        let buffers = push_and_pull_repeats(&mut h, original, 2);

        assert_contiguous(&buffers, false);
    }
}

#[test]
fn repeat_reverse_audio() {
    // Repeats line up in reverse when they last longer or shorter than the buffer
    let audio = TestAudio::for_rate(48_000);

    for duration in [MINIMUM_DURATION / 2, MAXIMUM_DURATION * 2] {
        let mut h = livesync_harness(&audio.caps(), false, -1.0);
        let original = with_pts(audio.buffer(duration), REVERSE_STOP - duration);
        let buffers = push_and_pull_repeats(&mut h, original, 2);

        assert_contiguous(&buffers, false);
    }
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
fn repeat_single_segment_rate_audio() {
    // Repeats last as long as the buffer at the segment rate and hold exactly that many samples
    let audio = TestAudio::for_rate(48_000);

    for (rate, repeat_duration) in [
        (2.0, gst::ClockTime::from_mseconds(10)),
        (0.5, gst::ClockTime::from_mseconds(40)),
        (-2.0, gst::ClockTime::from_mseconds(10)),
        (-0.5, gst::ClockTime::from_mseconds(40)),
    ] {
        let mut h = livesync_harness(&audio.caps(), true, rate);
        let duration = gst::ClockTime::from_mseconds(20);
        let buffers = push_and_pull_repeats(&mut h, audio.buffer(duration), 3);

        assert_repeats_last(&buffers, repeat_duration);
        // FIXME: Skip the original buffer since livesync doesn't resample yet
        for buf in &buffers[1..] {
            assert_eq!(buf.size(), audio.samples(repeat_duration), "rate {rate}");
        }
    }
}

#[test]
fn repeat_single_segment_rate_audio_limits() {
    // Checks whether duration limits take the rate into account
    let audio = TestAudio::for_rate(48_000);

    for (rate, duration, repeat_duration) in [
        // Below the minimum before the rate but within limits after it
        (0.5, MINIMUM_DURATION * 3 / 4, MINIMUM_DURATION * 3 / 2),
        // Within limits before the rate but below the minimum after it
        (2.0, MINIMUM_DURATION * 3 / 2, DEFAULT_DURATION),
        // Above the maximum before the rate but within limits after it
        (2.0, MAXIMUM_DURATION * 3 / 2, MAXIMUM_DURATION * 3 / 4),
        // Within limits before the rate but above the maximum after it
        (0.5, MAXIMUM_DURATION * 3 / 4, DEFAULT_DURATION),
    ] {
        let mut h = livesync_harness(&audio.caps(), true, rate);
        let buffers = push_and_pull_repeats(&mut h, audio.buffer(duration), 3);

        for buf in &buffers[1..] {
            assert_eq!(
                buf.size(),
                audio.samples(repeat_duration),
                "{duration} at rate {rate}"
            );
        }
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
