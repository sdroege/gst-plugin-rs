// Copyright (C) 2020 François Laignel <fengalin@free.fr>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use gst::gst_debug;
use gst::prelude::*;

use std::sync::mpsc;

use once_cell::sync::Lazy;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-test",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing test"),
    )
});

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare jitterbuffer test");
    });
}

#[test]
fn jb_pipeline() {
    init();

    const CONTEXT_WAIT: u32 = 20;
    const LATENCY: u32 = 20;
    const BUFFER_NB: i32 = 3;

    let pipeline = gst::Pipeline::new(None);

    let src = gst::ElementFactory::make("audiotestsrc", Some("audiotestsrc")).unwrap();
    src.set_property("is-live", &true).unwrap();
    src.set_property("num-buffers", &BUFFER_NB).unwrap();

    let enc = gst::ElementFactory::make("alawenc", Some("alawenc")).unwrap();
    let pay = gst::ElementFactory::make("rtppcmapay", Some("rtppcmapay")).unwrap();

    let jb = gst::ElementFactory::make("ts-jitterbuffer", Some("ts-jitterbuffer")).unwrap();
    jb.set_property("context", &"jb_pipeline").unwrap();
    jb.set_property("context-wait", &CONTEXT_WAIT).unwrap();
    jb.set_property("latency", &LATENCY).unwrap();

    let depay = gst::ElementFactory::make("rtppcmadepay", Some("rtppcmadepay")).unwrap();
    let dec = gst::ElementFactory::make("alawdec", Some("alawdec")).unwrap();

    let sink = gst::ElementFactory::make("appsink", Some("appsink")).unwrap();
    sink.set_property("sync", &false).unwrap();
    sink.set_property("async", &false).unwrap();
    sink.set_property("emit-signals", &true).unwrap();

    pipeline
        .add_many(&[&src, &enc, &pay, &jb, &depay, &dec, &sink])
        .unwrap();
    gst::Element::link_many(&[&src, &enc, &pay, &jb, &depay, &dec, &sink]).unwrap();

    let appsink = sink.dynamic_cast::<gst_app::AppSink>().unwrap();
    let (sender, receiver) = mpsc::channel();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let _sample = appsink
                    .emit_by_name("pull-sample", &[])
                    .unwrap()
                    .unwrap()
                    .get::<gst::Sample>()
                    .unwrap();

                sender.send(()).unwrap();
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing).unwrap();

    gst_debug!(CAT, "jb_pipeline: waiting for {} buffers", BUFFER_NB);
    for idx in 0..BUFFER_NB {
        receiver.recv().unwrap();
        gst_debug!(CAT, "jb_pipeline: received buffer #{}", idx);
    }

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn jb_ts_pipeline() {
    init();

    const CONTEXT_WAIT: u32 = 20;
    const LATENCY: u32 = 20;
    const BUFFER_NB: i32 = 3;

    let pipeline = gst::Pipeline::new(None);

    let src = gst::ElementFactory::make("audiotestsrc", Some("audiotestsrc")).unwrap();
    src.set_property("is-live", &true).unwrap();
    src.set_property("num-buffers", &BUFFER_NB).unwrap();

    let queue = gst::ElementFactory::make("ts-queue", Some("ts-queue")).unwrap();
    queue
        .set_property("context", &"jb_ts_pipeline_queue")
        .unwrap();
    queue.set_property("context-wait", &CONTEXT_WAIT).unwrap();

    let enc = gst::ElementFactory::make("alawenc", Some("alawenc")).unwrap();
    let pay = gst::ElementFactory::make("rtppcmapay", Some("rtppcmapay")).unwrap();

    let jb = gst::ElementFactory::make("ts-jitterbuffer", Some("ts-jitterbuffer")).unwrap();
    jb.set_property("context", &"jb_ts_pipeline").unwrap();
    jb.set_property("context-wait", &CONTEXT_WAIT).unwrap();
    jb.set_property("latency", &LATENCY).unwrap();

    let depay = gst::ElementFactory::make("rtppcmadepay", Some("rtppcmadepay")).unwrap();
    let dec = gst::ElementFactory::make("alawdec", Some("alawdec")).unwrap();

    let sink = gst::ElementFactory::make("appsink", Some("appsink")).unwrap();
    sink.set_property("sync", &false).unwrap();
    sink.set_property("async", &false).unwrap();
    sink.set_property("emit-signals", &true).unwrap();

    pipeline
        .add_many(&[&src, &queue, &enc, &pay, &jb, &depay, &dec, &sink])
        .unwrap();
    gst::Element::link_many(&[&src, &queue, &enc, &pay, &jb, &depay, &dec, &sink]).unwrap();

    let appsink = sink.dynamic_cast::<gst_app::AppSink>().unwrap();
    let (sender, receiver) = mpsc::channel();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let _sample = appsink
                    .emit_by_name("pull-sample", &[])
                    .unwrap()
                    .unwrap()
                    .get::<gst::Sample>()
                    .unwrap();

                sender.send(()).unwrap();
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing).unwrap();

    gst_debug!(CAT, "jb_ts_pipeline: waiting for {} buffers", BUFFER_NB);
    for idx in 0..BUFFER_NB {
        receiver.recv().unwrap();
        gst_debug!(CAT, "jb_ts_pipeline: received buffer #{}", idx);
    }

    pipeline.set_state(gst::State::Null).unwrap();
}
