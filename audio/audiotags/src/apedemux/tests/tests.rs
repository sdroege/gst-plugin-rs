// GStreamer ape tag demuxer - unit tests
//
// Copyright (C) 2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

use std::path::PathBuf;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("apetagdemux test");
    });
}

fn get_tags_from_file(filename: &str, blocksize: Option<u32>) -> (gst::Pipeline, gst::Message) {
    let pipeline = gst::Pipeline::default();

    let input_path = {
        let mut r = PathBuf::new();
        r.push(env!("CARGO_MANIFEST_DIR"));
        r.push("src");
        r.push("apedemux");
        r.push("tests");
        r.push(filename);
        r
    };

    gst::info!(crate::apedemux::apedemux::CAT, "Test file: {input_path:?}");

    let src = gst::ElementFactory::make("filesrc")
        .property("location", input_path.to_str().unwrap())
        .property_if_some("blocksize", blocksize)
        .build()
        .unwrap();

    let tagdemux = gst::ElementFactory::make("apetagdemux").build().unwrap();

    let sink = gst::ElementFactory::make("fakesink").build().unwrap();

    pipeline
        .add_many([src.upcast_ref(), &tagdemux, sink.upcast_ref()])
        .unwrap();

    src.link_pads(Some("src"), &tagdemux, Some("sink")).unwrap();

    tagdemux
        .link_pads(Some("src"), &sink, Some("sink"))
        .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    // Wait for tag message
    let msg = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(gst::ClockTime::NONE, &[gst::MessageType::Tag])
        .expect("tag message");

    (pipeline, msg)
}

#[test]
fn test_apedemux_end_tag_with_coverart() {
    init();

    let (pipeline, msg) = get_tags_from_file("ape-cover-art.mp3", None);

    let gst::MessageView::Tag(tag) = msg.view() else {
        unreachable!();
    };

    let tag_list = tag.tags();
    let tag_value = tag_list.get::<gst::tags::Title>().expect("title");
    assert_eq!(tag_value.get(), "Cover Test");

    let tag_value = tag_list.get::<gst::tags::Artist>().expect("artist");
    assert_eq!(tag_value.get(), "APE Artist");

    let tag_value = tag_list.get::<gst::tags::Image>().expect("image");
    {
        let sample = tag_value.get();

        let buffer = sample.buffer().unwrap();
        assert_eq!(buffer.size(), 32);

        let caps = sample.caps().unwrap();
        assert_eq!(caps, &gst::Caps::new_empty_simple("image/jpeg"));

        let info = sample.info().unwrap();
        println!("info: {info}");

        let filename = info.get::<&str>("filename").expect("filename");
        assert_eq!(filename, "cover.jpg");

        let img_type = info
            .get::<gst_tag::TagImageType>("image-type")
            .expect("image type");
        assert_eq!(img_type, gst_tag::TagImageType::FrontCover);
    }

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_apedemux_start_tag_push_mode() {
    init();

    // Silly block size, but makes sure things are handled right in the detect function
    let (pipeline, msg) = get_tags_from_file("ape-start-tag.mp3", Some(1));

    let gst::MessageView::Tag(tag) = msg.view() else {
        unreachable!();
    };

    let tag_list = tag.tags();
    assert_eq!(
        tag_list.get::<gst::tags::Title>().expect("title").get(),
        "Happe"
    );
    assert_eq!(
        tag_list.get::<gst::tags::Artist>().expect("artist").get(),
        "Alain Bashung"
    );
    assert_eq!(
        tag_list
            .get::<gst::tags::AlbumVolumeNumber>()
            .expect("disc")
            .get(),
        2u32
    );
    assert_eq!(
        tag_list
            .get::<gst::tags::AlbumVolumeCount>()
            .expect("dcount")
            .get(),
        9u32
    );
    assert_eq!(
        tag_list
            .get::<gst::tags::TrackNumber>()
            .expect("track")
            .get(),
        7u32
    );
    assert_eq!(
        tag_list
            .get::<gst::tags::TrackCount>()
            .expect("tcount")
            .get(),
        12u32
    );

    // Make sure we only set the year and nothing else
    let dt = tag_list
        .get::<gst::tags::DateTime>()
        .expect("datetime")
        .get();
    assert_eq!(dt.year(), 1995);
    assert_eq!(dt.month(), None);
    assert_eq!(dt.day(), None);
    assert!(!dt.has_time());

    // Lyrics
    let lyrics = tag_list.get::<gst::tags::Lyrics>().expect("lyrics");
    assert!(lyrics.get().starts_with("\nTu vois"));

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_apedemux_empty_tag() {
    init();

    let (pipeline, msg) = get_tags_from_file("empty-ape-tag.mpc", None);

    let gst::MessageView::Tag(tag) = msg.view() else {
        unreachable!();
    };

    let tag_list = tag.tags();
    assert_eq!(tag_list.n_tags(), 1);

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_apedemux_bogus_tag_size() {
    init();

    let (pipeline, msg) = get_tags_from_file("bogus-size-ape-tag.mpc", None);

    let gst::MessageView::Tag(tag) = msg.view() else {
        unreachable!();
    };

    let tag_list = tag.tags();
    assert_eq!(tag_list.n_tags(), 1);

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_apedemux_bogus_item_size() {
    init();

    let (pipeline, msg) = get_tags_from_file("ape-tag-with-bogus-item-value-size.mpc", None);

    let gst::MessageView::Tag(tag) = msg.view() else {
        unreachable!();
    };

    let tag_list = tag.tags();

    // Has to bail out after first item, can't parse second item
    assert_eq!(tag_list.n_tags(), 1);

    pipeline.set_state(gst::State::Null).unwrap();
}

// Checks that we just skip to the next item if there's an invalid item
#[test]
fn test_apedemux_invalid_text_value() {
    init();

    let (pipeline, msg) = get_tags_from_file("ape-tag-with-invalid-text-item.mpc", None);

    let gst::MessageView::Tag(tag) = msg.view() else {
        unreachable!();
    };

    let tag_list = tag.tags();
    assert_eq!(
        tag_list.get::<gst::tags::Title>().expect("title").get(),
        "xy"
    );
    assert_eq!(tag_list.n_tags(), 2);

    pipeline.set_state(gst::State::Null).unwrap();
}
