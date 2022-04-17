// Copyright (C) 2022 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use gstvideofx::{HashAlgorithm, VideoCompareMessage};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstvideofx::plugin_register_static().expect("Failed to register videofx plugin");
    });
}

fn setup_pipeline(
    pipeline: &gst::Pipeline,
    pattern_a: &str,
    pattern_b: &str,
    max_distance_threshold: f64,
    hash_algo: HashAlgorithm,
) {
    let videocompare = gst::ElementFactory::make("videocompare", None).unwrap();
    videocompare.set_property("max-dist-threshold", max_distance_threshold);
    videocompare.set_property("hash-algo", hash_algo);

    let reference_src = gst::ElementFactory::make("videotestsrc", Some("reference_src")).unwrap();
    reference_src.set_property_from_str("pattern", pattern_a);
    reference_src.set_property("num-buffers", 1i32);

    let secondary_src = gst::ElementFactory::make("videotestsrc", Some("secondary_src")).unwrap();
    reference_src.set_property_from_str("pattern", pattern_b);

    let sink = gst::ElementFactory::make("fakesink", None).unwrap();

    pipeline
        .add_many(&[&reference_src, &secondary_src, &videocompare, &sink])
        .unwrap();
    gst::Element::link_many(&[&reference_src, &videocompare, &sink]).expect("Link primary path");
    gst::Element::link_many(&[&secondary_src, &videocompare]).expect("Link secondary path");
}

#[test]
fn test_can_find_similar_frames() {
    init();

    // TODO: for some reason only in the tests, the distance is higher
    //       than when running via gst-launch tool for the same pipeline. What is happening?
    let max_distance = 32_f64;

    let pipeline = gst::Pipeline::new(None);
    setup_pipeline(
        &pipeline,
        "red",
        "red",
        max_distance,
        HashAlgorithm::Blockhash,
    );

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut detection = None;
    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Element(elt) => {
                if let Some(s) = elt.structure() {
                    if s.name() == "videocompare" {
                        detection = Some(
                            VideoCompareMessage::try_from(s.to_owned())
                                .expect("Can convert message to struct"),
                        );
                    }
                }
            }
            MessageView::Eos(..) => break,
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    let detection = detection.expect("Has found similar images");
    let pad_distance = detection
        .pad_distances()
        .iter()
        .find(|pd| pd.pad().name() == "sink_1")
        .unwrap();
    assert!(pad_distance.distance() <= max_distance);
}

#[test]
fn test_do_not_send_message_when_image_not_found() {
    init();

    let pipeline = gst::Pipeline::new(None);
    setup_pipeline(&pipeline, "black", "red", 0f64, HashAlgorithm::Blockhash);

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut detection = None;
    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Element(elt) => {
                if let Some(s) = elt.structure() {
                    if s.name() == "videocompare" {
                        detection = Some(
                            VideoCompareMessage::try_from(s.to_owned())
                                .expect("Can convert message to struct"),
                        );
                    }
                }
            }
            MessageView::Eos(..) => break,
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    assert!(detection.is_none());
}

#[cfg(feature = "dssim")]
#[test]
fn test_use_dssim_to_find_similar_frames() {
    init();

    let max_distance = 1_f64;

    let pipeline = gst::Pipeline::new(None);
    setup_pipeline(&pipeline, "red", "red", max_distance, HashAlgorithm::Dssim);

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut detection = None;
    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Element(elt) => {
                if let Some(s) = elt.structure() {
                    if s.name() == "videocompare" {
                        detection = Some(
                            VideoCompareMessage::try_from(s.to_owned())
                                .expect("Can convert message to struct"),
                        );
                    }
                }
            }
            MessageView::Eos(..) => break,
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    let detection = detection.expect("Has found similar images");
    let pad_distance = detection
        .pad_distances()
        .iter()
        .find(|pd| pd.pad().name() == "sink_1")
        .unwrap();
    assert!(pad_distance.distance() <= max_distance);
}
