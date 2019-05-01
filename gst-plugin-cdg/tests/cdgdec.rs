// Copyright (C) 2019 Guillaume Desmottes <guillaume.desmottes@collabora.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst_app::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use std::path::PathBuf;

use gstrscdg;

fn init() {
    use std::sync::{Once, ONCE_INIT};
    static INIT: Once = ONCE_INIT;

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrscdg::plugin_register_static().expect("cdgdec tests");
    });
}

#[test]
fn test_cdgdec() {
    init();

    let pipeline = gst::Pipeline::new(Some("cdgdec-test"));

    let input_path = {
        let mut r = PathBuf::new();
        r.push(env!("CARGO_MANIFEST_DIR"));
        r.push("tests");
        r.push("BrotherJohn");
        r.set_extension("cdg");
        r
    };

    let filesrc = gst::ElementFactory::make("filesrc", None).unwrap();
    filesrc
        .set_property("location", &input_path.to_str().unwrap())
        .expect("failed to set 'location' property");
    filesrc
        .set_property("num-buffers", &1)
        .expect("failed to set 'num-buffers' property");
    let blocksize: u32 = 24; // One CDG instruction
    filesrc
        .set_property("blocksize", &blocksize)
        .expect("failed to set 'blocksize' property");

    let dec = gst::ElementFactory::make("cdgdec", None).unwrap();
    let sink = gst::ElementFactory::make("appsink", None).unwrap();

    pipeline
        .add_many(&[&filesrc, &dec, &sink])
        .expect("failed to add elements to the pipeline");
    gst::Element::link_many(&[&filesrc, &dec, &sink]).expect("failed to link the elements");

    let sink = sink.downcast::<gst_app::AppSink>().unwrap();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::new()
            // Add a handler to the "new-sample" signal.
            .new_sample(move |appsink| {
                // Pull the sample in question out of the appsink's buffer.
                let sample = appsink.pull_sample().ok_or(gst::FlowError::Eos)?;
                let buffer = sample.get_buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().ok_or(gst::FlowError::Error)?;

                // First frame fully blue
                map.as_slice()
                    .chunks_exact(4)
                    .for_each(|p| assert_eq!(p, [0, 0, 136, 255]));

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to the `Playing` state");

    let bus = pipeline.get_bus().unwrap();
    for msg in bus.iter_timed(gst::CLOCK_TIME_NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Error(err) => {
                eprintln!(
                    "Error received from element {:?}: {}",
                    err.get_src().map(|s| s.get_path_string()),
                    err.get_error()
                );
                eprintln!("Debugging information: {:?}", err.get_debug());
                assert!(true);
                break;
            }
            MessageView::Eos(..) => break,
            _ => (),
        }
    }

    pipeline
        .set_state(gst::State::Null)
        .expect("Unable to set the pipeline to the `Null` state");
}
