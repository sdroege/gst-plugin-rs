// Copyright (C) 2021 Mathieu Duponchelle <mathieu@centricular.com>
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
use pretty_assertions::assert_eq;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrswebp::plugin_register_static().expect("webpenc-rs test");
    });
}

#[test]
fn test_decode() {
    init();
    let data = include_bytes!("animated.webp").as_ref();
    let mut h = gst_check::Harness::new("webpdec-rs");

    h.set_src_caps_str("image/webp");

    let buf = gst::Buffer::from_slice(data);
    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
    h.push_event(gst::event::Eos::new());

    let mut expected_timestamp: Option<gst::ClockTime> = Some(gst::ClockTime::ZERO);
    let mut count = 0;
    let expected_duration: Option<gst::ClockTime> = Some(gst::ClockTime::from_nseconds(40_000_000));

    while let Some(buf) = h.try_pull() {
        assert_eq!(buf.pts(), expected_timestamp);
        assert_eq!(buf.duration(), expected_duration);

        expected_timestamp = expected_timestamp
            .zip(expected_duration)
            .map(|(ts, duration)| ts + duration);
        count += 1;
    }

    assert_eq!(count, 10);

    let caps = h
        .sinkpad()
        .expect("harness has no sinkpad")
        .current_caps()
        .expect("pad has no caps");
    assert_eq!(
        caps,
        gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgba, 400, 400)
            .fps((0, 1))
            .build()
            .unwrap()
            .to_caps()
            .unwrap()
    );
}
