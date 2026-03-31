// Copyright (C) 2026, Taruntej Kanakamalla <tarun@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use serial_test::file_serial;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrswebrtc::plugin_register_static().expect("Register rswebrtc plugin");
    });
}

fn run_webrtc_producer(
    pipeline_str: &str,
    tx: mpsc::Sender<String>,
    signaller_server_port: u16,
) -> gst::Pipeline {
    let pipeline = gst::parse::launch(pipeline_str)
        .expect("producer pipeline")
        .downcast::<gst::Pipeline>()
        .unwrap();
    let webrtcsink = pipeline.by_name("ws").unwrap();

    webrtcsink.set_property("signalling-server-port", signaller_server_port as u32);
    let signaller = webrtcsink
        .dynamic_cast_ref::<gst::ChildProxy>()
        .unwrap()
        .child_by_name("signaller")
        .unwrap();

    signaller.connect_notify(
        Some("client-id"),
        glib::clone!(move |signaller, _pspec| {
            let client_id = signaller.property::<String>("client-id");

            tx.send(client_id).unwrap();
        }),
    );

    pipeline
        .set_state(gst::State::Playing)
        .expect("producer changing to playing state");

    pipeline
}

fn run_test(
    producer_pipeline_str: &str,
    consumer_pipeline_str: &str,
    output_caps_name: &str,
    media_type: &str,
    signaller_server_port: u16,
) {
    init();

    let (tx, rx) = mpsc::channel::<String>();
    let producer = run_webrtc_producer(producer_pipeline_str, tx, signaller_server_port);

    let producer_peer_id = rx.recv().unwrap();
    let consumer = gst::parse::launch(consumer_pipeline_str)
        .unwrap()
        .downcast::<gst::Pipeline>()
        .expect("consumer pipeline");

    let webrtcsrc = consumer.by_name("ws").unwrap();

    let signaller = webrtcsrc
        .dynamic_cast_ref::<gst::ChildProxy>()
        .unwrap()
        .child_by_name("signaller")
        .unwrap();

    signaller.set_property("producer-peer-id", producer_peer_id);

    let uri = format!("ws://127.0.0.1:{signaller_server_port}");
    signaller.set_property("uri", uri.as_str());

    let caps_matched = Arc::new(AtomicBool::new(false));

    webrtcsrc.connect_pad_added(glib::clone!(
        #[strong]
        caps_matched,
        #[to_owned]
        media_type,
        #[to_owned]
        output_caps_name,
        move |_ws, pad| {
            if pad.name().contains(media_type.as_str()) {
                let caps = pad.allowed_caps().unwrap();

                assert_eq!(caps.structure(0).unwrap().name(), output_caps_name.as_str());
                caps_matched.store(true, Ordering::SeqCst);
                pad.push_event(gst::event::Eos::new());
            }
        }
    ));

    consumer
        .set_state(gst::State::Playing)
        .expect("changing to playing state");

    let bus = consumer.bus().unwrap();
    while let Some(msg) = bus.timed_pop(5.seconds()) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                break;
            }

            MessageView::Error(err) => panic!("{err:?}"),
            _ => {}
        }
    }
    assert!(caps_matched.load(Ordering::SeqCst));
    let consumer_stop = consumer.set_state(gst::State::Null).unwrap();
    assert_eq!(consumer_stop, gst::StateChangeSuccess::Success);

    let producer_stop = producer.set_state(gst::State::Null).unwrap();
    assert_eq!(producer_stop, gst::StateChangeSuccess::Success);
}

#[test]
#[file_serial(webrtctest)]
fn test_webrtcsrc_no_depayloading() {
    run_test(
        "videotestsrc ! vp8enc ! webrtcsink congestion-control=0 name=ws run-signalling-server=true",
        "webrtcsrc name=ws ! rtpvp8depay ! vp8dec ! fakesink",
        "application/x-rtp",
        "video",
        8444,
    );
}

#[test]
#[file_serial(webrtctest)]
fn test_webrtcsrc_no_decoding() {
    run_test(
        "videotestsrc ! vp8enc ! webrtcsink name=ws congestion-control=0 run-signalling-server=true",
        "webrtcsrc name=ws ! vp8dec ! fakesink",
        "video/x-vp8",
        "video",
        8445,
    );
}

#[test]
#[file_serial(webrtctest)]
fn test_webrtcsrc_decoding() {
    run_test(
        "videotestsrc ! vp8enc ! webrtcsink congestion-control=0 name=ws run-signalling-server=true",
        "webrtcsrc name=ws ! video/x-raw ! fakesink",
        "video/x-raw",
        "video",
        8446,
    );
}
