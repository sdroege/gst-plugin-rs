// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::prelude::*;
use gstreamer as gst;

use std::sync::mpsc;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstreqwest::plugin_register_static().expect("reqwesthttpsrc tests");
    });
}

/// Our custom test harness around the HTTP source
#[derive(Debug)]
struct Harness {
    src: gst::Element,
    pad: gst::Pad,
    receiver: Option<mpsc::Receiver<Message>>,
    rt: Option<tokio::runtime::Runtime>,
}

/// Messages sent from our test harness
#[derive(Debug, Clone)]
enum Message {
    Buffer(gst::Buffer),
    Event(gst::Event),
    Message(gst::Message),
    ServerError(String),
}

impl Harness {
    /// Creates a new HTTP source and test harness around it
    ///
    /// `http_func`: Function to generate HTTP responses based on a request
    /// `setup_func`: Setup function for the HTTP source, should only set properties and similar
    fn new<
        F: FnMut(hyper::Request<hyper::Body>) -> hyper::Response<hyper::Body> + Send + 'static,
        G: FnOnce(&gst::Element),
    >(
        http_func: F,
        setup_func: G,
    ) -> Harness {
        use hyper::service::{make_service_fn, service_fn_ok};
        use hyper::Server;
        use std::sync::{Arc, Mutex};
        use tokio::prelude::*;

        // Create the HTTP source
        let src = gst::ElementFactory::make("reqwesthttpsrc", None).unwrap();

        // Sender/receiver for the messages we generate from various places for the tests
        //
        // Sending to this sender will block until the corresponding item was received from the
        // receiver, which allows us to handle everything as if it is running in a single thread
        let (sender, receiver) = mpsc::sync_channel(0);

        // Sink pad that receives everything the source is generating
        let pad = gst::Pad::new(Some("sink"), gst::PadDirection::Sink);
        let srcpad = src.get_static_pad("src").unwrap();
        srcpad.link(&pad).unwrap();

        // Collect all buffers, events and messages sent from the source
        let sender_clone = sender.clone();
        pad.set_chain_function(move |_pad, _parent, buffer| {
            let _ = sender_clone.send(Message::Buffer(buffer));
            Ok(gst::FlowSuccess::Ok)
        });
        let sender_clone = sender.clone();
        pad.set_event_function(move |_pad, _parent, event| {
            let _ = sender_clone.send(Message::Event(event));
            true
        });
        let bus = gst::Bus::new();
        bus.set_flushing(false);
        src.set_bus(Some(&bus));
        let sender_clone = sender.clone();
        bus.set_sync_handler(move |_bus, msg| {
            let _ = sender_clone.send(Message::Message(msg.clone()));
            gst::BusSyncReply::Drop
        });

        // Activate the pad so that it can be used now
        pad.set_active(true).unwrap();

        // Create the tokio runtime used for the HTTP server in this test
        let mut rt = tokio::runtime::Builder::new()
            .core_threads(1)
            .build()
            .unwrap();

        // Create an HTTP sever that listens on localhost on some random, free port
        let addr = ([127, 0, 0, 1], 0).into();

        // Whenever a new client is connecting, a new service function is requested. For each
        // client we use the same service function, which simply calls the function used by the
        // test
        let http_func = Arc::new(Mutex::new(http_func));
        let make_service = make_service_fn(move |_ctx| {
            let http_func = http_func.clone();
            service_fn_ok(move |req| (&mut *http_func.lock().unwrap())(req))
        });

        // Bind the server, retrieve the local port that was selected in the end and set this as
        // the location property on the source
        let server = Server::bind(&addr).serve(make_service);
        let local_addr = server.local_addr();
        src.set_property("location", &format!("http://{}/", local_addr))
            .unwrap();

        // Spawn the server in the background so that it can handle requests
        rt.spawn(server.map_err(move |e| {
            let _ = sender.send(Message::ServerError(format!("{:?}", e)));
        }));

        // Let the test setup anything needed on the HTTP source now
        setup_func(&src);

        Harness {
            src,
            pad,
            receiver: Some(receiver),
            rt: Some(rt),
        }
    }

    fn wait_for_error(&mut self) -> glib::Error {
        loop {
            match self.receiver.as_mut().unwrap().recv().unwrap() {
                Message::ServerError(err) => {
                    panic!("Got server error: {}", err);
                }
                Message::Event(ev) => {
                    use gst::EventView;

                    match ev.view() {
                        EventView::Eos(_) => {
                            panic!("Got EOS but expected error");
                        }
                        _ => (),
                    }
                }
                Message::Message(msg) => {
                    use gst::MessageView;

                    match msg.view() {
                        MessageView::Error(err) => {
                            return err.get_error();
                        }
                        _ => (),
                    }
                }
                Message::Buffer(_buffer) => {
                    panic!("Got buffer but expected error");
                }
            }
        }
    }

    fn wait_for_state_change(&mut self) -> gst::State {
        loop {
            match self.receiver.as_mut().unwrap().recv().unwrap() {
                Message::ServerError(err) => {
                    panic!("Got server error: {}", err);
                }
                Message::Event(ev) => {
                    use gst::EventView;

                    match ev.view() {
                        EventView::Eos(_) => {
                            panic!("Got EOS but expected state change");
                        }
                        _ => (),
                    }
                }
                Message::Message(msg) => {
                    use gst::MessageView;

                    match msg.view() {
                        MessageView::StateChanged(state) => {
                            return state.get_current();
                        }
                        MessageView::Error(err) => {
                            use std::error::Error;
                            panic!(
                                "Got error: {} ({})",
                                err.get_error().description(),
                                err.get_debug().unwrap_or_else(|| String::from("None"))
                            );
                        }
                        _ => (),
                    }
                }
                Message::Buffer(_buffer) => {
                    panic!("Got buffer but expected state change");
                }
            }
        }
    }

    fn wait_for_segment(
        &mut self,
        allow_buffer: bool,
    ) -> gst::FormattedSegment<gst::format::Bytes> {
        loop {
            match self.receiver.as_mut().unwrap().recv().unwrap() {
                Message::ServerError(err) => {
                    panic!("Got server error: {}", err);
                }
                Message::Event(ev) => {
                    use gst::EventView;

                    match ev.view() {
                        EventView::Segment(seg) => {
                            return seg
                                .get_segment()
                                .clone()
                                .downcast::<gst::format::Bytes>()
                                .unwrap();
                        }
                        _ => (),
                    }
                }
                Message::Message(msg) => {
                    use gst::MessageView;

                    match msg.view() {
                        MessageView::Error(err) => {
                            use std::error::Error;
                            panic!(
                                "Got error: {} ({})",
                                err.get_error().description(),
                                err.get_debug().unwrap_or_else(|| String::from("None"))
                            );
                        }
                        _ => (),
                    }
                }
                Message::Buffer(_buffer) => {
                    if !allow_buffer {
                        panic!("Got buffer but expected segment");
                    }
                }
            }
        }
    }

    /// Wait until a buffer is available or EOS was reached
    ///
    /// This function will panic on errors.
    fn wait_buffer_or_eos(&mut self) -> Option<gst::Buffer> {
        loop {
            match self.receiver.as_mut().unwrap().recv().unwrap() {
                Message::ServerError(err) => {
                    panic!("Got server error: {}", err);
                }
                Message::Event(ev) => {
                    use gst::EventView;

                    match ev.view() {
                        EventView::Eos(_) => return None,
                        _ => (),
                    }
                }
                Message::Message(msg) => {
                    use gst::MessageView;

                    match msg.view() {
                        MessageView::Error(err) => {
                            use std::error::Error;

                            panic!(
                                "Got error: {} ({})",
                                err.get_error().description(),
                                err.get_debug().unwrap_or_else(|| String::from("None"))
                            );
                        }
                        _ => (),
                    }
                }
                Message::Buffer(buffer) => return Some(buffer),
            }
        }
    }

    /// Run some code asynchronously on another thread with the HTTP source
    fn run<F: FnOnce(&gst::Element) + Send + 'static>(&self, func: F) {
        self.src.call_async(move |src| func(src));
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        use tokio::prelude::*;

        // Shut down everything that was set up for this test harness
        // and wait until the tokio runtime exited
        let bus = self.src.get_bus().unwrap();
        bus.set_flushing(true);

        // Drop the receiver first before setting the state so that
        // any threads that might still be blocked on the sender
        // are immediately unblocked
        self.receiver.take().unwrap();

        self.pad.set_active(false).unwrap();
        self.src.set_state(gst::State::Null).unwrap();

        self.rt.take().unwrap().shutdown_now().wait().unwrap();
    }
}

#[test]
fn test_basic_request() {
    use std::io::{Cursor, Read};
    init();

    // Set up a simple harness that returns "Hello World" for any HTTP request
    let mut h = Harness::new(
        |_req| {
            use hyper::{Body, Response};

            Response::new(Body::from("Hello World"))
        },
        |_src| {
            // No additional setup needed here
        },
    );

    // Set the HTTP source to Playing so that everything can start
    h.run(|src| {
        src.set_state(gst::State::Playing).unwrap();
    });

    // And now check if the data we receive is exactly what we expect it to be
    let expected_output = "Hello World";
    let mut cursor = Cursor::new(expected_output);

    while let Some(buffer) = h.wait_buffer_or_eos() {
        // On the first buffer also check if the duration reported by the HTTP source is what we
        // would expect it to be
        if cursor.position() == 0 {
            assert_eq!(
                h.src.query_duration::<gst::format::Bytes>(),
                Some(gst::format::Bytes::from(expected_output.len() as u64))
            );
        }

        // Map the buffer readable and check if it contains exactly the data we would expect at
        // this point after reading everything else we read in previous runs
        let map = buffer.map_readable().unwrap();
        let mut read_buf = vec![0; map.get_size()];
        assert_eq!(cursor.read(&mut read_buf).unwrap(), map.get_size());
        assert_eq!(&*map, &*read_buf);
    }

    // Check if everything was read
    assert_eq!(cursor.position(), 11);
}

#[test]
fn test_404_error() {
    use reqwest::StatusCode;
    init();

    let mut h = Harness::new(
        |_req| {
            use hyper::{Body, Response};

            Response::builder()
                .status(StatusCode::NOT_FOUND.as_u16())
                .body(Body::empty())
                .unwrap()
        },
        |_src| {},
    );

    h.run(|src| {
        let _ = src.set_state(gst::State::Playing);
    });

    let err_code = h.wait_for_error();
    if let Some(err) = err_code.kind::<gst::ResourceError>() {
        assert_eq!(err, gst::ResourceError::NotFound);
    }
}

#[test]
fn test_seek_after_ready() {
    use std::io::{Cursor, Read};
    init();

    let mut h = Harness::new(
        |req| {
            use hyper::{Body, Response};

            if req.headers().contains_key("Range") {
                let range = req.headers().get("Range").unwrap();
                if range == "bytes=123-" {
                    let mut data_seek = vec![0; 8192 - 123];
                    for (i, d) in data_seek.iter_mut().enumerate() {
                        *d = (i + 123 % 256) as u8;
                    }

                    Response::builder()
                        .header("content-length", 8192 - 123)
                        .header("accept-ranges", "bytes")
                        .header("content-range", "bytes 123-8192/8192")
                        .body(Body::from(data_seek))
                        .unwrap()
                } else {
                    panic!("Received an unexpected Range header")
                }
            } else {
                // `panic!("Received no Range header")` should be called here but due to a bug
                // in `basesrc` we cant do that here. If we do a seek in READY state, basesrc
                // will do a `start()` call without seek. Once we get seek forwarded, the call
                // with seek is made. This issue has to be solved.
                // issue link: https://gitlab.freedesktop.org/gstreamer/gstreamer/issues/413
                let mut data_full = vec![0; 8192];
                for (i, d) in data_full.iter_mut().enumerate() {
                    *d = (i % 256) as u8;
                }

                Response::builder()
                    .header("content-length", 8192)
                    .header("accept-ranges", "bytes")
                    .body(Body::from(data_full))
                    .unwrap()
            }
        },
        |_src| {},
    );

    h.run(|src| {
        src.set_state(gst::State::Ready).unwrap();
    });

    let current_state = h.wait_for_state_change();
    assert_eq!(current_state, gst::State::Ready);

    h.run(|src| {
        src.seek_simple(gst::SeekFlags::FLUSH, gst::format::Bytes::from(123))
            .unwrap();
        src.set_state(gst::State::Playing).unwrap();
    });

    let segment = h.wait_for_segment(false);
    assert_eq!(
        gst::format::Bytes::from(segment.get_start()),
        gst::format::Bytes::from(123)
    );

    let mut expected_output = vec![0; 8192 - 123];
    for (i, d) in expected_output.iter_mut().enumerate() {
        *d = ((123 + i) % 256) as u8;
    }
    let mut cursor = Cursor::new(expected_output);

    while let Some(buffer) = h.wait_buffer_or_eos() {
        assert_eq!(buffer.get_offset(), 123 + cursor.position());

        let map = buffer.map_readable().unwrap();
        let mut read_buf = vec![0; map.get_size()];

        assert_eq!(cursor.read(&mut read_buf).unwrap(), map.get_size());
        assert_eq!(&*map, &*read_buf);
    }
}

#[test]
fn test_seek_after_buffer_received() {
    use std::io::{Cursor, Read};
    init();

    let mut h = Harness::new(
        |req| {
            use hyper::{Body, Response};

            if req.headers().contains_key("Range") {
                let range = req.headers().get("Range").unwrap();
                if range == "bytes=123-" {
                    let mut data_seek = vec![0; 8192 - 123];
                    for (i, d) in data_seek.iter_mut().enumerate() {
                        *d = (i + 123 % 256) as u8;
                    }

                    Response::builder()
                        .header("content-length", 8192 - 123)
                        .header("accept-ranges", "bytes")
                        .header("content-range", "bytes 123-8192/8192")
                        .body(Body::from(data_seek))
                        .unwrap()
                } else {
                    panic!("Received an unexpected Range header")
                }
            } else {
                let mut data_full = vec![0; 8192];
                for (i, d) in data_full.iter_mut().enumerate() {
                    *d = (i % 256) as u8;
                }

                Response::builder()
                    .header("content-length", 8192)
                    .header("accept-ranges", "bytes")
                    .body(Body::from(data_full))
                    .unwrap()
            }
        },
        |_src| {},
    );

    h.run(|src| {
        src.set_state(gst::State::Playing).unwrap();
    });

    //wait for a buffer
    let buffer = h.wait_buffer_or_eos().unwrap();
    assert_eq!(buffer.get_offset(), 0);

    //seek to a position after a buffer is Received
    h.run(|src| {
        src.seek_simple(gst::SeekFlags::FLUSH, gst::format::Bytes::from(123))
            .unwrap();
    });

    let segment = h.wait_for_segment(true);
    assert_eq!(
        gst::format::Bytes::from(segment.get_start()),
        gst::format::Bytes::from(123)
    );

    let mut expected_output = vec![0; 8192 - 123];
    for (i, d) in expected_output.iter_mut().enumerate() {
        *d = ((123 + i) % 256) as u8;
    }
    let mut cursor = Cursor::new(expected_output);

    while let Some(buffer) = h.wait_buffer_or_eos() {
        assert_eq!(buffer.get_offset(), 123 + cursor.position());

        let map = buffer.map_readable().unwrap();
        let mut read_buf = vec![0; map.get_size()];

        assert_eq!(cursor.read(&mut read_buf).unwrap(), map.get_size());
        assert_eq!(&*map, &*read_buf);
    }
}

#[test]
fn test_cookies() {
    init();

    // Set up a simple harness that returns "Hello World" for any HTTP request
    let mut h = Harness::new(
        |_req| {
            use hyper::{Body, Response};

            Response::builder()
                .header("Set-Cookie", "foo=bar")
                .body(Body::from("Hello World"))
                .unwrap()
        },
        |_src| {
            // No additional setup needed here
        },
    );

    // Set the HTTP source to Playing so that everything can start
    h.run(|src| {
        src.set_state(gst::State::Playing).unwrap();
    });

    let mut num_bytes = 0;
    while let Some(buffer) = h.wait_buffer_or_eos() {
        num_bytes += buffer.get_size();
    }
    assert_eq!(num_bytes, 11);

    // Set up a simple harness that returns "Hello World" for any HTTP request
    let mut h2 = Harness::new(
        |req| {
            use hyper::{Body, Response};

            let cookies = req
                .headers()
                .get("Cookie")
                .expect("No cookies set")
                .to_str()
                .unwrap();
            assert!(cookies.split(';').any(|c| c == "foo=bar"));
            Response::builder()
                .body(Body::from("Hello again!"))
                .unwrap()
        },
        |_src| {
            // No additional setup needed here
        },
    );

    let context = h.src.get_context("gst.request.client").expect("No context");
    h2.src.set_context(&context);

    // Set the HTTP source to Playing so that everything can start
    h2.run(|src| {
        src.set_state(gst::State::Playing).unwrap();
    });

    let mut num_bytes = 0;
    while let Some(buffer) = h2.wait_buffer_or_eos() {
        num_bytes += buffer.get_size();
    }
    assert_eq!(num_bytes, 12);
}
