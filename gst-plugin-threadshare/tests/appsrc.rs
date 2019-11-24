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

use glib::prelude::*;

use gst;
use gst_check;

use gstthreadshare;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare appsrc test");
    });
}

#[test]
fn test_push() {
    init();

    let mut h = gst_check::Harness::new("ts-appsrc");

    let caps = gst::Caps::new_simple("foo/bar", &[]);
    {
        let appsrc = h.get_element().unwrap();
        appsrc.set_property("caps", &caps).unwrap();
        appsrc.set_property("do-timestamp", &true).unwrap();
        appsrc.set_property("context", &"test-push").unwrap();
    }

    h.play();

    {
        let appsrc = h.get_element().unwrap();
        for _ in 0..3 {
            assert!(appsrc
                .emit("push-buffer", &[&gst::Buffer::new()])
                .unwrap()
                .unwrap()
                .get_some::<bool>()
                .unwrap());
        }

        assert!(appsrc
            .emit("end-of-stream", &[])
            .unwrap()
            .unwrap()
            .get_some::<bool>()
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
                let event_caps = ev.get_caps();
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
    assert!(n_events >= 3);
}
