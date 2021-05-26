// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
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

use gst::prelude::*;

use std::sync::{Arc, Mutex};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare proxy test");
    });
}

#[test]
fn test_push() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let fakesrc = gst::ElementFactory::make("fakesrc", None).unwrap();
    let proxysink = gst::ElementFactory::make("ts-proxysink", None).unwrap();
    let proxysrc = gst::ElementFactory::make("ts-proxysrc", None).unwrap();
    let appsink = gst::ElementFactory::make("appsink", None).unwrap();

    pipeline
        .add_many(&[&fakesrc, &proxysink, &proxysrc, &appsink])
        .unwrap();
    fakesrc.link(&proxysink).unwrap();
    proxysrc.link(&appsink).unwrap();

    fakesrc.set_property("num-buffers", &3i32).unwrap();
    proxysink.set_property("proxy-context", &"test1").unwrap();
    proxysrc.set_property("proxy-context", &"test1").unwrap();

    appsink.set_property("emit-signals", &true).unwrap();

    let samples = Arc::new(Mutex::new(Vec::new()));

    let appsink = appsink.dynamic_cast::<gst_app::AppSink>().unwrap();
    let samples_clone = samples.clone();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let sample = appsink
                    .emit_by_name("pull-sample", &[])
                    .unwrap()
                    .unwrap()
                    .get::<gst::Sample>()
                    .unwrap();

                samples_clone.lock().unwrap().push(sample);

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut eos = false;
    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.timed_pop(5 * gst::ClockTime::SECOND) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                eos = true;
                break;
            }
            MessageView::Error(err) => unreachable!("proxy::test_push {:?}", err),
            _ => (),
        }
    }

    assert!(eos);
    let samples = samples.lock().unwrap();
    assert_eq!(samples.len(), 3);

    for sample in samples.iter() {
        assert!(sample.buffer().is_some());
    }

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_from_pipeline_to_pipeline() {
    init();

    let pipe_1 = gst::Pipeline::new(None);
    let fakesrc = gst::ElementFactory::make("fakesrc", None).unwrap();
    let pxsink = gst::ElementFactory::make("ts-proxysink", None).unwrap();

    let pipe_2 = gst::Pipeline::new(None);
    let pxsrc = gst::ElementFactory::make("ts-proxysrc", None).unwrap();
    let fakesink = gst::ElementFactory::make("fakesink", None).unwrap();

    pipe_1.add_many(&[&fakesrc, &pxsink]).unwrap();
    fakesrc.link(&pxsink).unwrap();

    pipe_2.add_many(&[&pxsrc, &fakesink]).unwrap();
    pxsrc.link(&fakesink).unwrap();

    pxsink.set_property("proxy-context", &"test2").unwrap();
    pxsrc.set_property("proxy-context", &"test2").unwrap();

    pipe_1.set_state(gst::State::Paused).unwrap();
    pipe_2.set_state(gst::State::Paused).unwrap();

    let _ = pipe_1.state(gst::ClockTime::NONE);
    let _ = pipe_2.state(gst::ClockTime::NONE);

    pipe_1.set_state(gst::State::Null).unwrap();

    pipe_2.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_from_pipeline_to_pipeline_and_back() {
    init();

    let pipe_1 = gst::Pipeline::new(None);
    let pxsrc_1 = gst::ElementFactory::make("ts-proxysrc", None).unwrap();
    let pxsink_1 = gst::ElementFactory::make("ts-proxysink", None).unwrap();

    let pipe_2 = gst::Pipeline::new(None);
    let pxsrc_2 = gst::ElementFactory::make("ts-proxysrc", None).unwrap();
    let pxsink_2 = gst::ElementFactory::make("ts-proxysink", None).unwrap();

    pipe_1.add_many(&[&pxsrc_1, &pxsink_1]).unwrap();
    pxsrc_1.link(&pxsink_1).unwrap();

    pipe_2.add_many(&[&pxsrc_2, &pxsink_2]).unwrap();
    pxsrc_2.link(&pxsink_2).unwrap();

    pxsrc_1.set_property("proxy-context", &"test3").unwrap();
    pxsink_2.set_property("proxy-context", &"test3").unwrap();

    pxsrc_2.set_property("proxy-context", &"test4").unwrap();
    pxsink_1.set_property("proxy-context", &"test4").unwrap();

    pipe_1.set_state(gst::State::Paused).unwrap();
    pipe_2.set_state(gst::State::Paused).unwrap();

    pipe_1.set_state(gst::State::Null).unwrap();
    pipe_2.set_state(gst::State::Null).unwrap();
}
