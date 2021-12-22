// Copyright (C) 2021 OneStream Live <guillaume.desmottes@onestream.live>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::path::PathBuf;

use gst::prelude::*;
use gst::MessageView;
use more_asserts::assert_ge;

struct TestMedia {
    uri: String,
    len: gst::ClockTime,
}

fn file_name_to_uri(name: &str) -> String {
    let input_path = {
        let mut r = PathBuf::new();
        r.push(env!("CARGO_MANIFEST_DIR"));
        r.push("tests");
        r.push(name);
        r
    };

    let url = url::Url::from_file_path(&input_path).unwrap();
    url.to_string()
}

impl TestMedia {
    fn ogg() -> Self {
        Self {
            uri: file_name_to_uri("sample.ogg"),
            len: gst::ClockTime::from_mseconds(510),
        }
    }

    fn mkv() -> Self {
        Self {
            uri: file_name_to_uri("sample.mkv"),
            len: gst::ClockTime::from_mseconds(510),
        }
    }

    fn missing_file() -> Self {
        Self {
            uri: "file:///not-there.ogg".to_string(),
            len: gst::ClockTime::from_mseconds(10),
        }
    }

    fn missing_http() -> Self {
        Self {
            uri: "http:///not-there.ogg".to_string(),
            len: gst::ClockTime::from_mseconds(10),
        }
    }
}

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gsturiplaylistbin::plugin_register_static()
            .expect("Failed to register uriplaylistbin plugin");
    });
}

fn test(
    medias: Vec<TestMedia>,
    n_streams: u32,
    iterations: u32,
    check_streams: bool,
) -> (Vec<gst::Message>, u32, u64) {
    init();

    let playlist_len = medias.len() * (iterations as usize);

    let pipeline = gst::Pipeline::new(None);
    let playlist = gst::ElementFactory::make("uriplaylistbin", None).unwrap();
    let mq = gst::ElementFactory::make("multiqueue", None).unwrap();

    pipeline.add_many(&[&playlist, &mq]).unwrap();

    let total_len = gst::ClockTime::from_nseconds(
        medias
            .iter()
            .map(|t| t.len.nseconds() * (iterations as u64))
            .sum(),
    );

    let uris: Vec<String> = medias.iter().map(|t| t.uri.clone()).collect();

    playlist.set_property("uris", &uris);
    playlist.set_property("iterations", &iterations);

    assert_eq!(playlist.property::<u32>("current-iteration"), 0);
    assert_eq!(playlist.property::<u64>("current-uri-index"), 0);

    let mq_clone = mq.clone();
    playlist.connect_pad_added(move |_playlist, src_pad| {
        let mq_sink = mq_clone.request_pad_simple("sink_%u").unwrap();
        src_pad.link(&mq_sink).unwrap();
    });

    let pipeline_weak = pipeline.downgrade();
    mq.connect_pad_added(move |_mq, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }

        let pipeline = match pipeline_weak.upgrade() {
            Some(pipeline) => pipeline,
            None => return,
        };

        let sink = gst::ElementFactory::make("fakesink", None).unwrap();
        pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent().unwrap();

        pad.link(&sink.static_pad("sink").unwrap()).unwrap();
    });

    pipeline.set_state(gst::State::Playing).unwrap();

    let bus = pipeline.bus().unwrap();
    let mut events = vec![];

    loop {
        let msg = bus.iter_timed(gst::ClockTime::NONE).next().unwrap();

        match msg.view() {
            MessageView::Error(_) | MessageView::Eos(..) => {
                events.push(msg.clone());
                break;
            }
            // check stream related messages
            MessageView::StreamCollection(_) | MessageView::StreamsSelected(_) => {
                events.push(msg.clone())
            }
            _ => {}
        }
    }

    // check we actually played all files and all streams
    fn stream_end_ts(sink: &gst::Element) -> gst::ClockTime {
        let sample: gst::Sample = sink.property("last-sample");
        let buffer = sample.buffer().unwrap();
        let pts = buffer.pts().unwrap();
        let segment = sample.segment().unwrap();
        let segment = segment.downcast_ref::<gst::ClockTime>().unwrap();
        let rt = segment.to_running_time(pts).unwrap();

        rt + buffer.duration().unwrap()
    }

    if check_streams {
        // check all streams have been fully played
        let mut n = 0;
        for sink in pipeline.iterate_sinks() {
            let sink = sink.unwrap();
            assert_ge!(
                stream_end_ts(&sink),
                total_len,
                "{}: {} < {}",
                sink.name(),
                stream_end_ts(&sink),
                total_len
            );
            n += 1;
        }
        assert_eq!(n, n_streams);

        // check stream-collection and streams-selected message ordering
        let mut events = events.clone().into_iter();

        for _ in 0..playlist_len {
            let decodebin = assert_stream_collection(events.next().unwrap(), n_streams as usize);
            assert_eq!(
                assert_stream_selected(events.next().unwrap(), n_streams as usize),
                decodebin
            );
        }
    }

    let current_iteration = playlist.property::<u32>("current-iteration");
    let current_uri_index = playlist.property::<u64>("current-uri-index");

    pipeline.set_state(gst::State::Null).unwrap();

    (events, current_iteration, current_uri_index)
}

fn assert_eos(msg: gst::Message) {
    assert!(matches!(msg.view(), MessageView::Eos(_)));
}

fn assert_error(msg: gst::Message, failing: TestMedia) {
    match msg.view() {
        MessageView::Error(err) => {
            let details = err.details().unwrap();
            assert_eq!(details.get::<&str>("uri").unwrap(), failing.uri);
        }
        _ => {
            panic!("last message is not an error");
        }
    }
}

fn assert_stream_collection(msg: gst::Message, n_streams: usize) -> gst::Object {
    match msg.view() {
        MessageView::StreamCollection(sc) => {
            let collection = sc.stream_collection();
            assert_eq!(collection.len(), n_streams);
            sc.src().unwrap()
        }
        _ => {
            panic!("message is not a stream collection");
        }
    }
}

fn assert_stream_selected(msg: gst::Message, n_streams: usize) -> gst::Object {
    match msg.view() {
        MessageView::StreamsSelected(ss) => {
            let collection = ss.stream_collection();
            assert_eq!(collection.len(), n_streams);
            ss.src().unwrap()
        }
        _ => {
            panic!("message is not stream selected");
        }
    }
}

#[test]
fn single_audio() {
    let (events, current_iteration, current_uri_index) = test(vec![TestMedia::ogg()], 1, 1, true);
    assert_eos(events.into_iter().last().unwrap());
    assert_eq!(current_iteration, 0);
    assert_eq!(current_uri_index, 0);
}

#[test]
fn single_video() {
    let (events, current_iteration, current_uri_index) = test(vec![TestMedia::mkv()], 2, 1, true);
    assert_eos(events.into_iter().last().unwrap());
    assert_eq!(current_iteration, 0);
    assert_eq!(current_uri_index, 0);
}

#[test]
fn multi_audio() {
    let (events, current_iteration, current_uri_index) = test(
        vec![TestMedia::ogg(), TestMedia::ogg(), TestMedia::ogg()],
        1,
        1,
        true,
    );
    assert_eos(events.into_iter().last().unwrap());
    assert_eq!(current_iteration, 0);
    assert_eq!(current_uri_index, 2);
}

#[test]
fn multi_audio_video() {
    let (events, current_iteration, current_uri_index) =
        test(vec![TestMedia::mkv(), TestMedia::mkv()], 2, 1, true);
    assert_eos(events.into_iter().last().unwrap());
    assert_eq!(current_iteration, 0);
    assert_eq!(current_uri_index, 1);
}

#[test]
fn iterations() {
    let (events, current_iteration, current_uri_index) =
        test(vec![TestMedia::mkv(), TestMedia::mkv()], 2, 2, true);
    assert_eos(events.into_iter().last().unwrap());
    assert_eq!(current_iteration, 1);
    assert_eq!(current_uri_index, 1);
}

#[test]
fn nb_streams_increasing() {
    let (events, current_iteration, current_uri_index) =
        test(vec![TestMedia::ogg(), TestMedia::mkv()], 2, 1, false);
    assert_eos(events.into_iter().last().unwrap());
    assert_eq!(current_iteration, 0);
    assert_eq!(current_uri_index, 1);
}

#[test]
fn missing_file() {
    let (events, current_iteration, current_uri_index) = test(
        vec![TestMedia::ogg(), TestMedia::missing_file()],
        1,
        1,
        false,
    );
    assert_error(
        events.into_iter().last().unwrap(),
        TestMedia::missing_file(),
    );
    assert_eq!(current_iteration, 0);
    assert_eq!(current_uri_index, 0);
}

#[test]
fn missing_http() {
    let (events, current_iteration, current_uri_index) = test(
        vec![TestMedia::ogg(), TestMedia::missing_http()],
        1,
        1,
        false,
    );
    assert_error(
        events.into_iter().last().unwrap(),
        TestMedia::missing_http(),
    );
    assert_eq!(current_iteration, 0);
    assert_eq!(current_uri_index, 0);
}
