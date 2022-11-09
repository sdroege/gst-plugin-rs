// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//

use gst::prelude::*;
use gst_pbutils::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstmp4::plugin_register_static().unwrap();
    });
}

#[test]
fn test_basic() {
    init();

    struct Pipeline(gst::Pipeline);
    impl std::ops::Deref for Pipeline {
        type Target = gst::Pipeline;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl Drop for Pipeline {
        fn drop(&mut self) {
            let _ = self.0.set_state(gst::State::Null);
        }
    }

    let pipeline = match gst::parse_launch(
        "videotestsrc num-buffers=99 ! x264enc ! mux. \
         audiotestsrc num-buffers=140 ! fdkaacenc ! mux. \
         isomp4mux name=mux ! filesink name=sink \
    ",
    ) {
        Ok(pipeline) => Pipeline(pipeline.downcast::<gst::Pipeline>().unwrap()),
        Err(_) => return,
    };

    let dir = tempfile::TempDir::new().unwrap();
    let mut location = dir.path().to_owned();
    location.push("test.mp4");

    let sink = pipeline.by_name("sink").unwrap();
    sink.set_property("location", location.to_str().expect("Non-UTF8 filename"));

    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to the `Playing` state");

    for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                panic!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            _ => (),
        }
    }

    pipeline
        .set_state(gst::State::Null)
        .expect("Unable to set the pipeline to the `Null` state");

    drop(pipeline);

    let discoverer = gst_pbutils::Discoverer::new(gst::ClockTime::from_seconds(5))
        .expect("Failed to create discoverer");
    let info = discoverer
        .discover_uri(
            url::Url::from_file_path(&location)
                .expect("Failed to convert filename to URL")
                .as_str(),
        )
        .expect("Failed to discover MP4 file");

    assert_eq!(info.duration(), Some(gst::ClockTime::from_mseconds(3_300)));

    let audio_streams = info.audio_streams();
    assert_eq!(audio_streams.len(), 1);
    let audio_stream = &audio_streams[0];
    assert_eq!(audio_stream.channels(), 1);
    assert_eq!(audio_stream.sample_rate(), 44_100);
    let caps = audio_stream.caps().unwrap();
    assert!(
        caps.can_intersect(
            &gst::Caps::builder("audio/mpeg")
                .any_features()
                .field("mpegversion", 4i32)
                .build()
        ),
        "Unexpected audio caps {:?}",
        caps
    );

    let video_streams = info.video_streams();
    assert_eq!(video_streams.len(), 1);
    let video_stream = &video_streams[0];
    assert_eq!(video_stream.width(), 320);
    assert_eq!(video_stream.height(), 240);
    assert_eq!(video_stream.framerate(), gst::Fraction::new(30, 1));
    assert_eq!(video_stream.par(), gst::Fraction::new(1, 1));
    assert!(!video_stream.is_interlaced());
    let caps = video_stream.caps().unwrap();
    assert!(
        caps.can_intersect(&gst::Caps::builder("video/x-h264").any_features().build()),
        "Unexpected video caps {:?}",
        caps
    );
}
