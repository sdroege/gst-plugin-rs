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
        gstthreadshare::plugin_register_static().expect("gstthreadshare queue test");
    });
}

#[test]
fn test_push() {
    init();

    let pipeline = gst::Pipeline::new(None);
    let fakesrc = gst::ElementFactory::make("fakesrc", None).unwrap();
    let queue = gst::ElementFactory::make("ts-queue", None).unwrap();
    let appsink = gst::ElementFactory::make("appsink", None).unwrap();

    pipeline.add_many(&[&fakesrc, &queue, &appsink]).unwrap();
    fakesrc.link(&queue).unwrap();
    queue.link(&appsink).unwrap();

    fakesrc.set_property("num-buffers", &3i32).unwrap();

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
            MessageView::Error(..) => unreachable!(),
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
