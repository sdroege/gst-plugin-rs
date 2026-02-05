// Copyright (C) 2026, Taruntej Kanakamalla <tarun@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use anyhow::Error;
use gst::glib;
use gst::prelude::*;
use gst_plugin_webrtc_signalling::handlers::Handler;
use gst_plugin_webrtc_signalling::server::Server;
use gstrswebrtc::RUNTIME;
use serial_test::file_serial;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;
use tokio::net::TcpListener;

async fn spawn_signalling_server(tx: mpsc::Sender<Result<(), Error>>) -> Result<(), Error> {
    let addr = String::from("0.0.0.0:8443");
    let server = Server::spawn(Handler::new);
    let Ok(listener) = TcpListener::bind(&addr).await else {
        let err = anyhow::anyhow!("failed to bind");
        let _ = tx.send(Err(err));
        return Err(anyhow::anyhow!("failed to bind"));
    };

    let _ = tx.send(Ok(()));
    while let Ok((stream, _address)) = listener.accept().await {
        let mut server_clone = server.clone();
        RUNTIME.spawn(async move { server_clone.accept_async(stream).await });
    }

    Ok(())
}

fn init() -> Option<tokio::task::JoinHandle<Result<(), anyhow::Error>>> {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrswebrtc::plugin_register_static().expect("Register rswebrtc plugin");
    });

    let (tx, rx) = std::sync::mpsc::channel();

    let handle = Some(RUNTIME.spawn(async move {
        spawn_signalling_server(tx)
            .await
            .inspect_err(|e| panic!("{e:?}"))
    }));

    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            panic!("Failed to start signalling server : {e:?}");
        }
        Err(e) => {
            panic!("Timed out waiting for the signalling server to start :{e:?}");
        }
    }

    handle
}

fn run_webrtc_producer(pipeline_str: &str, tx: mpsc::Sender<String>) -> gst::Pipeline {
    let pipeline = gst::parse::launch(pipeline_str)
        .expect("producer pipeline")
        .downcast::<gst::Pipeline>()
        .unwrap();
    let webrtcsink = pipeline.by_name("ws").unwrap();

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
) {
    let mut h = init();

    let (tx, rx) = mpsc::channel::<String>();
    let producer = run_webrtc_producer(producer_pipeline_str, tx);

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

    if let Some(h) = h.take() {
        h.abort();
    };
}

#[test]
#[file_serial(webrtctest)]
fn test_webrtcsrc_no_depayloading() {
    run_test(
        "videotestsrc ! vp8enc ! webrtcsink congestion-control=0 name=ws",
        "webrtcsrc name=ws ! rtpvp8depay ! vp8dec ! fakesink",
        "application/x-rtp",
        "video",
    );
}

#[test]
#[file_serial(webrtctest)]
fn test_webrtcsrc_no_decoding() {
    run_test(
        "videotestsrc ! vp8enc ! webrtcsink name=ws congestion-control=0",
        "webrtcsrc name=ws ! vp8dec ! fakesink",
        "video/x-vp8",
        "video",
    );
}

#[test]
#[file_serial(webrtctest)]
fn test_webrtcsrc_decoding() {
    run_test(
        "videotestsrc ! vp8enc ! webrtcsink congestion-control=0 name=ws",
        "webrtcsrc name=ws ! video/x-raw ! fakesink",
        "video/x-raw",
        "video",
    );
}
