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

extern crate glib;
use glib::prelude::*;

extern crate gstreamer as gst;
use gst::prelude::*;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::{thread, time};

extern crate gstthreadshare;

fn init() {
    use std::sync::{Once, ONCE_INIT};
    static INIT: Once = ONCE_INIT;

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static();
    });
}

#[test]
fn test_push() {
    init();

    let handler = thread::spawn(move || {
        use std::net;

        let listener = net::TcpListener::bind("0.0.0.0:5000").unwrap();
        for stream in listener.incoming() {
            let buffer = [0; 160];
            let mut socket = stream.unwrap();
            for _ in 0..3 {
                let _ = socket.write(&buffer);
                thread::sleep(time::Duration::from_millis(20));
            }
            break;
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
    tcpclientsrc.set_property("port", &(5000u32)).unwrap();

    appsink.set_property("emit-signals", &true).unwrap();

    let samples = Arc::new(Mutex::new(Vec::new()));

    let samples_clone = samples.clone();
    appsink
        .connect("new-sample", true, move |args| {
            let appsink = args[0].get::<gst::Element>().unwrap();

            let sample = appsink
                .emit("pull-sample", &[])
                .unwrap()
                .unwrap()
                .get::<gst::Sample>()
                .unwrap();

            let mut samples = samples_clone.lock().unwrap();
            samples.push(sample);
            Some(gst::FlowReturn::Ok.to_value())
        })
        .unwrap();

    pipeline
        .set_state(gst::State::Playing)
        .into_result()
        .unwrap();

    let mut eos = false;
    let bus = pipeline.get_bus().unwrap();
    while let Some(msg) = bus.timed_pop(5 * gst::SECOND) {
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
    for sample in samples.iter() {
        assert_eq!(Some(&caps), sample.get_caps().as_ref());
    }

    let total_received_size = samples.iter().fold(0, |acc, sample| {
        acc + sample.get_buffer().unwrap().get_size()
    });
    assert_eq!(total_received_size, 3 * 160);

    pipeline.set_state(gst::State::Null).into_result().unwrap();

    handler.join().unwrap();
}
