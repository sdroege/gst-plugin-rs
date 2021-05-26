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

use gst::EventView;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsjson::plugin_register_static().expect("json test");
    });
}

#[test]
fn test_enc() {
    init();

    let input = "{\"foo\":42}";

    let mut h = gst_check::Harness::new("jsongstenc");

    h.set_src_caps_str("application/x-json, format=test");

    let buf = {
        let mut buf = gst::Buffer::from_mut_slice(Vec::from(input));
        let buf_ref = buf.get_mut().unwrap();
        buf_ref.set_pts(gst::ClockTime::from_seconds(0));
        buf_ref.set_duration(gst::ClockTime::from_seconds(2));
        buf
    };

    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));

    let buf = h.pull().expect("Couldn't pull buffer");
    let map = buf.map_readable().expect("Couldn't map buffer readable");
    assert_eq!(
        std::str::from_utf8(map.as_ref()),
        Ok("{\"Header\":{\"format\":\"test\"}}\n"),
    );

    let buf = h.pull().expect("Couldn't pull buffer");
    assert_eq!(buf.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(buf.duration(), Some(2 * gst::ClockTime::SECOND));
    let map = buf.map_readable().expect("Couldn't map buffer readable");
    assert_eq!(
        std::str::from_utf8(map.as_ref()),
        Ok("{\"Buffer\":{\"pts\":0,\"duration\":2000000000,\"data\":{\"foo\":42}}}\n")
    );
}

#[test]
fn test_parse() {
    init();

    let input = [
        "{\"Header\":{\"format\":\"test\"}}\n",
        "{\"Buffer\":{\"pts\":0,\"duration\":2000000000,\"data\":{\"foo\":42}}}\n",
    ];

    let mut h = gst_check::Harness::new("jsongstparse");

    h.set_src_caps_str("text/x-raw");

    for input in &input {
        let buf = gst::Buffer::from_mut_slice(Vec::from(&input[..]));
        assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
    }

    while h.events_in_queue() > 0 {
        let ev = h.pull_event().unwrap();

        match ev.view() {
            EventView::Caps(ev) => {
                assert!(ev.caps().is_strictly_equal(&gst::Caps::new_simple(
                    &"application/x-json",
                    &[(&"format", &"test")]
                )));
            }
            _ => (),
        }
    }

    let buf = h.pull().expect("Couldn't pull buffer");
    let map = buf.map_readable().expect("Couldn't map buffer readable");
    assert_eq!(buf.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(buf.duration(), Some(2 * gst::ClockTime::SECOND));
    assert_eq!(std::str::from_utf8(map.as_ref()), Ok("{\"foo\":42}"));
}
