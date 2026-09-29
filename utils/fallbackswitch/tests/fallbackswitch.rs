// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2021 Jan Schmidt <jan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::debug;
use gst::prelude::*;

use std::sync::LazyLock;
use std::time::{Duration, Instant};

const LATENCY: gst::ClockTime = gst::ClockTime::from_mseconds(10);

const WAIT_DEADLINE: Duration = Duration::from_secs(5);

static TEST_CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "fallbackswitch-test",
        gst::DebugColorFlags::empty(),
        Some("fallbackswitch test"),
    )
});

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstfallbackswitch::plugin_register_static().expect("gstfallbackswitch test");
    });
}

macro_rules! assert_fallback_buffer {
    ($buffer:expr, $ts:expr) => {
        assert_eq!($buffer.pts(), $ts);
        assert_eq!($buffer.size(), 160 * 120 * 4);
    };
}

macro_rules! assert_buffer {
    ($buffer:expr, $ts:expr) => {
        assert_eq!($buffer.pts(), $ts);
        assert_eq!($buffer.size(), 320 * 240 * 4);
    };
}

#[test]
fn test_no_fallback_no_drops() {
    let pipeline = setup_pipeline(None, None, None);

    push_buffer(&pipeline, gst::ClockTime::ZERO);
    advance_clock(&pipeline, gst::ClockTime::ZERO + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::ZERO));

    push_buffer(&pipeline, 1.seconds());
    advance_clock(&pipeline, 1.seconds() + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::SECOND));

    push_buffer(&pipeline, 2.seconds());
    advance_clock(&pipeline, 2.seconds() + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(2.seconds()));

    push_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_no_drops_live() {
    test_no_drops(true);
}

#[test]
fn test_no_drops_not_live() {
    test_no_drops(false);
}

fn test_no_drops(live: bool) {
    let pipeline = setup_pipeline(Some(live), None, None);

    push_buffer(&pipeline, gst::ClockTime::ZERO);
    push_fallback_buffer(&pipeline, gst::ClockTime::ZERO);
    advance_clock(&pipeline, gst::ClockTime::ZERO + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::ZERO));

    push_fallback_buffer(&pipeline, 1.seconds());
    push_buffer(&pipeline, 1.seconds());
    advance_clock(&pipeline, 1.seconds() + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::SECOND));

    push_buffer(&pipeline, 2.seconds());
    push_fallback_buffer(&pipeline, 2.seconds());
    advance_clock(&pipeline, 2.seconds() + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(2.seconds()));

    // EOS on the fallback should not be required
    push_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_no_drops_but_no_fallback_frames_live() {
    test_no_drops_but_no_fallback_frames(true);
}

#[test]
fn test_no_drops_but_no_fallback_frames_not_live() {
    test_no_drops_but_no_fallback_frames(false);
}

fn test_no_drops_but_no_fallback_frames(live: bool) {
    let pipeline = setup_pipeline(Some(live), None, None);

    push_buffer(&pipeline, gst::ClockTime::ZERO);
    // +10ms needed here because the immediate timeout will be always at running time 0, but
    // aggregator also adds the latency to it so we end up at 10ms instead.
    advance_clock(&pipeline, LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::ZERO));

    push_buffer(&pipeline, 1.seconds());
    advance_clock(&pipeline, 1.seconds() + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::SECOND));

    push_buffer(&pipeline, 2.seconds());
    advance_clock(&pipeline, 2.seconds() + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(2.seconds()));

    // EOS on the fallback should not be required
    push_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_short_drop_live() {
    test_short_drop(true);
}

#[test]
fn test_short_drop_not_live() {
    test_short_drop(false);
}

fn test_short_drop(live: bool) {
    let pipeline = setup_pipeline(Some(live), None, None);

    push_buffer(&pipeline, gst::ClockTime::ZERO);
    push_fallback_buffer(&pipeline, gst::ClockTime::ZERO);
    advance_clock(&pipeline, gst::ClockTime::ZERO + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::ZERO));

    // Main has no frame at 1s, but the fallback timeout has not expired
    // yet. The fallback 1s frame must stay parked on its inactive-pad
    // wait instead of being output.
    push_fallback_buffer(&pipeline, 1.seconds());
    advance_clock(&pipeline, 1.seconds() + 10.mseconds());

    push_fallback_buffer(&pipeline, 2.seconds());
    push_buffer(&pipeline, 2.seconds());
    advance_clock(&pipeline, 2.seconds() + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(2.seconds()));

    push_eos(&pipeline);
    push_fallback_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_long_drop_and_eos_live() {
    test_long_drop_and_eos(true);
}

#[test]
fn test_long_drop_and_eos_not_live() {
    test_long_drop_and_eos(false);
}

fn test_long_drop_and_eos(live: bool) {
    let pipeline = setup_pipeline(Some(live), None, None);

    // Produce the first frame
    push_buffer(&pipeline, gst::ClockTime::ZERO);
    push_fallback_buffer(&pipeline, gst::ClockTime::ZERO);
    advance_clock(&pipeline, gst::ClockTime::ZERO + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::ZERO));

    // Produce a second frame but only from the fallback source
    push_fallback_buffer(&pipeline, 1.seconds());
    advance_clock(&pipeline, 1.seconds() + 10.mseconds());

    // Produce a third frame but only from the fallback source
    push_fallback_buffer(&pipeline, 2.seconds());
    advance_clock(&pipeline, 2.seconds() + 10.mseconds());

    // Produce a fourth frame but only from the fallback source
    // The timeout has expired, so this frame is trailing and dropped
    push_fallback_buffer(&pipeline, 3.seconds());
    advance_clock(&pipeline, 3.seconds() + 10.mseconds());

    // Produce a fifth frame but only from the fallback source
    // This should be output now
    push_fallback_buffer(&pipeline, 4.seconds());
    advance_clock(&pipeline, 4.seconds() + 10.mseconds());
    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer, Some(4.seconds()));

    // Wait for EOS to arrive at appsink
    push_eos(&pipeline);
    push_fallback_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_long_drop_and_recover_live() {
    test_long_drop_and_recover(true);
}

#[test]
fn test_long_drop_and_recover_not_live() {
    test_long_drop_and_recover(false);
}

fn test_long_drop_and_recover(live: bool) {
    let pipeline = setup_pipeline(Some(live), None, None);
    let switch = pipeline.by_name("switch").unwrap();
    let mainsink = switch.static_pad("sink_0").unwrap();

    // Produce the first frame
    push_buffer(&pipeline, gst::ClockTime::ZERO);
    push_fallback_buffer(&pipeline, gst::ClockTime::ZERO);
    advance_clock(&pipeline, gst::ClockTime::ZERO + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::ZERO));
    assert!(mainsink.property::<bool>("is-healthy"));

    // Produce a second frame but only from the fallback source
    push_fallback_buffer(&pipeline, 1.seconds());
    advance_clock(&pipeline, 1.seconds() + 10.mseconds());

    // Produce a third frame but only from the fallback source
    push_fallback_buffer(&pipeline, 2.seconds());
    advance_clock(&pipeline, 2.seconds() + 10.mseconds());

    // Produce a fourth frame but only from the fallback source
    // The timeout has expired, so this frame is trailing and dropped
    push_fallback_buffer(&pipeline, 3.seconds());
    advance_clock(&pipeline, 3.seconds() + 10.mseconds());

    // Produce a fifth frame but only from the fallback source
    // This should be output now
    push_fallback_buffer(&pipeline, 4.seconds());
    advance_clock(&pipeline, 4.seconds() + 10.mseconds());
    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer, Some(4.seconds()));

    // Produce a sixth frame from the normal source, which
    // will make it healthy again
    push_buffer(&pipeline, 5.seconds());
    advance_clock(&pipeline, 5.seconds() + 10.mseconds());
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(5.seconds()));
    assert!(mainsink.property::<bool>("is-healthy"));
    drop(mainsink);
    drop(switch);

    // Produce a seventh frame from the normal source but no fallback.
    // This should still be output immediately
    push_buffer(&pipeline, 6.seconds());
    advance_clock(&pipeline, 6.seconds() + 10.mseconds());
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(6.seconds()));

    // Produce a eight frame from the normal source
    push_buffer(&pipeline, 7.seconds());
    push_fallback_buffer(&pipeline, 7.seconds());
    advance_clock(&pipeline, 7.seconds() + 10.mseconds());
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(7.seconds()));

    // Wait for EOS to arrive at appsink
    push_eos(&pipeline);
    push_fallback_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_initial_timeout_live() {
    test_initial_timeout(true);
}

#[test]
fn test_initial_timeout_not_live() {
    test_initial_timeout(false);
}

fn test_initial_timeout(live: bool) {
    let pipeline = setup_pipeline(Some(live), None, None);

    // Produce the first frame but only from the fallback source
    push_fallback_buffer(&pipeline, gst::ClockTime::ZERO);
    advance_clock(&pipeline, gst::ClockTime::ZERO + LATENCY);

    // Produce a second frame but only from the fallback source
    push_fallback_buffer(&pipeline, 1.seconds());
    advance_clock(&pipeline, 1.seconds() + 10.mseconds());

    // Produce a third frame but only from the fallback source
    push_fallback_buffer(&pipeline, 2.seconds());
    advance_clock(&pipeline, 2.seconds() + 10.mseconds());

    // Produce a fourth frame but only from the fallback source
    // The timeout has expired, so this frame is trailing and dropped
    push_fallback_buffer(&pipeline, 3.seconds());
    advance_clock(&pipeline, 3.seconds() + 10.mseconds());

    // Produce a fifth frame but only from the fallback source
    // This should be output now
    push_fallback_buffer(&pipeline, 4.seconds());
    advance_clock(&pipeline, 4.seconds() + 10.mseconds());
    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer, Some(4.seconds()));

    // Wait for EOS to arrive at appsink
    push_eos(&pipeline);
    push_fallback_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_immediate_fallback_live() {
    test_immediate_fallback(true);
}

#[test]
fn test_immediate_fallback_not_live() {
    test_immediate_fallback(false);
}

fn test_immediate_fallback(live: bool) {
    let pipeline = setup_pipeline(Some(live), Some(true), None);

    // Produce the first frame but only from the fallback source
    push_fallback_buffer(&pipeline, gst::ClockTime::ZERO);
    advance_clock(&pipeline, gst::ClockTime::ZERO + LATENCY);

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer, Some(gst::ClockTime::ZERO));

    // Wait for EOS to arrive at appsink
    push_eos(&pipeline);
    push_fallback_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_manual_switch_live() {
    test_manual_switch(true);
}

#[test]
fn test_manual_switch_not_live() {
    test_manual_switch(false);
}

fn test_manual_switch(live: bool) {
    let pipeline = setup_pipeline(Some(live), None, Some(false));
    let switch = pipeline.by_name("switch").unwrap();
    let mainsink = switch.static_pad("sink_0").unwrap();
    let fallbacksink = switch.static_pad("sink_1").unwrap();

    switch.set_property("active-pad", &mainsink);
    push_buffer(&pipeline, gst::ClockTime::ZERO);
    push_fallback_buffer(&pipeline, gst::ClockTime::ZERO);
    advance_clock(&pipeline, gst::ClockTime::ZERO + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::ZERO));

    wait_for_parked_buffer(&pipeline);

    switch.set_property("active-pad", &fallbacksink);
    push_fallback_buffer(&pipeline, 1.seconds());
    push_buffer(&pipeline, 1.seconds());
    advance_clock(&pipeline, 1.seconds() + LATENCY);

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer, Some(gst::ClockTime::ZERO));
    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer, Some(gst::ClockTime::SECOND));

    wait_for_parked_buffer(&pipeline);

    switch.set_property("active-pad", &mainsink);
    push_buffer(&pipeline, 2.seconds());
    push_fallback_buffer(&pipeline, 2.seconds());
    advance_clock(&pipeline, 2.seconds() + LATENCY);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::SECOND));
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(2.seconds()));

    drop(mainsink);
    drop(fallbacksink);
    drop(switch);
    // EOS on the fallback should not be required
    push_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_buffer_with_incorrect_timestamp_dropped() {
    let pipeline = setup_pipeline(None, None, None);

    push_buffer(&pipeline, gst::ClockTime::ZERO);
    advance_clock(&pipeline, LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::ZERO));

    push_buffer_with_incorrect_timestamp(&pipeline);

    push_buffer(&pipeline, 1.seconds());
    advance_clock(&pipeline, 1.seconds() + LATENCY);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::SECOND));

    push_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_buffer_with_incorrect_timestamp_pause_play() {
    let pipeline = setup_pipeline(None, None, None);

    push_buffer(&pipeline, gst::ClockTime::ZERO);
    advance_clock(&pipeline, LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(gst::ClockTime::ZERO));

    push_buffer_with_incorrect_timestamp(&pipeline);

    advance_clock(&pipeline, 4.seconds());

    let deadline = Instant::now() + WAIT_DEADLINE;
    loop {
        pipeline.set_state(gst::State::Paused).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        if try_push_buffer(&pipeline, 5.seconds(), Duration::from_millis(100)) {
            break;
        }

        assert!(
            Instant::now() < deadline,
            "The appsrc did not resume within {WAIT_DEADLINE:?}: pipeline state {:?}",
            pipeline.state(gst::ClockTime::ZERO),
        );
    }

    advance_clock(&pipeline, 5.seconds() + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(5.seconds()));

    push_buffer(&pipeline, 6.seconds());
    advance_clock(&pipeline, 6.seconds() + LATENCY);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, Some(6.seconds()));

    push_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

struct Pipeline {
    pipeline: gst::Pipeline,
    clock: gst_check::TestClock,
}

impl std::ops::Deref for Pipeline {
    type Target = gst::Pipeline;

    fn deref(&self) -> &gst::Pipeline {
        &self.pipeline
    }
}

fn setup_pipeline(
    with_live_fallback: Option<bool>,
    immediate_fallback: Option<bool>,
    auto_switch: Option<bool>,
) -> Pipeline {
    init();

    debug!(TEST_CAT, "Setting up pipeline");

    let clock = gst_check::TestClock::new();

    // basesrc computes first buffer's running time as clock time minus
    // base time on the first buffer.
    clock.set_time(gst::ClockTime::SECOND);
    let pipeline = gst::Pipeline::default();

    pipeline.use_clock(Some(&clock));
    pipeline.set_base_time(gst::ClockTime::SECOND);
    pipeline.set_start_time(gst::ClockTime::NONE);

    let src = gst_app::AppSrc::builder()
        .name("src")
        .is_live(true)
        .format(gst::Format::Time)
        .min_latency(LATENCY.nseconds() as i64)
        .caps(
            &gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Argb)
                .width(320)
                .height(240)
                .framerate((0, 1).into())
                .build(),
        )
        .build();

    let switch = gst::ElementFactory::make("fallbackswitch")
        .name("switch")
        .property("timeout", 3.seconds())
        .property_if_some("immediate-fallback", immediate_fallback)
        .property_if_some("auto-switch", auto_switch)
        .build()
        .unwrap();

    let sink = gst_app::AppSink::builder().name("sink").sync(false).build();

    let queue = gst::ElementFactory::make("queue").build().unwrap();

    pipeline
        .add_many([src.upcast_ref(), &switch, &queue, sink.upcast_ref()])
        .unwrap();
    src.link_pads(Some("src"), &switch, Some("sink_0")).unwrap();
    switch.link_pads(Some("src"), &queue, Some("sink")).unwrap();
    queue.link_pads(Some("src"), &sink, Some("sink")).unwrap();

    let sink_pad = switch.static_pad("sink_0").unwrap();
    sink_pad.set_property("priority", 0u32);

    if let Some(live) = with_live_fallback {
        let fallback_src = gst_app::AppSrc::builder()
            .name("fallback-src")
            .is_live(live)
            .format(gst::Format::Time)
            .min_latency(LATENCY.nseconds() as i64)
            .caps(
                &gst_video::VideoCapsBuilder::new()
                    .format(gst_video::VideoFormat::Argb)
                    .width(160)
                    .height(120)
                    .framerate((0, 1).into())
                    .build(),
            )
            .build();

        pipeline.add(&fallback_src).unwrap();

        fallback_src
            .link_pads(Some("src"), &switch, Some("sink_1"))
            .unwrap();
        let sink_pad = switch.static_pad("sink_1").unwrap();
        sink_pad.set_property("priority", 1u32);
    }

    pipeline.set_state(gst::State::Playing).unwrap();

    // Query the latency once before any buffer is pushed so that
    // fallbackswitch's upstream latency is known when the first
    // timeout is armed.
    let sink = pipeline.by_name("sink").unwrap();
    let sink_pad = sink.static_pad("sink").unwrap();
    let mut latency_query = gst::query::Latency::new();
    assert!(sink_pad.peer_query(&mut latency_query));

    Pipeline { pipeline, clock }
}

// See gsttestclock.c and gst_test_clock_process_next_clock_id.
fn drain_clock(pipeline: &Pipeline) -> usize {
    let mut processed = 0;

    while pipeline.clock.process_next_clock_id().is_some() {
        processed += 1;
    }

    processed
}

fn advance_clock(pipeline: &Pipeline, time: gst::ClockTime) {
    debug!(TEST_CAT, "Setting time to {}", time);

    pipeline.clock.set_time(gst::ClockTime::SECOND + time);
    let processed = drain_clock(pipeline);

    debug!(TEST_CAT, "Processed {processed} pending clock entries");
}

// The pending count must be exactly 2. The parked buffer and the switch
// timeout.
fn wait_for_parked_buffer(pipeline: &Pipeline) {
    let deadline = Instant::now() + WAIT_DEADLINE;
    let mut previous_count = None;

    loop {
        let pending = pipeline.clock.peek_id_count();

        if previous_count == Some(pending) {
            if pending == 2 {
                return;
            }

            if pending > 2 {
                panic!(
                    "Unexpected pending clock entries ({pending}) while waiting for the buffer \
                     to park: clock at {}, next entry at {:?}",
                    pipeline.clock.time(),
                    pipeline.clock.next_entry_time(),
                );
            }
        }

        if Instant::now() >= deadline {
            panic!(
                "Buffer did not park within {WAIT_DEADLINE:?}: clock at {}, \
                 pending clock entries: {pending}, next entry at {:?}",
                pipeline.clock.time(),
                pipeline.clock.next_entry_time(),
            );
        }

        previous_count = Some(pending);
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn push_buffer(pipeline: &Pipeline, time: gst::ClockTime) {
    assert!(
        try_push_buffer(pipeline, time, WAIT_DEADLINE),
        "Pushing the buffer at {time} failed: the appsrc is still flushing: \
         pipeline state {:?}",
        pipeline.state(gst::ClockTime::ZERO),
    );
}

fn try_push_buffer(pipeline: &Pipeline, time: gst::ClockTime, timeout: Duration) -> bool {
    let src = pipeline
        .by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    let mut buffer = gst::Buffer::with_size(320 * 240 * 4).unwrap();
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(time);
    }

    let deadline = Instant::now() + timeout;
    loop {
        match src.push_buffer(buffer.clone()) {
            Ok(_) => return true,
            Err(gst::FlowError::Flushing) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(gst::FlowError::Flushing) => return false,
            Err(err) => panic!(
                "Pushing the buffer at {time} failed: {err:?}: pipeline state {:?}",
                pipeline.state(gst::ClockTime::ZERO),
            ),
        }
    }
}

fn push_buffer_with_incorrect_timestamp(pipeline: &Pipeline) {
    let src = pipeline
        .by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    let mut buffer = gst::Buffer::with_size(320 * 240 * 4).unwrap();
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(gst::ClockTime::MAX - gst::ClockTime::SECOND);
        buffer.set_duration(2 * gst::ClockTime::SECOND);
    }

    src.push_buffer(buffer).unwrap();
}

fn push_fallback_buffer(pipeline: &Pipeline, time: gst::ClockTime) {
    let src = pipeline
        .by_name("fallback-src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    let mut buffer = gst::Buffer::with_size(160 * 120 * 4).unwrap();
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(time);
    }
    src.push_buffer(buffer).unwrap();
}

fn push_eos(pipeline: &Pipeline) {
    let src = pipeline
        .by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    src.end_of_stream().unwrap();
}

fn push_fallback_eos(pipeline: &Pipeline) {
    let src = pipeline
        .by_name("fallback-src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    src.end_of_stream().unwrap();
}

fn pull_buffer(pipeline: &Pipeline) -> gst::Buffer {
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();

    let deadline = Instant::now() + WAIT_DEADLINE;

    loop {
        if let Some(sample) = sink.try_pull_sample(5.mseconds()) {
            return sample.buffer_owned().unwrap();
        }

        if Instant::now() >= deadline {
            panic!(
                "No buffer arrived within {WAIT_DEADLINE:?}: clock at {}, \
                 pending clock entries: {}, next entry at {:?}",
                pipeline.clock.time(),
                pipeline.clock.peek_id_count(),
                pipeline.clock.next_entry_time(),
            );
        }

        drain_clock(pipeline);
    }
}

fn wait_eos(pipeline: &Pipeline) {
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();

    let deadline = Instant::now() + WAIT_DEADLINE;

    loop {
        if sink.is_eos() {
            debug!(TEST_CAT, "Waited for EOS");
            break;
        }

        if Instant::now() >= deadline {
            panic!(
                "EOS did not arrive within {WAIT_DEADLINE:?}: clock at {}, \
                 pending clock entries: {}, next entry at {:?}",
                pipeline.clock.time(),
                pipeline.clock.peek_id_count(),
                pipeline.clock.next_entry_time(),
            );
        }

        drain_clock(pipeline);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn stop_pipeline(pipeline: Pipeline) {
    pipeline.pipeline.set_state(gst::State::Null).unwrap();

    let switch = pipeline.by_name("switch").unwrap();
    let switch_weak = switch.downgrade();
    drop(switch);
    let pipeline_weak = pipeline.downgrade();

    drop(pipeline);

    assert!(switch_weak.upgrade().is_none());
    assert!(pipeline_weak.upgrade().is_none());
}
