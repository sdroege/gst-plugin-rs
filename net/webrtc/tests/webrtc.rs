// Copyright (C) 2026, Taruntej Kanakamalla <tarun@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gstrswebrtc::signaller::{Signaller, WebRTCSignallerRole, prelude::*};
use serial_test::file_serial;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
    mpsc,
};
use std::time::Duration;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrswebrtc::plugin_register_static().expect("Register rswebrtc plugin");
    });
}

fn run_webrtc_producer(pipeline_str: &str, signaller_server_port: u16) -> gst::Pipeline {
    let pipeline = gst::parse::launch(pipeline_str)
        .expect("producer pipeline")
        .downcast::<gst::Pipeline>()
        .unwrap();
    let webrtcsink = pipeline.by_name("ws").unwrap();

    webrtcsink.set_property("signalling-server-port", signaller_server_port as u32);

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

    let signaller = Signaller::new(WebRTCSignallerRole::Listener);
    let uri = format!("ws://127.0.0.1:{signaller_server_port}");
    signaller.set_property("uri", uri.as_str());

    let producer = run_webrtc_producer(producer_pipeline_str, signaller_server_port);

    signaller.start();

    let (tx, rx) = mpsc::channel::<String>();

    signaller.connect("producer-added", false, move |args| {
        let peer_id = args[1].get::<String>().unwrap();
        tx.send(peer_id).unwrap();
        None
    });

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

/// Test that SDP renegotiation on webrtcsrc
///
/// This test starts with a simple producer pipeline with a single video stream.
/// After the initial negotiation completes, it dynamically adds a new video stream
/// to the producer, which should trigger SDP renegotiation. The test waits for the
/// new stream to be negotiated and ensures that the new pads are added to
/// webrtcsrc as expected.
#[test]
#[file_serial(webrtctest)]
fn test_webrtcsrc_renegotiation() {
    const SIGNALLER_PORT: u16 = 8447;

    init();

    let (tx, rx) = mpsc::channel::<String>();
    let producer = run_webrtc_producer(
        "videotestsrc ! vp8enc ! webrtcsink congestion-control=0 \
         enable-control-data-channel=true run-signalling-server=true name=ws",
        tx,
        SIGNALLER_PORT,
    );

    let producer_peer_id = rx.recv().unwrap();

    // Build consumer pipeline programmatically so we can handle dynamic pads
    let consumer = gst::Pipeline::builder().build();
    let webrtcsrc = gst::ElementFactory::make("webrtcsrc")
        .name("ws")
        .build()
        .unwrap();
    consumer.add(&webrtcsrc).unwrap();

    let signaller = webrtcsrc
        .dynamic_cast_ref::<gst::ChildProxy>()
        .unwrap()
        .child_by_name("signaller")
        .unwrap();
    signaller.set_property("producer-peer-id", producer_peer_id);

    let uri = format!("ws://127.0.0.1:{SIGNALLER_PORT}");
    signaller.set_property("uri", uri.as_str());

    let pad_count = Arc::new(AtomicU32::new(0));
    let initial_done = Arc::new((Mutex::new(false), Condvar::new()));
    let renegotiation_done = Arc::new((Mutex::new(false), Condvar::new()));

    webrtcsrc.connect_pad_added(glib::clone!(
        #[strong]
        pad_count,
        #[strong]
        initial_done,
        #[strong]
        renegotiation_done,
        move |ws, pad| {
            let sink = gst::ElementFactory::make("fakesink")
                .property("async", false)
                .build()
                .unwrap();
            let pipeline = ws.parent().unwrap().downcast::<gst::Pipeline>().unwrap();
            pipeline.add(&sink).unwrap();
            sink.sync_state_with_parent().unwrap();
            pad.link(&sink.static_pad("sink").unwrap()).unwrap();

            let count = pad_count.fetch_add(1, Ordering::SeqCst) + 1;
            if count == 1 {
                let (lock, cvar) = &*initial_done;
                let mut done = lock.lock().unwrap();
                *done = true;
                cvar.notify_one();
            } else {
                let (lock, cvar) = &*renegotiation_done;
                let mut done = lock.lock().unwrap();
                *done = true;
                cvar.notify_one();
            }
        }
    ));

    consumer
        .set_state(gst::State::Playing)
        .expect("consumer changing to playing state");

    // Wait for initial negotiation to complete
    {
        let (lock, cvar) = &*initial_done;
        let done = lock.lock().unwrap();
        let result = cvar
            .wait_timeout_while(done, Duration::from_secs(5), |done| !*done)
            .unwrap();
        assert!(*result.0, "Timed out waiting for initial negotiation");
    }

    // Let the connection stabilize before triggering renegotiation
    std::thread::sleep(Duration::from_secs(2));

    // Dynamically add a new video stream to the producer, triggering SDP renegotiation
    let webrtcsink = producer.by_name("ws").unwrap();
    let videotestsrc = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .build()
        .unwrap();
    let queue = gst::ElementFactory::make("queue").build().unwrap();

    producer.add_many([&videotestsrc, &queue]).unwrap();
    gst::Element::link_many([&videotestsrc, &queue]).unwrap();

    let new_pad = webrtcsink.request_pad_simple("video_%u").unwrap();
    queue.static_pad("src").unwrap().link(&new_pad).unwrap();

    queue.sync_state_with_parent().unwrap();
    videotestsrc.sync_state_with_parent().unwrap();

    // Wait for renegotiation
    {
        let (lock, cvar) = &*renegotiation_done;
        let done = lock.lock().unwrap();
        let result = cvar
            .wait_timeout_while(done, Duration::from_secs(2), |done| !*done)
            .unwrap();

        assert!(
            *result.0,
            "Timed out waiting for renegotiation, pad count is {}",
            pad_count.load(Ordering::SeqCst)
        );
    }

    assert!(
        pad_count.load(Ordering::SeqCst) >= 2,
        "Expected at least 2 pads (initial + renegotiation)"
    );

    let consumer_stop = consumer.set_state(gst::State::Null).unwrap();
    assert_eq!(consumer_stop, gst::StateChangeSuccess::Success);

    let producer_stop = producer.set_state(gst::State::Null).unwrap();
    assert_eq!(producer_stop, gst::StateChangeSuccess::Success);
}
