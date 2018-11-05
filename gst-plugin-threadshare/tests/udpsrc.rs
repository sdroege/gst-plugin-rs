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

extern crate glib;
use glib::prelude::*;

extern crate gstreamer as gst;
use gst::prelude::*;

use std::sync::{Arc, Mutex};
use std::thread;

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

    let pipeline = gst::Pipeline::new(None);
    let udpsrc = gst::ElementFactory::make("ts-udpsrc", None).unwrap();
    let appsink = gst::ElementFactory::make("appsink", None).unwrap();
    pipeline.add_many(&[&udpsrc, &appsink]).unwrap();
    udpsrc.link(&appsink).unwrap();

    let caps = gst::Caps::new_simple("foo/bar", &[]);
    udpsrc.set_property("caps", &caps).unwrap();
    udpsrc.set_property("port", &(5000 as u32)).unwrap();

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
            if samples.len() == 3 {
                let _ = appsink.post_message(&gst::Message::new_eos().src(Some(&appsink)).build());
            }

            Some(gst::FlowReturn::Ok.to_value())
        })
        .unwrap();

    pipeline
        .set_state(gst::State::Playing)
        .into_result()
        .unwrap();

    thread::spawn(move || {
        use std::net;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let buffer = [0; 160];
        let socket = net::UdpSocket::bind("0.0.0.0:0").unwrap();

        let ipaddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let dest = SocketAddr::new(ipaddr, 5000 as u16);

        for _ in 0..3 {
            socket.send_to(&buffer, dest).unwrap();
        }
    });

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
    assert_eq!(samples.len(), 3);

    for sample in samples.iter() {
        assert_eq!(sample.get_buffer().map(|b| b.get_size()), Some(160));
        assert_eq!(Some(&caps), sample.get_caps().as_ref());
    }

    pipeline.set_state(gst::State::Null).into_result().unwrap();
}
