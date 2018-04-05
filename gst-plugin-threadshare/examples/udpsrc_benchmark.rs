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

extern crate glib as glib;
use glib::prelude::*;

extern crate gstreamer as gst;
use gst::prelude::*;

use std::{env, thread, time};

fn main() {
    gst::init().unwrap();

    #[cfg(debug_assertions)]
    {
        use std::path::Path;

        let mut path = Path::new("target/debug");
        if !path.exists() {
            path = Path::new("../target/debug");
        }

        gst::Registry::get().scan_path(path);
    }
    #[cfg(not(debug_assertions))]
    {
        use std::path::Path;

        let mut path = Path::new("target/release");
        if !path.exists() {
            path = Path::new("../target/release");
        }

        gst::Registry::get().scan_path(path);
    }

    let args = env::args().collect::<Vec<_>>();
    assert_eq!(args.len(), 6);
    let n_streams: u16 = args[1].parse().unwrap();
    let source = &args[2];
    let n_threads: i32 = args[3].parse().unwrap();
    let n_groups: u32 = args[4].parse().unwrap();
    let wait: u32 = args[5].parse().unwrap();

    if source == "udpsrc" || source == "ts-udpsrc" {
        // Start our thread that just sends packets forever to the ports

        thread::spawn(move || {
            use std::net;
            use std::net::{IpAddr, Ipv4Addr, SocketAddr};

            let buffer = [0; 160];
            let socket = net::UdpSocket::bind("0.0.0.0:0").unwrap();

            let ipaddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
            let destinations = (40000..(40000 + n_streams))
                .map(|port| SocketAddr::new(ipaddr, port))
                .collect::<Vec<_>>();

            let wait = time::Duration::from_millis(wait as u64);

            thread::sleep(time::Duration::from_millis(1000));

            loop {
                let now = time::Instant::now();

                for dest in &destinations {
                    socket.send_to(&buffer, dest).unwrap();
                }

                let elapsed = now.elapsed();
                if elapsed < wait {
                    thread::sleep(wait - elapsed);
                }
            }
        });
    }

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);

    for i in 0..n_streams {
        let fakesink =
            gst::ElementFactory::make("fakesink", Some(format!("sink-{}", i).as_str())).unwrap();
        fakesink.set_property("sync", &false).unwrap();
        fakesink.set_property("async", &false).unwrap();

        let source = match source.as_str() {
            "udpsrc" => {
                let source =
                    gst::ElementFactory::make("udpsrc", Some(format!("source-{}", i).as_str()))
                        .unwrap();
                source
                    .set_property("port", &(40000i32 + (i as i32)))
                    .unwrap();
                source
                    .set_property("retrieve-sender-address", &false)
                    .unwrap();

                source
            }
            "ts-udpsrc" => {
                let source =
                    gst::ElementFactory::make("ts-udpsrc", Some(format!("source-{}", i).as_str()))
                        .unwrap();
                source
                    .set_property("port", &(40000u32 + (i as u32)))
                    .unwrap();
                source
                    .set_property("context", &format!("context-{}", (i as u32) % n_groups))
                    .unwrap();
                source.set_property("context-threads", &n_threads).unwrap();
                source.set_property("context-wait", &wait).unwrap();

                source
            }
            "tonegeneratesrc" => {
                let source = gst::ElementFactory::make(
                    "tonegeneratesrc",
                    Some(format!("source-{}", i).as_str()),
                ).unwrap();
                source
                    .set_property("samplesperbuffer", &((wait as i32) * 8000 / 1000))
                    .unwrap();

                fakesink.set_property("sync", &true).unwrap();

                source
            }
            "ts-tonesrc" => {
                let source =
                    gst::ElementFactory::make("ts-tonesrc", Some(format!("source-{}", i).as_str()))
                        .unwrap();
                source
                    .set_property("samples-per-buffer", &((wait as u32) * 8000 / 1000))
                    .unwrap();
                source
                    .set_property("context", &format!("context-{}", (i as u32) % n_groups))
                    .unwrap();
                source.set_property("context-threads", &n_threads).unwrap();
                source.set_property("context-wait", &wait).unwrap();

                source
            }
            _ => unimplemented!(),
        };

        pipeline.add_many(&[&source, &fakesink]).unwrap();
        source.link(&fakesink).unwrap();
    }

    let bus = pipeline.get_bus().unwrap();
    let l_clone = l.clone();
    bus.add_watch(move |_, msg| {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => l_clone.quit(),
            MessageView::Error(err) => {
                println!(
                    "Error from {:?}: {} ({:?})",
                    err.get_src().map(|s| s.get_path_string()),
                    err.get_error(),
                    err.get_debug()
                );
                l_clone.quit();
            }
            _ => (),
        };

        glib::Continue(true)
    });

    assert_ne!(
        pipeline.set_state(gst::State::Playing),
        gst::StateChangeReturn::Failure
    );

    println!("started");

    l.run();
}
