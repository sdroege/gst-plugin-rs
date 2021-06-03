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

use gst::glib;
use gst::prelude::*;
use gst::{gst_debug, gst_error};

use once_cell::sync::Lazy;

use std::sync::mpsc;

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
        gstthreadshare::plugin_register_static().expect("gstthreadshare pipeline test");
    });
}

#[test]
fn multiple_contexts_queue() {
    use std::net;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::mpsc;

    init();

    const CONTEXT_NB: u32 = 2;
    const SRC_NB: u16 = 4;
    const CONTEXT_WAIT: u32 = 1;
    const BUFFER_NB: u32 = 3;
    const FIRST_PORT: u16 = 40000;

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);

    let (sender, receiver) = mpsc::channel();

    for i in 0..SRC_NB {
        let src =
            gst::ElementFactory::make("ts-udpsrc", Some(format!("src-{}", i).as_str())).unwrap();
        src.set_property("context", &format!("context-{}", (i as u32) % CONTEXT_NB))
            .unwrap();
        src.set_property("context-wait", &CONTEXT_WAIT).unwrap();
        src.set_property("port", &((FIRST_PORT + i) as i32))
            .unwrap();

        let queue =
            gst::ElementFactory::make("ts-queue", Some(format!("queue-{}", i).as_str())).unwrap();
        queue
            .set_property("context", &format!("context-{}", (i as u32) % CONTEXT_NB))
            .unwrap();
        queue.set_property("context-wait", &CONTEXT_WAIT).unwrap();

        let sink =
            gst::ElementFactory::make("appsink", Some(format!("sink-{}", i).as_str())).unwrap();
        sink.set_property("sync", &false).unwrap();
        sink.set_property("async", &false).unwrap();
        sink.set_property("emit-signals", &true).unwrap();

        pipeline.add_many(&[&src, &queue, &sink]).unwrap();
        gst::Element::link_many(&[&src, &queue, &sink]).unwrap();

        let appsink = sink.dynamic_cast::<gst_app::AppSink>().unwrap();
        let sender_clone = sender.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let _sample = appsink
                        .emit_by_name("pull-sample", &[])
                        .unwrap()
                        .unwrap()
                        .get::<gst::Sample>()
                        .unwrap();

                    sender_clone.send(()).unwrap();
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }

    let pipeline_clone = pipeline.clone();
    let l_clone = l.clone();
    let mut test_scenario = Some(move || {
        let buffer = [0; 160];
        let socket = net::UdpSocket::bind("0.0.0.0:0").unwrap();

        let ipaddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let destinations = (FIRST_PORT..(FIRST_PORT + SRC_NB))
            .map(|port| SocketAddr::new(ipaddr, port))
            .collect::<Vec<_>>();

        for _ in 0..BUFFER_NB {
            for dest in &destinations {
                gst_debug!(CAT, "multiple_contexts_queue: sending buffer to {:?}", dest);
                socket.send_to(&buffer, dest).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(CONTEXT_WAIT as u64));
            }
        }

        gst_debug!(
            CAT,
            "multiple_contexts_queue: waiting for all buffers notifications"
        );
        for _ in 0..(BUFFER_NB * (SRC_NB as u32)) {
            receiver.recv().unwrap();
        }

        pipeline_clone.set_state(gst::State::Null).unwrap();
        l_clone.quit();
    });

    let bus = pipeline.bus().unwrap();
    let l_clone = l.clone();
    bus.add_watch(move |_, msg| {
        use gst::MessageView;

        match msg.view() {
            MessageView::StateChanged(state_changed) => {
                if let Some(source) = state_changed.src() {
                    if source.type_() == gst::Pipeline::static_type()
                        && state_changed.old() == gst::State::Paused
                        && state_changed.current() == gst::State::Playing
                    {
                        if let Some(test_scenario) = test_scenario.take() {
                            std::thread::spawn(test_scenario);
                        }
                    }
                }
            }
            MessageView::Error(err) => {
                gst_error!(
                    CAT,
                    "multiple_contexts_queue: Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                l_clone.quit();
            }
            _ => (),
        };

        glib::Continue(true)
    })
    .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    gst_debug!(CAT, "Starting main loop for multiple_contexts_queue...");
    l.run();
    gst_debug!(CAT, "Stopping main loop for multiple_contexts_queue...");
}

#[test]
fn multiple_contexts_proxy() {
    use std::net;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    init();

    const CONTEXT_NB: u32 = 2;
    const SRC_NB: u16 = 4;
    const CONTEXT_WAIT: u32 = 1;
    const BUFFER_NB: u32 = 3;
    // Don't overlap with `multiple_contexts_queue`
    const OFFSET: u16 = 10;
    const FIRST_PORT: u16 = 40000 + OFFSET;

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);

    let (sender, receiver) = mpsc::channel();

    for i in 0..SRC_NB {
        let pipeline_index = i + OFFSET;

        let src = gst::ElementFactory::make(
            "ts-udpsrc",
            Some(format!("src-{}", pipeline_index).as_str()),
        )
        .unwrap();
        src.set_property("context", &format!("context-{}", (i as u32) % CONTEXT_NB))
            .unwrap();
        src.set_property("context-wait", &CONTEXT_WAIT).unwrap();
        src.set_property("port", &((FIRST_PORT + i) as i32))
            .unwrap();

        let proxysink = gst::ElementFactory::make(
            "ts-proxysink",
            Some(format!("proxysink-{}", pipeline_index).as_str()),
        )
        .unwrap();
        proxysink
            .set_property("proxy-context", &format!("proxy-{}", pipeline_index))
            .unwrap();
        let proxysrc = gst::ElementFactory::make(
            "ts-proxysrc",
            Some(format!("proxysrc-{}", pipeline_index).as_str()),
        )
        .unwrap();
        proxysrc
            .set_property(
                "context",
                &format!("context-{}", (pipeline_index as u32) % CONTEXT_NB),
            )
            .unwrap();
        proxysrc
            .set_property("proxy-context", &format!("proxy-{}", pipeline_index))
            .unwrap();

        let sink =
            gst::ElementFactory::make("appsink", Some(format!("sink-{}", pipeline_index).as_str()))
                .unwrap();
        sink.set_property("sync", &false).unwrap();
        sink.set_property("async", &false).unwrap();
        sink.set_property("emit-signals", &true).unwrap();

        pipeline
            .add_many(&[&src, &proxysink, &proxysrc, &sink])
            .unwrap();
        src.link(&proxysink).unwrap();
        proxysrc.link(&sink).unwrap();

        let appsink = sink.dynamic_cast::<gst_app::AppSink>().unwrap();
        let sender_clone = sender.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let _sample = appsink
                        .emit_by_name("pull-sample", &[])
                        .unwrap()
                        .unwrap()
                        .get::<gst::Sample>()
                        .unwrap();

                    sender_clone.send(()).unwrap();
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }

    let pipeline_clone = pipeline.clone();
    let l_clone = l.clone();
    let mut test_scenario = Some(move || {
        let buffer = [0; 160];
        let socket = net::UdpSocket::bind("0.0.0.0:0").unwrap();

        let ipaddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let destinations = (FIRST_PORT..(FIRST_PORT + SRC_NB))
            .map(|port| SocketAddr::new(ipaddr, port))
            .collect::<Vec<_>>();

        for _ in 0..BUFFER_NB {
            for dest in &destinations {
                gst_debug!(CAT, "multiple_contexts_proxy: sending buffer to {:?}", dest);
                socket.send_to(&buffer, dest).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(CONTEXT_WAIT as u64));
            }
        }

        gst_debug!(
            CAT,
            "multiple_contexts_proxy: waiting for all buffers notifications"
        );
        for _ in 0..(BUFFER_NB * (SRC_NB as u32)) {
            receiver.recv().unwrap();
        }

        pipeline_clone.set_state(gst::State::Null).unwrap();
        l_clone.quit();
    });

    let bus = pipeline.bus().unwrap();
    let l_clone = l.clone();
    bus.add_watch(move |_, msg| {
        use gst::MessageView;

        match msg.view() {
            MessageView::StateChanged(state_changed) => {
                if let Some(source) = state_changed.src() {
                    if source.type_() == gst::Pipeline::static_type()
                        && state_changed.old() == gst::State::Paused
                        && state_changed.current() == gst::State::Playing
                    {
                        if let Some(test_scenario) = test_scenario.take() {
                            std::thread::spawn(test_scenario);
                        }
                    }
                }
            }
            MessageView::Error(err) => {
                gst_error!(
                    CAT,
                    "multiple_contexts_proxy: Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                l_clone.quit();
            }
            _ => (),
        };

        glib::Continue(true)
    })
    .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    gst_debug!(CAT, "Starting main loop for multiple_contexts_proxy...");
    l.run();
    gst_debug!(CAT, "Stopping main loop for multiple_contexts_proxy...");
}

#[test]
fn eos() {
    const CONTEXT: &str = "test_eos";

    init();

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);

    let caps = gst::Caps::new_simple("foo/bar", &[]);

    let src = gst::ElementFactory::make("ts-appsrc", Some("src-eos")).unwrap();
    src.set_property("caps", &caps).unwrap();
    src.set_property("do-timestamp", &true).unwrap();
    src.set_property("context", &CONTEXT).unwrap();

    let queue = gst::ElementFactory::make("ts-queue", Some("queue-eos")).unwrap();
    queue.set_property("context", &CONTEXT).unwrap();

    let appsink = gst::ElementFactory::make("appsink", Some("sink-eos")).unwrap();

    pipeline.add_many(&[&src, &queue, &appsink]).unwrap();
    gst::Element::link_many(&[&src, &queue, &appsink]).unwrap();

    appsink.set_property("sync", &false).unwrap();
    appsink.set_property("async", &false).unwrap();

    appsink.set_property("emit-signals", &true).unwrap();
    let (sample_notifier, sample_notif_rcv) = mpsc::channel();
    let (eos_notifier, eos_notif_rcv) = mpsc::channel();
    let appsink = appsink.dynamic_cast::<gst_app::AppSink>().unwrap();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                gst_debug!(CAT, obj: appsink, "eos: pulling sample");
                let _ = appsink
                    .emit_by_name("pull-sample", &[])
                    .unwrap()
                    .unwrap()
                    .get::<gst::Sample>()
                    .unwrap();

                sample_notifier.send(()).unwrap();

                Ok(gst::FlowSuccess::Ok)
            })
            .eos(move |_appsink| eos_notifier.send(()).unwrap())
            .build(),
    );

    fn push_buffer(src: &gst::Element) -> bool {
        gst_debug!(CAT, obj: src, "eos: pushing buffer");
        src.emit_by_name("push-buffer", &[&gst::Buffer::from_slice(vec![0; 1024])])
            .unwrap()
            .unwrap()
            .get::<bool>()
            .unwrap()
    }

    let pipeline_clone = pipeline.clone();
    let l_clone = l.clone();
    let mut scenario = Some(move || {
        // Initialize the dataflow
        assert!(push_buffer(&src));

        sample_notif_rcv.recv().unwrap();

        assert!(src
            .emit_by_name("end-of-stream", &[])
            .unwrap()
            .unwrap()
            .get::<bool>()
            .unwrap());

        eos_notif_rcv.recv().unwrap();

        // FIXME not ideal, but better than previous approach.
        // I think the "end-of-stream" signal should block
        // until the **src** element has actually reached EOS
        loop {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if !push_buffer(&src) {
                break;
            }
        }

        pipeline_clone.set_state(gst::State::Null).unwrap();
        l_clone.quit();
    });

    let l_clone = l.clone();
    pipeline
        .bus()
        .unwrap()
        .add_watch(move |_, msg| {
            use gst::MessageView;

            match msg.view() {
                MessageView::StateChanged(state_changed) => {
                    if let Some(source) = state_changed.src() {
                        if source.type_() != gst::Pipeline::static_type() {
                            return glib::Continue(true);
                        }
                        if state_changed.old() == gst::State::Paused
                            && state_changed.current() == gst::State::Playing
                        {
                            if let Some(scenario) = scenario.take() {
                                std::thread::spawn(scenario);
                            }
                        }
                    }
                }
                MessageView::Error(err) => {
                    gst_error!(
                        CAT,
                        "eos: Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                    l_clone.quit();
                }
                _ => (),
            };

            glib::Continue(true)
        })
        .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    gst_debug!(CAT, "Starting main loop for eos...");
    l.run();
    gst_debug!(CAT, "Stopping main loop for eos...");
}

#[test]
fn premature_shutdown() {
    init();

    const APPSRC_CONTEXT_WAIT: u32 = 0;
    const QUEUE_CONTEXT_WAIT: u32 = 1;
    const QUEUE_ITEMS_CAPACITY: u32 = 1;

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);

    let caps = gst::Caps::new_simple("foo/bar", &[]);

    let src = gst::ElementFactory::make("ts-appsrc", Some("src-ps")).unwrap();
    src.set_property("caps", &caps).unwrap();
    src.set_property("do-timestamp", &true).unwrap();
    src.set_property("context", &"appsrc-context").unwrap();
    src.set_property("context-wait", &APPSRC_CONTEXT_WAIT)
        .unwrap();

    let queue = gst::ElementFactory::make("ts-queue", Some("queue-ps")).unwrap();
    queue.set_property("context", &"queue-context").unwrap();
    queue
        .set_property("context-wait", &QUEUE_CONTEXT_WAIT)
        .unwrap();
    queue
        .set_property("max-size-buffers", &QUEUE_ITEMS_CAPACITY)
        .unwrap();

    let appsink = gst::ElementFactory::make("appsink", Some("sink-ps")).unwrap();

    pipeline.add_many(&[&src, &queue, &appsink]).unwrap();
    gst::Element::link_many(&[&src, &queue, &appsink]).unwrap();

    appsink.set_property("emit-signals", &true).unwrap();
    appsink.set_property("sync", &false).unwrap();
    appsink.set_property("async", &false).unwrap();

    let (appsink_sender, appsink_receiver) = mpsc::channel();

    let appsink = appsink.dynamic_cast::<gst_app::AppSink>().unwrap();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                gst_debug!(CAT, obj: appsink, "premature_shutdown: pulling sample");
                let _sample = appsink
                    .emit_by_name("pull-sample", &[])
                    .unwrap()
                    .unwrap()
                    .get::<gst::Sample>()
                    .unwrap();

                appsink_sender.send(()).unwrap();

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    fn push_buffer(src: &gst::Element, intent: &str) -> bool {
        gst_debug!(
            CAT,
            obj: src,
            "premature_shutdown: pushing buffer {}",
            intent
        );
        src.emit_by_name("push-buffer", &[&gst::Buffer::from_slice(vec![0; 1024])])
            .unwrap()
            .unwrap()
            .get::<bool>()
            .unwrap()
    }

    let pipeline_clone = pipeline.clone();
    let l_clone = l.clone();
    let mut scenario = Some(move || {
        gst_debug!(CAT, "premature_shutdown: STEP 1: Playing");
        // Initialize the dataflow
        assert!(push_buffer(&src, "(initial)"));

        // Wait for the buffer to reach AppSink
        appsink_receiver.recv().unwrap();
        assert_eq!(
            appsink_receiver.try_recv().unwrap_err(),
            mpsc::TryRecvError::Empty
        );

        assert!(push_buffer(&src, "before Playing -> Paused"));

        gst_debug!(CAT, "premature_shutdown: STEP 2: Playing -> Paused");
        pipeline_clone.set_state(gst::State::Paused).unwrap();

        gst_debug!(CAT, "premature_shutdown: STEP 3: Paused -> Playing");
        pipeline_clone.set_state(gst::State::Playing).unwrap();

        gst_debug!(CAT, "premature_shutdown: Playing again");

        gst_debug!(CAT, "Waiting for buffer sent before Playing -> Paused");
        appsink_receiver.recv().unwrap();

        assert!(push_buffer(&src, "after Paused -> Playing"));
        gst_debug!(CAT, "Waiting for buffer sent after Paused -> Playing");
        appsink_receiver.recv().unwrap();

        // Fill up the (dataqueue) and abruptly shutdown
        assert!(push_buffer(&src, "filling 1"));
        assert!(push_buffer(&src, "filling 2"));

        gst_debug!(CAT, "premature_shutdown: STEP 4: Playing -> Null");

        pipeline_clone.set_state(gst::State::Null).unwrap();

        assert!(!push_buffer(&src, "after Null"));

        l_clone.quit();
    });

    let l_clone = l.clone();
    pipeline
        .bus()
        .unwrap()
        .add_watch(move |_, msg| {
            use gst::MessageView;

            match msg.view() {
                MessageView::StateChanged(state_changed) => {
                    if let Some(source) = state_changed.src() {
                        if source.type_() != gst::Pipeline::static_type() {
                            return glib::Continue(true);
                        }
                        if state_changed.old() == gst::State::Paused
                            && state_changed.current() == gst::State::Playing
                        {
                            if let Some(scenario) = scenario.take() {
                                std::thread::spawn(scenario);
                            }
                        }
                    }
                }
                MessageView::Error(err) => {
                    gst_error!(
                        CAT,
                        "premature_shutdown: Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                    l_clone.quit();
                }
                _ => (),
            };

            glib::Continue(true)
        })
        .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    gst_debug!(CAT, "Starting main loop for premature_shutdown...");
    l.run();
    gst_debug!(CAT, "Stopped main loop for premature_shutdown...");
}
