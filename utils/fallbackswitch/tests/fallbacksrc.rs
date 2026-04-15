// Copyright (C) 2026 Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::debug;
use gst::glib;
use gst::prelude::*;

use std::str::FromStr;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

static TEST_CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "fallbacksrc-test",
        gst::DebugColorFlags::empty(),
        Some("fallbacksrc test"),
    )
});

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstfallbackswitch::plugin_register_static().expect("gstfallbacksrc test");
    });
}

const WAIT_DEADLINE: Duration = Duration::from_secs(5);

macro_rules! assert_fallback_buffer {
    ($buffer:expr) => {
        assert_eq!($buffer.size(), 160 * 120 * 4);
    };
}

macro_rules! assert_buffer {
    ($buffer:expr) => {
        assert_eq!($buffer.size(), 320 * 240 * 4);
    };
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

fn disable_source(pipeline: &gst::Pipeline, is_fallback: bool) {
    let valve = pipeline
        .by_name(format!("valve-{}", if is_fallback { "fallback" } else { "main" }).as_str())
        .unwrap();
    valve.set_property("drop", true);
}

fn source_bin(drop: bool, width: i32, height: i32, is_fallback: bool) -> gst::Bin {
    let src = gst::ElementFactory::make("videotestsrc")
        .name("src")
        .property("is-live", true)
        .build()
        .unwrap();

    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::from_str(
                format!("video/x-raw,format=ARGB,width={width},height={height}").as_str(),
            )
            .unwrap(),
        )
        .build()
        .unwrap();

    let valve = gst::ElementFactory::make("valve")
        .name(format!("valve-{}", if is_fallback { "fallback" } else { "main" }).as_str())
        .property("drop", drop)
        .build()
        .unwrap();

    let bin = gst::Bin::with_name(
        format!("src-{}", if is_fallback { "fallback" } else { "main" }).as_str(),
    );

    bin.add(&src).unwrap();
    bin.add(&caps).unwrap();
    bin.add(&valve).unwrap();

    gst::Element::link_many([&src, &caps, &valve]).unwrap();

    let srcpad = valve.static_pad("src").unwrap();

    let srcpad = gst::GhostPad::builder(gst::PadDirection::Src)
        .with_target(&srcpad)
        .unwrap()
        .name("src")
        .build();
    bin.add_pad(&srcpad).unwrap();

    bin
}

fn setup_pipeline(drop_src: bool, with_live_fallback: bool, immediate_fallback: bool) -> Pipeline {
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

    let src = source_bin(drop_src, 320, 240, false);

    let source = gst::ElementFactory::make("fallbacksrc")
        .name("source")
        .property("immediate-fallback", immediate_fallback)
        .property("source", src)
        .property("enable-dummy", false)
        .property("enable-audio", false)
        .property("timeout", gst::ClockTime::from_seconds(5))
        .property("restart-timeout", gst::ClockTime::from_seconds(3600))
        .build()
        .unwrap();

    source.connect_pad_added(glib::clone!(
        #[weak]
        pipeline,
        move |_, src_pad| {
            let caps = src_pad
                .stream()
                .and_then(|stream| stream.caps())
                .expect("Expect stream caps to be valid here");

            debug!(TEST_CAT, "{} added with caps {caps:?}", src_pad.name());

            let sink = gst_app::AppSink::builder().sync(false).name("sink").build();
            pipeline.add(&sink).unwrap();

            let sink_pad = sink.static_pad("sink").unwrap();

            src_pad.link(&sink_pad).unwrap();
            sink.sync_state_with_parent().unwrap();
        }
    ));

    pipeline.add(&source).unwrap();

    if with_live_fallback {
        let fallback_src = source_bin(false, 160, 120, true);
        source.set_property("fallback-source", fallback_src.upcast_ref::<gst::Element>());
    }

    pipeline.set_state(gst::State::Playing).unwrap();

    Pipeline { pipeline, clock }
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

fn stop_pipeline(pipeline: Pipeline) {
    pipeline.pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_main_without_fallback() {
    let pipeline = setup_pipeline(false, false, false);
    advance_clock(&pipeline, gst::ClockTime::ZERO);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);
    advance_clock(&pipeline, 1.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);
    advance_clock(&pipeline, 2.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);
    advance_clock(&pipeline, 3.seconds());

    stop_pipeline(pipeline);
}

#[test]
fn test_fallback_without_main() {
    let pipeline = setup_pipeline(true, true, true);
    advance_clock(&pipeline, gst::ClockTime::ZERO);

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    advance_clock(&pipeline, 1.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    advance_clock(&pipeline, 2.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    advance_clock(&pipeline, 3.seconds());

    stop_pipeline(pipeline);
}

#[test]
fn test_main_with_fallback() {
    let pipeline = setup_pipeline(false, true, false);
    advance_clock(&pipeline, gst::ClockTime::ZERO);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);
    advance_clock(&pipeline, 1.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);
    advance_clock(&pipeline, 2.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);
    advance_clock(&pipeline, 3.seconds());

    stop_pipeline(pipeline);
}

#[test]
fn test_main_with_drops_with_fallback() {
    let pipeline = setup_pipeline(false, true, false);
    advance_clock(&pipeline, gst::ClockTime::ZERO);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);

    disable_source(&pipeline, false);
    // The 5s timeout configured in setup_pipeline() expires before this.
    advance_clock(&pipeline, 7.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    advance_clock(&pipeline, 8.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    advance_clock(&pipeline, 9.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    advance_clock(&pipeline, 10.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    advance_clock(&pipeline, 11.seconds());

    stop_pipeline(pipeline);
}
