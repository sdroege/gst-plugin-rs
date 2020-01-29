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

use glib;
use glib::prelude::*;

use gst;
use gst::prelude::*;
use gst_check;

use gstthreadshare;

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

    let mut h1 = gst_check::Harness::new_with_element(&is, Some("sink_%u"), Some("src"));
    let mut h2 = gst_check::Harness::new_with_element(&is, Some("sink_%u"), None);

    let active_pad = is
        .get_property("active-pad")
        .unwrap()
        .get::<gst::Pad>()
        .unwrap();
    assert!(active_pad == h1.get_srcpad().unwrap().get_peer());

    is.set_property("active-pad", &h2.get_srcpad().unwrap().get_peer())
        .unwrap();
    let active_pad = is
        .get_property("active-pad")
        .unwrap()
        .get::<gst::Pad>()
        .unwrap();
    assert!(active_pad == h2.get_srcpad().unwrap().get_peer());

    h1.set_src_caps_str("foo/bar");
    h2.set_src_caps_str("foo/bar");

    h1.play();

    /* Push buffer on inactive pad, we should not receive anything */
    let buf = gst::Buffer::new();
    assert!(h1.push(buf) == Ok(gst::FlowSuccess::Ok));
    assert!(h1.buffers_received() == 0);

    /* Buffers pushed on the active pad should be received */
    let buf = gst::Buffer::new();
    assert!(h2.push(buf) == Ok(gst::FlowSuccess::Ok));
    assert!(h1.buffers_received() == 1);

    assert!(h1.events_received() == 4);

    let event = h1.pull_event().unwrap();
    assert!(event.get_type() == gst::EventType::CustomDownstreamSticky);
    let event = h1.pull_event().unwrap();
    assert!(event.get_type() == gst::EventType::StreamStart);
    let event = h1.pull_event().unwrap();
    assert!(event.get_type() == gst::EventType::Caps);
    let event = h1.pull_event().unwrap();
    assert!(event.get_type() == gst::EventType::Segment);

    /* Push another buffer on the active pad, there should be no new events */
    let buf = gst::Buffer::new();
    assert!(h2.push(buf) == Ok(gst::FlowSuccess::Ok));
    assert!(h1.buffers_received() == 2);
    assert!(h1.events_received() == 4);

    /* Switch the active pad and push a buffer, we should receive segment and caps again */
    let buf = gst::Buffer::new();
    is.set_property("active-pad", &h1.get_srcpad().unwrap().get_peer())
        .unwrap();
    assert!(h1.push(buf) == Ok(gst::FlowSuccess::Ok));
    assert!(h1.buffers_received() == 3);
    assert!(h1.events_received() == 6);
    let event = h1.pull_event().unwrap();
    assert!(event.get_type() == gst::EventType::Caps);
    let event = h1.pull_event().unwrap();
    assert!(event.get_type() == gst::EventType::Segment);

    let _ = is.set_state(gst::State::Null);
}
