// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#![allow(clippy::single_match)]

use crate::st2038anc_utils::AncDataHeader;
use gst::EventView;
use gst::prelude::*;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("mccparse test");
    });
}

#[test]
fn test_pull() {
    init();

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/captions-test_708.mcc");

    let mut h = gst_check::Harness::new_parse(&format!("filesrc location={path:?} ! mccparse"));

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
        gst::ClockTime::SECOND,
        gst::SeekType::Set,
        2.seconds(),
    ));

    loop {
        let mut done = false;

        while h.buffers_in_queue() != 0 {
            if let Ok(buffer) = h.pull() {
                let pts = buffer.pts().unwrap();
                assert!(pts > gst::ClockTime::SECOND && pts < 2.seconds());
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

#[test]
fn test_parse_with_st2038() {
    init();

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/captions-test_708.mcc");

    let pipeline = gst::parse::launch(&format!(
        "filesrc location={path:?} ! mccparse ! appsink name=sink"
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();

    let appsink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    appsink.set_sync(false);

    let samples = Arc::new(Mutex::new(Vec::new()));
    let samples_clone = samples.clone();

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().unwrap();
                let mut samples = samples_clone.lock().unwrap();
                samples.push(sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut eos = false;
    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.timed_pop(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                eos = true;
                break;
            }
            MessageView::Error(..) => unreachable!(),
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();
    assert!(eos);

    let mut expected_timecode = None;
    let mut data = Vec::<u8>::new();

    let samples = samples.lock().unwrap();
    for s in samples.iter() {
        let buffer = s.buffer().unwrap();

        let tc_meta = buffer
            .meta::<gst_video::VideoTimeCodeMeta>()
            .expect("No timecode meta");

        if let Some(ref timecode) = expected_timecode {
            assert_eq!(&tc_meta.tc(), timecode);
        } else {
            expected_timecode = Some(tc_meta.tc());
        }

        if let Some(ref mut tc) = expected_timecode {
            tc.increment_frame();
        }

        let map = buffer.map_readable().unwrap();
        data.extend_from_slice(map.as_slice());
    }

    let mut slice = data.as_slice();
    while !slice.is_empty() {
        if slice[0] == 0b1111_1111 {
            break;
        }

        let header = AncDataHeader::from_slice(slice);
        assert!(header.is_ok());
        let header = header.unwrap();

        assert!(!header.c_not_y_channel_flag);
        assert_eq!(header.did, 97);
        assert_eq!(header.sdid, 1);
        assert_eq!(header.line_number, 0xFF /* Unknown/unspecified */);
        assert_eq!(
            header.horizontal_offset,
            0xFF /* Unknown/unspecified */
        );

        slice = &slice[header.len..];
    }
}
