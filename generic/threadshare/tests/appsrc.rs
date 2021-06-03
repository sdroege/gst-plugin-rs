// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare appsrc test");
    });
}

#[test]
fn push() {
    init();

    let mut h = gst_check::Harness::new("ts-appsrc");

    let caps = gst::Caps::new_simple("foo/bar", &[]);
    {
        let appsrc = h.element().unwrap();
        appsrc.set_property("caps", &caps).unwrap();
        appsrc.set_property("do-timestamp", &true).unwrap();
        appsrc.set_property("context", &"appsrc-push").unwrap();
    }

    h.play();

    {
        let appsrc = h.element().unwrap();
        for _ in 0..3 {
            assert!(appsrc
                .emit_by_name("push-buffer", &[&gst::Buffer::new()])
                .unwrap()
                .unwrap()
                .get::<bool>()
                .unwrap());
        }

        assert!(appsrc
            .emit_by_name("end-of-stream", &[])
            .unwrap()
            .unwrap()
            .get::<bool>()
            .unwrap());
    }

    for _ in 0..3 {
        let _buffer = h.pull().unwrap();
    }

    let mut n_events = 0;
    loop {
        use gst::EventView;

        let event = h.pull_event().unwrap();
        match event.view() {
            EventView::StreamStart(..) => {
                assert_eq!(n_events, 0);
            }
            EventView::Caps(ev) => {
                assert_eq!(n_events, 1);
                let event_caps = ev.caps();
                assert_eq!(caps.as_ref(), event_caps);
            }
            EventView::Segment(..) => {
                assert_eq!(n_events, 2);
            }
            EventView::Eos(..) => {
                break;
            }
            _ => (),
        }
        n_events += 1;
    }
    assert!(n_events >= 2);
}

#[test]
fn pause_regular() {
    init();

    let mut h = gst_check::Harness::new("ts-appsrc");

    let caps = gst::Caps::new_simple("foo/bar", &[]);
    {
        let appsrc = h.element().unwrap();
        appsrc.set_property("caps", &caps).unwrap();
        appsrc.set_property("do-timestamp", &true).unwrap();
        appsrc.set_property("context", &"appsrc-pause").unwrap();
    }

    h.play();

    let appsrc = h.element().unwrap();

    // Initial buffer
    assert!(appsrc
        .emit_by_name("push-buffer", &[&gst::Buffer::from_slice(vec![1, 2, 3, 4])])
        .unwrap()
        .unwrap()
        .get::<bool>()
        .unwrap());

    let _ = h.pull().unwrap();

    // Pre-pause buffer
    assert!(appsrc
        .emit_by_name("push-buffer", &[&gst::Buffer::from_slice(vec![5, 6, 7])])
        .unwrap()
        .unwrap()
        .get::<bool>()
        .unwrap());

    appsrc
        .change_state(gst::StateChange::PlayingToPaused)
        .unwrap();

    // Buffer is queued during Paused
    assert!(appsrc
        .emit_by_name("push-buffer", &[&gst::Buffer::from_slice(vec![8, 9])])
        .unwrap()
        .unwrap()
        .get::<bool>()
        .unwrap());

    appsrc
        .change_state(gst::StateChange::PausedToPlaying)
        .unwrap();

    // Pull Pre-pause buffer
    let _ = h.pull().unwrap();

    // Pull buffer queued while Paused
    let _ = h.pull().unwrap();

    // Can push again
    assert!(appsrc
        .emit_by_name("push-buffer", &[&gst::Buffer::new()])
        .unwrap()
        .unwrap()
        .get::<bool>()
        .unwrap());

    let _ = h.pull().unwrap();
    assert!(h.try_pull().is_none());
}

#[test]
fn flush_regular() {
    init();

    let mut h = gst_check::Harness::new("ts-appsrc");

    let caps = gst::Caps::new_simple("foo/bar", &[]);
    {
        let appsrc = h.element().unwrap();
        appsrc.set_property("caps", &caps).unwrap();
        appsrc.set_property("do-timestamp", &true).unwrap();
        appsrc.set_property("context", &"appsrc-flush").unwrap();
    }

    h.play();

    let appsrc = h.element().unwrap();

    // Initial buffer
    assert!(appsrc
        .emit_by_name("push-buffer", &[&gst::Buffer::from_slice(vec![1, 2, 3, 4])])
        .unwrap()
        .unwrap()
        .get::<bool>()
        .unwrap());

    let _ = h.pull().unwrap();

    // FlushStart
    assert!(h.push_upstream_event(gst::event::FlushStart::new()));

    // Can't push buffer while flushing
    assert!(!appsrc
        .emit_by_name("push-buffer", &[&gst::Buffer::new()])
        .unwrap()
        .unwrap()
        .get::<bool>()
        .unwrap());

    assert!(h.try_pull().is_none());

    // FlushStop
    assert!(h.push_upstream_event(gst::event::FlushStop::new(true)));

    // No buffer available due to flush
    assert!(h.try_pull().is_none());

    // Can push again
    assert!(appsrc
        .emit_by_name("push-buffer", &[&gst::Buffer::new()])
        .unwrap()
        .unwrap()
        .get::<bool>()
        .unwrap());

    let _ = h.pull().unwrap();
    assert!(h.try_pull().is_none());
}

#[test]
fn pause_flush() {
    init();

    let mut h = gst_check::Harness::new("ts-appsrc");

    let caps = gst::Caps::new_simple("foo/bar", &[]);
    {
        let appsrc = h.element().unwrap();
        appsrc.set_property("caps", &caps).unwrap();
        appsrc.set_property("do-timestamp", &true).unwrap();
        appsrc
            .set_property("context", &"appsrc-pause_flush")
            .unwrap();
    }

    h.play();

    let appsrc = h.element().unwrap();

    // Initial buffer
    assert!(appsrc
        .emit_by_name("push-buffer", &[&gst::Buffer::from_slice(vec![1, 2, 3, 4])])
        .unwrap()
        .unwrap()
        .get::<bool>()
        .unwrap());

    let _ = h.pull().unwrap();

    appsrc
        .change_state(gst::StateChange::PlayingToPaused)
        .unwrap();

    // FlushStart
    assert!(h.push_upstream_event(gst::event::FlushStart::new()));

    // Can't push buffers while flushing
    assert!(!appsrc
        .emit_by_name("push-buffer", &[&gst::Buffer::new()])
        .unwrap()
        .unwrap()
        .get::<bool>()
        .unwrap());

    assert!(h.try_pull().is_none());

    // FlushStop
    assert!(h.push_upstream_event(gst::event::FlushStop::new(true)));

    appsrc
        .change_state(gst::StateChange::PausedToPlaying)
        .unwrap();

    // No buffer available due to flush
    assert!(h.try_pull().is_none());

    // Can push again
    assert!(appsrc
        .emit_by_name("push-buffer", &[&gst::Buffer::new()])
        .unwrap()
        .unwrap()
        .get::<bool>()
        .unwrap());

    let _ = h.pull().unwrap();
    assert!(h.try_pull().is_none());
}
