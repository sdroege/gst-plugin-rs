// SPDX-License-Identifier: LGPL-2.1-or-later

use gst::prelude::*;

use gstrswebrtc::webrtcsink::WebRTCSinkCongestionControl;

use std::sync::{Arc, Condvar, LazyLock, Mutex, mpsc};

mod direct_signaller;
use direct_signaller::DirectSignaller;

mod stamper;
use stamper::{StampChecker, Stamper};

pub static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "webrtc-encoded-filter-test",
        gst::DebugColorFlags::empty(),
        Some("WebRTC encoded filter Test Suite"),
    )
});

#[test]
fn encoded_filter() {
    gst::init().unwrap();
    gstrswebrtc::plugin_register_static().unwrap();

    let decoding_caps = gst_audio::AudioCapsBuilder::new()
        .channels(1)
        .rate(8_000)
        .build();

    let passthrough_caps = gst::Caps::new_any();

    let no_extra_configuration = |_: &gst::Element| {};
    run_scenario(
        "minimal_end_2_end decoding",
        &decoding_caps,
        no_extra_configuration,
        no_extra_configuration,
    );
    run_scenario(
        "minimal_end_2_end not-decoding",
        &passthrough_caps,
        no_extra_configuration,
        no_extra_configuration,
    );

    run_scenario(
        "encoded_filter decoding",
        &decoding_caps,
        configure_encoded_filter_for_wsink,
        configure_encoded_filter_for_wsrc,
    );
    run_scenario(
        "encoded_filter not-decoding",
        &passthrough_caps,
        configure_encoded_filter_for_wsink,
        configure_encoded_filter_for_wsrc,
    );
}

fn configure_encoded_filter_for_wsink(wsink: &gst::Element) {
    wsink.connect("request-encoded-filter", false, move |args| {
        let consumer_id = args[1].get::<Option<String>>().unwrap();
        let pad_name = args[2].get::<String>().unwrap();
        let caps = args[3].get::<gst::Caps>().unwrap();
        assert!(pad_name.starts_with("audio_"));
        gst::debug!(
            CAT,
            "Producer calling `request-encoded-filter` for consumer {consumer_id:?}, pad {pad_name}, caps {caps:?}",
        );

        Some(Stamper::default().into())
    });
}

fn configure_encoded_filter_for_wsrc(wsrc: &gst::Element) {
    wsrc.connect("request-encoded-filter", false, move |args| {
        let producer_id = args[1].get::<Option<String>>().unwrap();
        let pad_name = args[2].get::<String>().unwrap();
        assert!(pad_name.starts_with("audio_"));
        let caps = args[3].get::<gst::Caps>().unwrap();
        gst::debug!(
            CAT,
            "Consumer calling `request-encoded-filter` for producer {producer_id:?}, pad {pad_name}, caps {caps:?}",
        );

        Some(StampChecker::default().into())
    });
}

pub fn run_scenario<F, G>(test: &str, sink_caps: &gst::Caps, configure_wsink: F, configure_wsrc: G)
where
    F: FnOnce(&gst::Element),
    G: FnOnce(&gst::Element),
{
    gst::debug!(CAT, "Starting {test}");

    // webrtcsink
    let pipeline_sink = gst::Pipeline::builder().name("pipeline_sink").build();
    let audio_src = gst::ElementFactory::make("audiotestsrc").build().unwrap();
    let wsink_signaller = DirectSignaller::new(&format!("{test} sink"));
    let wsink = gst::ElementFactory::make("webrtcsink")
        .property("signaller", wsink_signaller.clone())
        .property("congestion-control", WebRTCSinkCongestionControl::Disabled)
        .property("stun-server", None::<String>)
        .build()
        .unwrap();

    configure_wsink(&wsink);

    let elems = [&audio_src, wsink.upcast_ref()];
    pipeline_sink.add_many(elems).unwrap();
    gst::Element::link_many(elems).unwrap();
    pipeline_sink.set_state(gst::State::Playing).unwrap();

    let prod_started_cvar_pair = Arc::new((Mutex::new(false), Condvar::new()));
    wsink_signaller.connect("started", false, {
        let prod_started_cvar_pair = prod_started_cvar_pair.clone();
        move |_| {
            let (lock, cvar) = &*prod_started_cvar_pair;
            let mut prod_started = lock.lock().unwrap();
            *prod_started = true;
            cvar.notify_one();

            None
        }
    });

    gst::debug!(CAT, "{test} awaiting for prod to start");
    let (lock, cvar) = &*prod_started_cvar_pair;
    let mut prod_started = lock.lock().unwrap();
    while !*prod_started {
        prod_started = cvar.wait(prod_started).unwrap();
    }

    // webrtcsrc
    let wsrc_signaller = DirectSignaller::new(&format!("{test} src"));
    DirectSignaller::associate(&wsink_signaller, &wsrc_signaller);

    let wsrc = gst::ElementFactory::make("webrtcsrc")
        .property("signaller", wsrc_signaller.clone())
        .property("stun-server", None::<String>)
        .build()
        .unwrap();

    configure_wsrc(&wsrc);

    let (pad_tx, pad_rx) = mpsc::channel();
    let pad_added_cvar_pair = Arc::new((Mutex::new(false), Condvar::new()));
    wsrc.connect_pad_added({
        let test = test.to_string();
        let pad_tx = Arc::new(Mutex::new(pad_tx));
        let pad_added_cvar_pair = pad_added_cvar_pair.clone();
        move |_, pad| {
            gst::debug!(CAT, "{test} got Pad: {}", pad.name());

            pad_tx.lock().unwrap().send(pad.clone()).unwrap();

            let (lock, cvar) = &*pad_added_cvar_pair;
            let mut pad_added = lock.lock().unwrap();
            while !*pad_added {
                pad_added = cvar.wait(pad_added).unwrap();
            }

            gst::debug!(CAT, "{test} pad added notified");
        }
    });

    let mut h_src = gst_check::Harness::with_element(&wsrc, None, None);
    h_src.use_systemclock();
    h_src.set_sink_caps(sink_caps.clone());

    h_src.play();

    gst::debug!(CAT, "{test} requesting session");
    wsrc_signaller.request_session();

    gst::debug!(CAT, "{test} awaiting Pad from WebRTCSrc");
    let pad = pad_rx.recv().unwrap();
    h_src.add_element_src_pad(&pad);
    gst::debug!(CAT, "{test} WebRTCSrc Pad linked to harness");

    let (lock, cvar) = &*pad_added_cvar_pair;
    let mut pad_added = lock.lock().unwrap();
    *pad_added = true;
    cvar.notify_one();
    drop(pad_added);

    gst::debug!(CAT, "{test} pulling initial events");
    assert_eq!(
        h_src.pull_event().unwrap().type_(),
        gst::EventType::StreamStart,
    );
    assert_eq!(h_src.pull_event().unwrap().type_(), gst::EventType::Caps);
    assert_eq!(h_src.pull_event().unwrap().type_(), gst::EventType::Segment);
    if !sink_caps.is_any() {
        assert_eq!(
            h_src.pull_event().unwrap().type_(),
            gst::EventType::StreamCollection
        );
    }

    gst::debug!(CAT, "{test} awaiting Buffer from WebRTCSrc");
    let _ = h_src.pull().unwrap();
    gst::info!(CAT, "{test} got Buffer");

    gst::debug!(CAT, "{test} tearing down Harness for WebRTCSrc");
    drop(h_src);

    gst::debug!(CAT, "{test} setting Pipeline for WebRTCSink to Null");
    pipeline_sink.set_state(gst::State::Null).unwrap();

    gst::debug!(CAT, "{test} complete");
}
