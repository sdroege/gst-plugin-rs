// Copyright (C) 2025 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{mpsc, LazyLock};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "icecastsinktest",
        gst::DebugColorFlags::empty(),
        Some("Icecast sink test"),
    )
});

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gsticecast::plugin_register_static().expect("icecastsink test");
    });
}

#[test]
fn test_reconnect() {
    init();

    // Channel to signal when server is up and running listening for incoming connection
    let (ready_tx, ready_rx) = mpsc::channel();

    let handler = thread::spawn(move || {
        use std::net;

        let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();

        let listening_port = listener.local_addr().unwrap().port();

        gst::info!(CAT, "Listening on port {listening_port}");

        ready_tx.send(listening_port).unwrap();

        'accept_loop: for (n, stream) in listener.incoming().enumerate() {
            let mut socket = stream.unwrap();

            let mut reader = BufReader::new(&socket);

            let mut request = String::new();

            while !request.ends_with("\r\n\r\n") {
                reader.read_line(&mut request).expect("OPTIONS request");
            }

            drop(reader);

            gst::info!(CAT, "Request from client: {request}");

            assert!(request.starts_with("OPTIONS * HTTP/1.1"));

            socket
                .write_all(
                    "HTTP/1.1 204 No Content\r
Server: rocketstreamingserver\r
Access-Control-Allow-Origin: \r
Access-Control-Allow-Methods: PUT, GET, HEAD, OPTIONS\r
Access-Control-Allow-Headers: Content-Type, Icy-Metadata, Authorization, X-Requested-With\r
Access-Control-Max-Age: 86400\r
Cache-Control: no-cache, no-store\r
Access-Control-Allow-Origin: *\r
Content-Length: 0\r
Connection: Keep-Alive\r
\r
\r
"
                    .as_bytes(),
                )
                .expect("OPTIONS response");

            let mut reader = BufReader::new(&socket);

            request.clear();

            while !request.ends_with("\r\n\r\n") {
                reader.read_line(&mut request).expect("PUT request");
            }

            drop(reader);

            gst::info!(CAT, "Request from client: {request}");

            assert!(request.starts_with("PUT /radio HTTP/1.1"));

            socket
                .write_all("HTTP/1.1 100 Continue\r\n\r\n".as_bytes())
                .expect("PUT response");

            let streaming_time = Instant::now();

            // Collect data read into a vec because we can't assume the reads will
            // come through in the same chunking as things were sent out
            let mut bytes_read = vec![];

            while streaming_time.elapsed() < Duration::from_millis(1200) {
                let mut buf = [0u8; 8192];

                let n_read = socket.read(&mut buf).expect("Read data");
                gst::info!(CAT, "read {n_read} bytes of data");
                bytes_read.extend_from_slice(&buf[..n_read]);

                // Bail out after 2 reconnection attempts
                if n_read == 0 && n == 2 {
                    break 'accept_loop;
                }
            }

            const OGG_PAGE_HEADER_FLAG_BOS: u8 = 0x02;

            // Check whether the stream starts with an Ogg page that has the beginning-of-stream
            // flag set. This basically tests whether the stream headers are re-sent on re-connect.
            let is_bos = (bytes_read[5] & OGG_PAGE_HEADER_FLAG_BOS) != 0;

            assert!(is_bos, "Expected stream to start with Ogg stream headers!");

            // TcpStream gets dropped, which closes the connection and forces a reconnect
        }
    });

    let pipeline = gst::Pipeline::default();

    let src = gst::ElementFactory::make("audiotestsrc").build().unwrap();

    // Could feed data from a GDP file as well
    let Ok(enc) = gst::ElementFactory::make("opusenc").build() else {
        gst::warning!(CAT, "Skipping test, opusenc element not available");
        return;
    };

    let Ok(mux) = gst::ElementFactory::make("oggmux").build() else {
        gst::warning!(CAT, "Skipping test, oggmux element not available");
        return;
    };

    let valve = gst::ElementFactory::make("valve").build().unwrap();

    let sink = gst::ElementFactory::make("icecastsink").build().unwrap();

    pipeline
        .add_many([&src, &enc, &mux, &valve, &sink])
        .unwrap();

    gst::Element::link_many([&src, &enc, &mux, &valve, &sink]).unwrap();

    // Wait for the server to be up and running
    let server_port = ready_rx.recv().unwrap();

    sink.set_property(
        "location",
        format!("ice+http://127.0.0.1:{server_port}/radio"),
    );

    // This starts up icecastsink which will connect to our test server, which will drop the
    // connection after a few seconds, after which icecastsink will automatically reconnect.
    pipeline.set_state(gst::State::Playing).unwrap();

    let mut n_connects = 0;

    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.timed_pop(5.seconds()) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => panic!("Didn't expect EOS"),

            MessageView::Error(err) => panic!("{err:?}"),

            MessageView::Progress(progress) => {
                gst::info!(CAT, "progress: {progress:?}");
                let (progress_type, code, _) = progress.get();
                if code == "connect" && progress_type == gst::ProgressType::Complete {
                    n_connects += 1;
                    gst::info!(CAT, "Connection #{n_connects}");

                    if n_connects > 2 {
                        gst::info!(CAT, "Bailing out after {n_connects} rounds of connects");
                        break;
                    }
                }
            }

            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    handler.join().unwrap();
}
