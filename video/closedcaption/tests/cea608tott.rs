// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
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
use gst::ClockTime;

use pretty_assertions::assert_eq;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsclosedcaption::plugin_register_static().unwrap();
    });
}

#[test]
fn test_parse() {
    init();
    let data = include_bytes!("dn2018-1217.scc").as_ref();

    let mut h = gst_check::Harness::new_parse("sccparse ! cea608tott");
    h.set_src_caps_str("application/x-scc");
    h.set_sink_caps_str("text/x-raw");

    let buf = gst::Buffer::from_mut_slice(Vec::from(data));
    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));

    // Check the first 4 output buffers
    let expected: [(ClockTime, ClockTime, &'static str); 4] = [
        (
            ClockTime::from_nseconds(15_048_366_666),
            ClockTime::from_nseconds(3_236_566_667),
            "From New York,\r\nthis is Democracy Now!",
        ),
        (
            ClockTime::from_nseconds(18_985_633_333),
            ClockTime::from_nseconds(1_234_566_667),
            "Yes, I’m supporting\r\nDonald Trump.",
        ),
        (
            ClockTime::from_nseconds(20_220_200_000),
            ClockTime::from_nseconds(2_168_833_333),
            "I’m doing so as enthusiastically\r\nas I can,",
        ),
        (
            ClockTime::from_nseconds(22_389_033_333),
            ClockTime::from_nseconds(2_235_566_667),
            "even the fact I think\r\nhe’s a terrible human being.",
        ),
    ];

    for (i, e) in expected.iter().enumerate() {
        let buf = h.try_pull().unwrap();

        assert_eq!(
            e.0,
            buf.pts().unwrap(),
            "Unexpected PTS for {}th buffer",
            i + 1
        );
        assert_eq!(
            e.1,
            buf.duration().unwrap(),
            "Unexpected duration for {}th buffer",
            i + 1
        );

        let data = buf.map_readable().unwrap();
        let s = std::str::from_utf8(&*data)
            .unwrap_or_else(|_| panic!("Non-UTF8 data for {}th buffer", i + 1));
        assert_eq!(e.2, s, "Unexpected data for {}th buffer", i + 1);
    }

    let caps = h
        .sinkpad()
        .expect("harness has no sinkpad")
        .current_caps()
        .expect("pad has no caps");
    assert_eq!(
        caps,
        gst::Caps::builder("text/x-raw")
            .field("format", &"utf8")
            .build()
    );
}
