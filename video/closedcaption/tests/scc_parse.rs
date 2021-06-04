// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2019 Jordan Petridis <jordan@centricular.com>
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
use gst::EventView;
use gst_video::{ValidVideoTimeCode, VideoTimeCode};
use pretty_assertions::assert_eq;
use rand::{Rng, SeedableRng};
use std::collections::VecDeque;
use std::path::PathBuf;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsclosedcaption::plugin_register_static().unwrap();
    });
}

/// Randomized test passing buffers of arbitrary sizes to the parser
#[test]
fn test_parse() {
    init();
    let mut data = include_bytes!("dn2018-1217.scc").as_ref();

    let mut rnd = if let Ok(seed) = std::env::var("SCC_PARSE_TEST_SEED") {
        rand::rngs::SmallRng::seed_from_u64(
            seed.parse::<u64>()
                .expect("SCC_PARSE_TEST_SEED has to contain a 64 bit integer seed"),
        )
    } else {
        let seed = rand::random::<u64>();
        println!("seed {}", seed);
        rand::rngs::SmallRng::seed_from_u64(seed)
    };

    let mut h = gst_check::Harness::new("sccparse");
    h.set_src_caps_str("application/x-scc");

    let mut input_len = 0;
    let mut output_len = 0;
    let mut checksum = 0u32;

    while !data.is_empty() {
        let l = if data.len() == 1 {
            1
        } else {
            rnd.gen_range(1..=data.len())
        };
        let buf = gst::Buffer::from_mut_slice(Vec::from(&data[0..l]));
        input_len += buf.size();
        assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
        while let Some(buf) = h.try_pull() {
            output_len += buf.size();
            checksum = checksum.wrapping_add(
                buf.map_readable()
                    .unwrap()
                    .iter()
                    .fold(0u32, |s, v| s.wrapping_add(*v as u32)),
            );
        }
        data = &data[l..];
    }

    h.push_event(gst::event::Eos::new());
    while let Some(buf) = h.try_pull() {
        output_len += buf.size();
        checksum = checksum.wrapping_add(
            buf.map_readable()
                .unwrap()
                .iter()
                .fold(0u32, |s, v| s.wrapping_add(*v as u32)),
        );
    }

    assert_eq!(input_len, 241_152);
    assert_eq!(output_len, 89084);
    assert_eq!(checksum, 12_554_799);

    let caps = h
        .sinkpad()
        .expect("harness has no sinkpad")
        .current_caps()
        .expect("pad has no caps");
    assert_eq!(
        caps,
        gst::Caps::builder("closedcaption/x-cea-608")
            .field("format", &"raw")
            .field("framerate", &gst::Fraction::new(30000, 1001))
            .build()
    );
}

/// Test that ensures timecode parsing is the expected one
#[test]
fn test_timecodes() {
    use std::convert::TryInto;
    use std::str::FromStr;

    init();
    let data = include_bytes!("timecodes-cut-down-sample.scc").as_ref();

    let mut h = gst_check::Harness::new("sccparse");
    h.set_src_caps_str("application/x-scc");

    let timecodes = [
        "00:00:00;00",
        "00:00:14;01",
        "00:00:17;26",
        "00:00:19;01",
        "00:00:21;02",
        "00:00:23;10",
        "00:00:25;18",
        "00:00:28;13",
        "00:00:30;29",
        "00:00:34;29",
        "00:00:37;27",
        "00:00:40;01",
        "00:00:43;27",
        "00:00:45;13",
        "00:00:49;16",
        "00:58:51;01",
        "00:58:52;29",
        "00:58:55;00",
        "00:59:00;25",
    ];

    let mut valid_timecodes: VecDeque<ValidVideoTimeCode> = timecodes
        .iter()
        .map(|s| {
            let mut t = VideoTimeCode::from_str(s).unwrap();
            t.set_fps(gst::Fraction::new(30000, 1001));
            t.set_flags(gst_video::VideoTimeCodeFlags::DROP_FRAME);
            t
        })
        .map(|t| t.try_into().unwrap())
        .collect();

    let mut output_len = 0;
    let mut checksum = 0u32;
    let mut expected_timecode = valid_timecodes.pop_front().unwrap();

    let buf = gst::Buffer::from_mut_slice(Vec::from(data));
    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
    while let Some(buf) = h.try_pull() {
        output_len += buf.size();
        checksum = checksum.wrapping_add(
            buf.map_readable()
                .unwrap()
                .iter()
                .fold(0u32, |s, v| s.wrapping_add(*v as u32)),
        );

        // get the timecode of the buffer
        let tc = buf
            .meta::<gst_video::VideoTimeCodeMeta>()
            .expect("No timecode meta")
            .tc();

        // if the timecode matches one of expected codes,
        // pop the valid_timecodes deque and set expected_timecode,
        // to the next timecode.
        if Some(&tc) == valid_timecodes.front() {
            expected_timecode = valid_timecodes.pop_front().unwrap();
        }

        assert_eq!(tc, expected_timecode);
        expected_timecode.increment_frame();
    }

    assert_eq!(output_len, 1268);
    assert_eq!(checksum, 174_295);

    let caps = h
        .sinkpad()
        .expect("harness has no sinkpad")
        .current_caps()
        .expect("pad has no caps");
    assert_eq!(
        caps,
        gst::Caps::builder("closedcaption/x-cea-608")
            .field("format", &"raw")
            .field("framerate", &gst::Fraction::new(30000, 1001))
            .build()
    );
}

#[test]
fn test_pull() {
    init();

    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/dn2018-1217.scc");

    let mut h = gst_check::Harness::new_parse(&format!("filesrc location={:?} ! sccparse", path));

    h.play();

    /* Let's first pull until EOS */
    loop {
        let mut done = false;

        while h.events_in_queue() != 0 {
            let event = h.pull_event();

            if let Ok(event) = event {
                match event.view() {
                    EventView::Eos(_) => {
                        done = true;
                        break;
                    }
                    _ => (),
                }
            }
        }

        while h.buffers_in_queue() != 0 {
            let _ = h.pull();
        }

        if done {
            break;
        }
    }

    /* Now seek and check that we receive buffers with appropriate PTS */
    h.push_upstream_event(gst::event::Seek::new(
        1.0,
        gst::SeekFlags::FLUSH,
        gst::SeekType::Set,
        gst::GenericFormattedValue::Time(Some(18 * gst::ClockTime::SECOND)),
        gst::SeekType::Set,
        gst::GenericFormattedValue::Time(Some(19 * gst::ClockTime::SECOND)),
    ));

    loop {
        let mut done = false;

        while h.buffers_in_queue() != 0 {
            if let Ok(buffer) = h.pull() {
                let pts = buffer.pts().unwrap();
                let end_time = pts + buffer.duration().unwrap();

                assert!(
                    end_time >= 18 * gst::ClockTime::SECOND && pts < 19 * gst::ClockTime::SECOND
                );
            }
        }

        while h.events_in_queue() != 0 {
            let event = h.pull_event();

            if let Ok(event) = event {
                match event.view() {
                    EventView::Eos(_) => {
                        done = true;
                        break;
                    }
                    _ => (),
                }
            }
        }

        if done {
            break;
        }
    }
}
