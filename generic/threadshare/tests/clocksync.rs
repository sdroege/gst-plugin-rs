// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use gstthreadshare::runtime::{Context, executor::block_on};

use std::sync::LazyLock;
use std::time::Duration;

const BUFFER_INTERVAL: gst::ClockTime = gst::ClockTime::from_mseconds(50);

pub static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ts-clocksync-test",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing clocksync Test"),
    )
});

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare clocksync test");
    });
}

#[track_caller]
fn push_buffer(appsrc: &gst::Element, clock_time: gst::ClockTime, buffer_ts: gst::ClockTime) {
    let mut buf = gst::Buffer::from_slice([0, 1, 2, 3]);
    let buf_mut = buf.get_mut().unwrap();
    buf_mut.set_pts(buffer_ts);

    gst::info!(CAT, "Pushing buffer, pts {buffer_ts} @ {clock_time}");
    // that's unfortunate...
    if appsrc.type_() == gst_app::AppSrc::static_type() {
        assert_eq!(
            appsrc.emit_by_name::<gst::FlowReturn>("push-buffer", &[&buf]),
            gst::FlowReturn::Ok
        );
    } else {
        assert!(appsrc.emit_by_name::<bool>("push-buffer", &[&buf]));
    }
    gst::info!(CAT, "Buffer {buffer_ts} pushed");
}

#[test]
// Because it involves timing, this test doesn't play well on CI
#[ignore]
fn clocksync_async() {
    const CONTEXT: &str = "test-clocksync-async";
    const MAX_THROTLING_DUR: gst::ClockTime = BUFFER_INTERVAL;

    init();

    let mut h = gst_check::Harness::new_parse(&format!(
        "
        ts-appsrc context={} context-wait={} caps=foo/bar name=appsrc
        ! ts-clocksync name=clocksync
        ",
        CONTEXT,
        MAX_THROTLING_DUR.mseconds(),
    ));
    let bus = gst::Bus::new();
    h.element().unwrap().set_bus(Some(&bus));
    h.play();
    // Context initialized by the pipeline
    let context =
        Context::acquire(CONTEXT, Duration::from_nanos(MAX_THROTLING_DUR.nseconds())).unwrap();
    let clock = h.testclock().unwrap();
    let mut clock_time = gst::ClockTime::ZERO;
    clock.set_time(clock_time);

    while let Some(_msg) = bus.timed_pop(gst::ClockTime::ZERO) {}

    let bin = h.element().unwrap();
    let bin = bin.downcast_ref::<gst::Bin>().unwrap();
    let appsrc = bin.by_name("appsrc").unwrap();

    gst::info!(CAT, "next: first buffer");
    let mut buffer_ts = gst::ClockTime::ZERO;
    push_buffer(&appsrc, clock_time, buffer_ts);

    assert_eq!(h.pull_event().unwrap().type_(), gst::EventType::StreamStart);
    assert_eq!(h.pull_event().unwrap().type_(), gst::EventType::Caps);
    assert_eq!(h.pull_event().unwrap().type_(), gst::EventType::Segment);

    let msg = bus.timed_pop(None).unwrap();
    assert_eq!(msg.type_(), gst::MessageType::Latency);
    let clocksync = bin.by_name("clocksync").unwrap();
    assert_eq!(*msg.src().unwrap(), clocksync);
    gst::info!(CAT, "Got latency event");

    gst::info!(CAT, "Waiting for buffer {buffer_ts}");
    let buf = h.pull().unwrap();
    assert_eq!(buf.pts(), Some(buffer_ts));
    gst::info!(CAT, "Got buffer {buffer_ts}");

    gst::info!(CAT, "next: early buffer => will sync");
    buffer_ts += BUFFER_INTERVAL;
    push_buffer(&appsrc, clock_time, buffer_ts);
    gst::info!(
        CAT,
        "waking the scheduler up to ensure buffer is being handled"
    );
    block_on(context.spawn_and_unpark(std::future::ready(()))).unwrap();
    // not available yet (also means the scheduler was not blocked)
    assert!(h.try_pull().is_none());

    gst::info!(CAT, "Waiting for buffer {buffer_ts}");
    let buf = h.pull().unwrap();
    assert_eq!(buf.pts(), Some(buffer_ts));
    gst::info!(CAT, "Got buffer {buffer_ts}");

    gst::info!(CAT, "next: late buffer => available immediately");
    clock_time += 2 * BUFFER_INTERVAL;
    clock.set_time(clock_time);
    buffer_ts += BUFFER_INTERVAL;
    push_buffer(&appsrc, clock_time, buffer_ts);

    gst::info!(
        CAT,
        "waking the scheduler up to ensure buffer is being handled"
    );
    block_on(context.spawn_and_unpark(std::future::ready(()))).unwrap();
    // buffer available immediately (after the scheduler wakes up)
    gst::info!(CAT, "Pulling buffer {buffer_ts}");
    let buf = h.try_pull().unwrap();
    assert_eq!(buf.pts(), Some(buffer_ts));
    gst::info!(CAT, "Got buffer {buffer_ts}");
}

#[test]
// Because it involves timing, this test doesn't play well on CI
#[ignore]
fn clocksync_sync() {
    init();

    let mut h = gst_check::Harness::new_parse(
        "
        appsrc caps=foo/bar name=appsrc format=time
        ! ts-clocksync name=clocksync
        ",
    );
    let bus = gst::Bus::new();
    h.element().unwrap().set_bus(Some(&bus));
    h.play();
    let clock = h.testclock().unwrap();
    let mut clock_time = gst::ClockTime::ZERO;
    clock.set_time(clock_time);

    while let Some(_msg) = bus.timed_pop(gst::ClockTime::ZERO) {}

    let bin = h.element().unwrap();
    let bin = bin.downcast_ref::<gst::Bin>().unwrap();
    let appsrc = bin.by_name("appsrc").unwrap();

    gst::info!(CAT, "next: first buffer");
    let mut buffer_ts = gst::ClockTime::ZERO;
    push_buffer(&appsrc, clock_time, buffer_ts);

    assert_eq!(h.pull_event().unwrap().type_(), gst::EventType::StreamStart);
    assert_eq!(h.pull_event().unwrap().type_(), gst::EventType::Caps);
    assert_eq!(h.pull_event().unwrap().type_(), gst::EventType::Segment);

    // Sometimes we get the StreamStatus message here
    let mut msg = bus.timed_pop(None).unwrap();
    let msg_type = msg.type_();
    if msg_type != gst::MessageType::Latency {
        msg = bus.timed_pop(None).unwrap();
        assert_eq!(msg.type_(), gst::MessageType::Latency);
    }
    let clocksync = bin.by_name("clocksync").unwrap();
    assert_eq!(*msg.src().unwrap(), clocksync);
    gst::info!(CAT, "Got latency event");

    gst::info!(CAT, "Waiting for buffer {buffer_ts}");
    let buf = h.pull().unwrap();
    assert_eq!(buf.pts(), Some(buffer_ts));
    gst::info!(CAT, "Got buffer {buffer_ts}");

    gst::info!(CAT, "next: early buffer => will sync");
    buffer_ts += BUFFER_INTERVAL;
    push_buffer(&appsrc, clock_time, buffer_ts);
    gst::info!(CAT, "waiting for buffer to be handled");
    std::thread::sleep(Duration::from_millis(5));
    // not available yet
    assert!(h.try_pull().is_none());

    gst::info!(CAT, "Waiting for buffer {buffer_ts}");
    let buf = h.pull().unwrap();
    assert_eq!(buf.pts(), Some(buffer_ts));
    gst::info!(CAT, "Got buffer {buffer_ts}");

    gst::info!(CAT, "next: late buffer => available immediately");
    clock_time += 2 * BUFFER_INTERVAL;
    clock.set_time(clock_time);
    buffer_ts += BUFFER_INTERVAL;
    push_buffer(&appsrc, clock_time, buffer_ts);
    gst::info!(CAT, "waiting for buffer to be handled");
    std::thread::sleep(Duration::from_millis(5));

    // buffer available immediately
    let buf = h.try_pull().unwrap();
    gst::info!(CAT, "Pulling buffer {buffer_ts}");
    assert_eq!(buf.pts(), Some(buffer_ts));
    gst::info!(CAT, "Got buffer {buffer_ts}");
}
