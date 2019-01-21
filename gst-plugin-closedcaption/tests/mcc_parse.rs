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

#[macro_use]
extern crate pretty_assertions;

use gst::prelude::*;
use rand::{Rng, SeedableRng};

fn init() {
    use std::sync::{Once, ONCE_INIT};
    static INIT: Once = ONCE_INIT;

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsclosedcaption::plugin_register_static().expect("mccparse test");
    });
}

/// Randomized test passing buffers of arbitrary sizes to the parser
#[test]
fn test_parse() {
    init();
    let mut data = include_bytes!("captions-test_708.mcc").as_ref();

    let mut rnd = if let Ok(seed) = std::env::var("MCC_PARSE_TEST_SEED") {
        rand::rngs::SmallRng::seed_from_u64(
            seed.parse::<u64>()
                .expect("MCC_PARSE_TEST_SEED has to contain a 64 bit integer seed"),
        )
    } else {
        let seed = rand::random::<u64>();
        println!("seed {}", seed);
        rand::rngs::SmallRng::seed_from_u64(seed)
    };

    let mut h = gst_check::Harness::new("mccparse");

    h.set_src_caps_str("application/x-mcc, version=(int) 1");

    let mut input_len = 0;
    let mut output_len = 0;
    let mut checksum = 0u32;
    let mut expected_timecode = None;

    while !data.is_empty() {
        let l = if data.len() == 1 {
            1
        } else {
            rnd.gen_range(1, data.len())
        };
        let buf = gst::Buffer::from_mut_slice(Vec::from(&data[0..l]));
        input_len += buf.get_size();
        assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
        while let Some(buf) = h.try_pull() {
            output_len += buf.get_size();
            checksum = checksum.wrapping_add(
                buf.map_readable()
                    .unwrap()
                    .iter()
                    .fold(0u32, |s, v| s.wrapping_add(*v as u32)),
            );

            let tc_meta = buf
                .get_meta::<gst_video::VideoTimeCodeMeta>()
                .expect("No timecode meta");
            if let Some(ref timecode) = expected_timecode {
                assert_eq!(&tc_meta.get_tc(), timecode);
            } else {
                expected_timecode = Some(tc_meta.get_tc());
            }
            expected_timecode.as_mut().map(|tc| tc.increment_frame());
        }
        data = &data[l..];
    }

    h.push_event(gst::Event::new_eos().build());
    while let Some(buf) = h.try_pull() {
        output_len += buf.get_size();
        checksum = checksum.wrapping_add(
            buf.map_readable()
                .unwrap()
                .iter()
                .fold(0u32, |s, v| s.wrapping_add(*v as u32)),
        );

        let tc_meta = buf
            .get_meta::<gst_video::VideoTimeCodeMeta>()
            .expect("No timecode meta");
        if let Some(ref timecode) = expected_timecode {
            assert_eq!(&tc_meta.get_tc(), timecode);
        } else {
            expected_timecode = Some(tc_meta.get_tc());
        }
        expected_timecode.as_mut().map(|tc| tc.increment_frame());
    }

    assert!(expected_timecode.is_some());
    assert_eq!(input_len, 28818);
    assert_eq!(output_len, 42383);
    assert_eq!(checksum, 3988480);

    let caps = h
        .get_sinkpad()
        .expect("harness has no sinkpad")
        .get_current_caps()
        .expect("pad has no caps");
    assert_eq!(
        caps,
        gst::Caps::builder("closedcaption/x-cea-708")
            .field("format", &"cdp")
            .field("framerate", &gst::Fraction::new(30000, 1001))
            .build()
    );
}
