// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
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
        gsttextwrap::plugin_register_static().expect("textwrap test");
    });
}

#[test]
fn test_columns() {
    init();

    let input = b"Split this text up";

    let expected_output = "Split\nthis\ntext\nup";

    let mut h = gst_check::Harness::new("textwrap");

    {
        let wrap = h.element().expect("Could not create textwrap");
        wrap.set_property("columns", 5u32);
    }

    h.set_src_caps_str("text/x-raw, format=utf8");

    let buf = {
        let mut buf = gst::Buffer::from_mut_slice(Vec::from(&input[..]));
        let buf_ref = buf.get_mut().unwrap();
        buf_ref.set_pts(gst::ClockTime::ZERO);
        buf_ref.set_duration(2.seconds());
        buf
    };

    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));

    let buf = h.pull().expect("Couldn't pull buffer");

    assert_eq!(buf.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(buf.duration(), Some(2.seconds()));

    let map = buf.map_readable().expect("Couldn't map buffer readable");

    assert_eq!(
        std::str::from_utf8(map.as_ref()),
        std::str::from_utf8(expected_output.as_ref())
    );
}

#[test]
fn test_lines() {
    init();

    let input = b"Split this text up";

    let mut h = gst_check::Harness::new("textwrap");

    {
        let wrap = h.element().expect("Could not create textwrap");
        wrap.set_property("columns", 5u32);
        wrap.set_property("lines", 2u32);
    }

    h.set_src_caps_str("text/x-raw, format=utf8");

    let buf = {
        let mut buf = gst::Buffer::from_mut_slice(Vec::from(&input[..]));
        let buf_ref = buf.get_mut().unwrap();
        buf_ref.set_pts(gst::ClockTime::ZERO);
        buf_ref.set_duration(2.seconds());
        buf
    };

    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));

    let buf = h.pull().expect("Couldn't pull buffer");

    assert_eq!(buf.pts(), Some(gst::ClockTime::ZERO));
    assert_eq!(buf.duration(), Some(gst::ClockTime::SECOND));

    let expected_output = "Split\nthis";

    let map = buf.map_readable().expect("Couldn't map buffer readable");

    assert_eq!(
        std::str::from_utf8(map.as_ref()),
        std::str::from_utf8(expected_output.as_ref())
    );

    let buf = h.pull().expect("Couldn't pull buffer");

    assert_eq!(buf.pts(), Some(gst::ClockTime::SECOND));
    assert_eq!(buf.duration(), Some(gst::ClockTime::SECOND));

    let expected_output = "text\nup";

    let map = buf.map_readable().expect("Couldn't map buffer readable");

    assert_eq!(
        std::str::from_utf8(map.as_ref()),
        std::str::from_utf8(expected_output.as_ref())
    );
}
