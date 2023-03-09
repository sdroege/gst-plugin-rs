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
//
// SPDX-License-Identifier: LGPL-2.1-or-later

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

    let pipeline = gst::Pipeline::default();

    let src = gst::ElementFactory::make("audiotestsrc")
        .name("audiotestsrc")
        .property("is-live", true)
        .property("num-buffers", BUFFER_NB)
        .build()
        .unwrap();

    let enc = gst::ElementFactory::make("alawenc")
        .name("alawenc")
        .build()
        .unwrap();
    let pay = gst::ElementFactory::make("rtppcmapay")
        .name("rtppcmapay")
        .build()
        .unwrap();

    let jb = gst::ElementFactory::make("ts-jitterbuffer")
        .name("ts-jitterbuffer")
        .property("context", "jb_pipeline")
        .property("context-wait", CONTEXT_WAIT)
        .property("latency", LATENCY)
        .build()
        .unwrap();

    let depay = gst::ElementFactory::make("rtppcmadepay")
        .name("rtppcmadepay")
        .build()
        .unwrap();
    let dec = gst::ElementFactory::make("alawdec")
        .name("alawdec")
        .build()
        .unwrap();

    let sink = gst_app::AppSink::builder()
        .name("appsink")
        .sync(false)
        .async_(false)
        .build();

    pipeline
        .add_many([&src, &enc, &pay, &jb, &depay, &dec, sink.upcast_ref()])
        .unwrap();
    gst::Element::link_many([&src, &enc, &pay, &jb, &depay, &dec, sink.upcast_ref()]).unwrap();

    let (sender, receiver) = mpsc::channel();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let _sample = appsink.pull_sample().unwrap();

                sender.send(()).unwrap();
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing).unwrap();

    gst::debug!(CAT, "jb_pipeline: waiting for {} buffers", BUFFER_NB);
    for idx in 0..BUFFER_NB {
        receiver.recv().unwrap();
        gst::debug!(CAT, "jb_pipeline: received buffer #{}", idx);
    }

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn jb_ts_pipeline() {
    init();

    const CONTEXT_WAIT: u32 = 20;
    const LATENCY: u32 = 20;
    const BUFFER_NB: i32 = 3;

    let pipeline = gst::Pipeline::default();

    let src = gst::ElementFactory::make("audiotestsrc")
        .name("audiotestsrc")
        .property("is-live", true)
        .property("num-buffers", BUFFER_NB)
        .build()
        .unwrap();

    let queue = gst::ElementFactory::make("ts-queue")
        .name("ts-queue")
        .property("context", "jb_ts_pipeline_queue")
        .property("context-wait", CONTEXT_WAIT)
        .build()
        .unwrap();

    let enc = gst::ElementFactory::make("alawenc")
        .name("alawenc")
        .build()
        .unwrap();
    let pay = gst::ElementFactory::make("rtppcmapay")
        .name("rtppcmapay")
        .build()
        .unwrap();

    let jb = gst::ElementFactory::make("ts-jitterbuffer")
        .name("ts-jitterbuffer")
        .property("context", "jb_ts_pipeline")
        .property("context-wait", CONTEXT_WAIT)
        .property("latency", LATENCY)
        .build()
        .unwrap();

    let depay = gst::ElementFactory::make("rtppcmadepay")
        .name("rtppcmadepay")
        .build()
        .unwrap();
    let dec = gst::ElementFactory::make("alawdec")
        .name("alawdec")
        .build()
        .unwrap();

    let sink = gst_app::AppSink::builder()
        .name("appsink")
        .sync(false)
        .async_(false)
        .build();

    pipeline
        .add_many([
            &src,
            &queue,
            &enc,
            &pay,
            &jb,
            &depay,
            &dec,
            sink.upcast_ref(),
        ])
        .unwrap();
    gst::Element::link_many([
        &src,
        &queue,
        &enc,
        &pay,
        &jb,
        &depay,
        &dec,
        sink.upcast_ref(),
    ])
    .unwrap();

    let (sender, receiver) = mpsc::channel();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let _sample = appsink.pull_sample().unwrap();

                sender.send(()).unwrap();
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing).unwrap();

    gst::debug!(CAT, "jb_ts_pipeline: waiting for {} buffers", BUFFER_NB);
    for idx in 0..BUFFER_NB {
        receiver.recv().unwrap();
        gst::debug!(CAT, "jb_ts_pipeline: received buffer #{}", idx);
    }

    pipeline.set_state(gst::State::Null).unwrap();
}
