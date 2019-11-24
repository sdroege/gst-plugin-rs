// Copyright (C) 2019 François Laignel <fengalin@free.fr>
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

use gst;

use gstthreadshare;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare pipeline test");
    });
}

#[test]
fn test_multiple_contexts() {
    use gst::prelude::*;

    use std::net;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    init();

    const CONTEXT_NB: u32 = 2;
    const SRC_NB: u16 = 4;
    const CONTEXT_WAIT: u32 = 1;
    const BUFFER_NB: u32 = 3;

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);

    let mut src_list = Vec::<gst::Element>::new();

    for i in 0..SRC_NB {
        let src =
            gst::ElementFactory::make("ts-udpsrc", Some(format!("src-{}", i).as_str())).unwrap();
        src.set_property("context", &format!("context-{}", (i as u32) % CONTEXT_NB))
            .unwrap();
        src.set_property("context-wait", &CONTEXT_WAIT).unwrap();
        src.set_property("port", &(40000u32 + (i as u32))).unwrap();

        let queue =
            gst::ElementFactory::make("ts-queue", Some(format!("queue-{}", i).as_str())).unwrap();
        queue
            .set_property("context", &format!("context-{}", (i as u32) % CONTEXT_NB))
            .unwrap();
        queue.set_property("context-wait", &CONTEXT_WAIT).unwrap();

        let fakesink =
            gst::ElementFactory::make("fakesink", Some(format!("sink-{}", i).as_str())).unwrap();
        fakesink.set_property("sync", &false).unwrap();
        fakesink.set_property("async", &false).unwrap();

        pipeline.add_many(&[&src, &queue, &fakesink]).unwrap();
        src.link(&queue).unwrap();
        queue.link(&fakesink).unwrap();

        src_list.push(src);
    }

    let bus = pipeline.get_bus().unwrap();
    let l_clone = l.clone();
    bus.add_watch(move |_, msg| {
        use gst::MessageView;

        match msg.view() {
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

    let pipeline_clone = pipeline.clone();
    let l_clone = l.clone();
    std::thread::spawn(move || {
        // Sleep to allow the pipeline to be ready
        std::thread::sleep(std::time::Duration::from_millis(50));

        let buffer = [0; 160];
        let socket = net::UdpSocket::bind("0.0.0.0:0").unwrap();

        let ipaddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let destinations = (40000..(40000 + SRC_NB))
            .map(|port| SocketAddr::new(ipaddr, port))
            .collect::<Vec<_>>();

        let wait = std::time::Duration::from_millis(CONTEXT_WAIT as u64);

        for _ in 0..BUFFER_NB {
            let now = std::time::Instant::now();

            for dest in &destinations {
                socket.send_to(&buffer, dest).unwrap();
            }

            let elapsed = now.elapsed();
            if elapsed < wait {
                std::thread::sleep(wait - elapsed);
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(50));

        pipeline_clone.set_state(gst::State::Null).unwrap();
        l_clone.quit();
    });

    pipeline.set_state(gst::State::Playing).unwrap();

    println!("starting...");

    l.run();
}

#[test]
fn test_premature_shutdown() {
    use gst::prelude::*;

    init();

    const CONTEXT_NAME: &str = "pipeline-context";
    const CONTEXT_WAIT: u32 = 1;
    const QUEUE_BUFFER_CAPACITY: u32 = 1;
    const BURST_NB: u32 = 2;

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);

    let caps = gst::Caps::new_simple("foo/bar", &[]);

    let appsrc = gst::ElementFactory::make("ts-appsrc", None).unwrap();
    appsrc.set_property("caps", &caps).unwrap();
    appsrc.set_property("do-timestamp", &true).unwrap();
    appsrc.set_property("context", &CONTEXT_NAME).unwrap();
    appsrc.set_property("context-wait", &CONTEXT_WAIT).unwrap();

    let queue = gst::ElementFactory::make("ts-queue", None).unwrap();
    queue.set_property("context", &CONTEXT_NAME).unwrap();
    queue.set_property("context-wait", &CONTEXT_WAIT).unwrap();
    queue
        .set_property("max-size-buffers", &QUEUE_BUFFER_CAPACITY)
        .unwrap();

    let fakesink = gst::ElementFactory::make("fakesink", None).unwrap();
    fakesink.set_property("sync", &false).unwrap();
    fakesink.set_property("async", &false).unwrap();

    pipeline.add_many(&[&appsrc, &queue, &fakesink]).unwrap();
    appsrc.link(&queue).unwrap();
    queue.link(&fakesink).unwrap();

    let bus = pipeline.get_bus().unwrap();
    let l_clone = l.clone();
    bus.add_watch(move |_, msg| {
        use gst::MessageView;

        match msg.view() {
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

    let pipeline_clone = pipeline.clone();
    let l_clone = l.clone();
    std::thread::spawn(move || {
        // Sleep to allow the pipeline to be ready
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Fill up the queue then pause a bit and push again
        let mut burst_idx = 0;
        loop {
            let was_pushed = appsrc
                .emit("push-buffer", &[&gst::Buffer::from_slice(vec![0; 1024])])
                .unwrap()
                .unwrap()
                .get_some::<bool>()
                .unwrap();

            if !was_pushed {
                if burst_idx < BURST_NB {
                    burst_idx += 1;
                    // Sleep a bit to let a few buffers go through
                    std::thread::sleep(std::time::Duration::from_micros(500));
                } else {
                    pipeline_clone.set_state(gst::State::Null).unwrap();
                    break;
                }
            }
        }

        l_clone.quit();
    });

    pipeline.set_state(gst::State::Playing).unwrap();

    println!("starting...");

    l.run();
}
