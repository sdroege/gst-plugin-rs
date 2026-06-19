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
        gstregex::plugin_register_static().expect("regex test");
    });
}

#[test]
fn test_replace_all() {
    init();

    let input = b"crap that mothertrapper";

    let expected_output = "trap that mothertrapper";

    let mut h = gst_check::Harness::new("regex");

    {
        let regex = h.element().expect("Could not create regex");

        let command = gst::Structure::builder("replace-all")
            .field("pattern", "crap")
            .field("replacement", "trap")
            .build();

        let commands = gst::Array::new([command]);

        regex.set_property("commands", &commands);
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
fn test_multi_buffer_replace() {
    init();

    let input = "Here in Saint Paul, Minnesota we love Saint";

    let command_1 = gst::Structure::builder("replace-all")
        .field("pattern", "Saint Paul")
        .field("replacement", "St. Paul")
        .build();

    let command_2 = gst::Structure::builder("replace-all")
        .field("pattern", "St. Paul, Minnesota")
        .field("replacement", "our city")
        .build();

    let commands = gst::Array::new([command_1, command_2]);

    let element = gst::ElementFactory::make("regex")
        .property("commands", &commands)
        .property_from_str("multi-buffer-mode", "compress")
        .build()
        .unwrap();

    let mut h = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    h.set_src_caps(
        gst::Caps::builder("text/x-raw")
            .field("format", "utf8")
            .build(),
    );

    for (idx, word) in input.split(" ").enumerate() {
        let buf = {
            let mut buf = gst::Buffer::from_mut_slice(Vec::from(word));
            let buf_ref = buf.get_mut().unwrap();
            buf_ref.set_pts(Some((idx as u64).seconds()));
            buf_ref.set_duration(1.seconds());
            buf
        };

        assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
    }

    h.push_event(gst::event::Eos::builder().build());

    let expected_output = [
        ("Here", 0),
        ("in", 1),
        ("our city we", 5),
        ("love", 6),
        ("Saint", 7),
    ];

    for (text, n_seconds) in expected_output {
        let buf = h.pull().expect("Couldn't pull buffer");

        assert_eq!(buf.pts(), Some(n_seconds.seconds()));
        assert_eq!(buf.duration(), Some(1.seconds()));

        let map = buf.map_readable().expect("Couldn't map buffer readable");
        assert_eq!(std::str::from_utf8(map.as_ref()).unwrap(), text);
        eprintln!("Processed {text}");
    }
}
