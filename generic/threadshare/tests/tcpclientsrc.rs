// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2018 LEE Dongjun <redongjun@gmail.com>
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

use std::io::Write;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::{thread, time};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare tcpclientsrc test");
    });
}

#[test]
fn test_push() {
    init();

    let (listening_tx, listening_rx) = mpsc::channel();
    let handler = thread::spawn(move || {
        use std::net;

        let listener = net::TcpListener::bind("0.0.0.0:5000").unwrap();
        listening_tx.send(()).unwrap();
        let stream = listener.incoming().next().unwrap();
        let buffer = [0; 160];
        let mut socket = stream.unwrap();
        for _ in 0..3 {
            let _ = socket.write(&buffer);
            thread::sleep(time::Duration::from_millis(20));
        }
    });

    let pipeline = gst::Pipeline::new(None);

    let tcpclientsrc = gst::ElementFactory::make("ts-tcpclientsrc", None).unwrap();
    let appsink = gst::ElementFactory::make("appsink", None).unwrap();
    appsink.set_property("sync", &false).unwrap();
    appsink.set_property("async", &false).unwrap();

    pipeline.add_many(&[&tcpclientsrc, &appsink]).unwrap();
    tcpclientsrc.link(&appsink).unwrap();

    let caps = gst::Caps::new_simple("foo/bar", &[]);
    tcpclientsrc.set_property("caps", &caps).unwrap();
    tcpclientsrc.set_property("port", &5000i32).unwrap();

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

                let mut samples = samples_clone.lock().unwrap();
                samples.push(sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    // Wait for the server to listen
    listening_rx.recv().unwrap();
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
            MessageView::Error(err) => panic!("{:?}", err),
            _ => (),
        }
    }

    assert!(eos);
    let samples = samples.lock().unwrap();
    for sample in samples.iter() {
        assert_eq!(Some(caps.as_ref()), sample.caps());
    }

    let total_received_size = samples
        .iter()
        .fold(0, |acc, sample| acc + sample.buffer().unwrap().size());
    assert_eq!(total_received_size, 3 * 160);

    pipeline.set_state(gst::State::Null).unwrap();

    handler.join().unwrap();
}
