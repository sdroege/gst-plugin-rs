// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
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
        gstthreadshare::plugin_register_static().expect("gstthreadshare inputselector test");
    });
}

#[test]
fn test_active_pad() {
    init();

    let is = gst::ElementFactory::make("ts-input-selector", None).unwrap();

    let mut h1 = gst_check::Harness::with_element(&is, Some("sink_%u"), Some("src"));
    let mut h2 = gst_check::Harness::with_element(&is, Some("sink_%u"), None);

    let active_pad = is
        .property("active-pad")
        .unwrap()
        .get::<Option<gst::Pad>>()
        .unwrap();
    assert_eq!(active_pad, h1.srcpad().unwrap().peer());

    is.set_property("active-pad", h2.srcpad().unwrap().peer())
        .unwrap();
    let active_pad = is
        .property("active-pad")
        .unwrap()
        .get::<Option<gst::Pad>>()
        .unwrap();
    assert_eq!(active_pad, h2.srcpad().unwrap().peer());

    h1.set_src_caps_str("foo/bar");
    h2.set_src_caps_str("foo/bar");

    h1.play();

    /* Push buffer on inactive pad, we should not receive anything */
    let buf = gst::Buffer::new();
    assert_eq!(h1.push(buf), Ok(gst::FlowSuccess::Ok));
    assert_eq!(h1.buffers_received(), 0);

    /* Buffers pushed on the active pad should be received */
    let buf = gst::Buffer::new();
    assert_eq!(h2.push(buf), Ok(gst::FlowSuccess::Ok));
    assert_eq!(h1.buffers_received(), 1);

    assert_eq!(h1.events_received(), 3);

    let event = h1.pull_event().unwrap();
    assert_eq!(event.type_(), gst::EventType::StreamStart);
    let event = h1.pull_event().unwrap();
    assert_eq!(event.type_(), gst::EventType::Caps);
    let event = h1.pull_event().unwrap();
    assert_eq!(event.type_(), gst::EventType::Segment);

    /* Push another buffer on the active pad, there should be no new events */
    let buf = gst::Buffer::new();
    assert_eq!(h2.push(buf), Ok(gst::FlowSuccess::Ok));
    assert_eq!(h1.buffers_received(), 2);
    assert_eq!(h1.events_received(), 3);

    /* Switch the active pad and push a buffer, we should receive stream-start, segment and caps
     * again */
    let buf = gst::Buffer::new();
    is.set_property("active-pad", h1.srcpad().unwrap().peer())
        .unwrap();
    assert_eq!(h1.push(buf), Ok(gst::FlowSuccess::Ok));
    assert_eq!(h1.buffers_received(), 3);
    assert_eq!(h1.events_received(), 6);
    let event = h1.pull_event().unwrap();
    assert_eq!(event.type_(), gst::EventType::StreamStart);
    let event = h1.pull_event().unwrap();
    assert_eq!(event.type_(), gst::EventType::Caps);
    let event = h1.pull_event().unwrap();
    assert_eq!(event.type_(), gst::EventType::Segment);

    let _ = is.set_state(gst::State::Null);
}
