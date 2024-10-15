// Copyright (C) 2024, Fluendo S.A.
//      Author: Andoni Morales Alastruey <amorales@fluendo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use serial_test::serial;
use std::{path::PathBuf, thread};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstquinn::plugin_register_static().expect("WebTransport source sink send receive tests");
    });
}

fn make_buffer(content: &[u8]) -> gst::Buffer {
    let mut buf = gst::Buffer::from_slice(content.to_owned());
    buf.make_mut().set_pts(gst::ClockTime::from_mseconds(200));
    buf
}

fn get_certificates_paths() -> (String, String) {
    let mut certs_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    certs_dir.push("tests");
    certs_dir.push("certs");

    (
        certs_dir
            .join("localhost.crt")
            .into_os_string()
            .into_string()
            .unwrap(),
        certs_dir
            .join("localhost.key")
            .into_os_string()
            .into_string()
            .unwrap(),
    )
}

fn send_receive(src_pipeline_props: &str, sink_pipeline_props: &str) {
    init();

    let content = "Hello, world!\n".as_bytes();

    let (cert_path, key_path) = get_certificates_paths();

    let src_pipeline = format!(
        "quinnwtclientsrc {} certificate-file={} caps=text/plain",
        src_pipeline_props, cert_path
    );
    let sink_pipeline = format!(
        "quinnwtserversink {} server-name=localhost \
            address=127.0.0.1 certificate-file={} private-key-file={}",
        sink_pipeline_props, cert_path, key_path
    );

    thread::spawn(move || {
        let mut h1 = gst_check::Harness::new_empty();
        h1.add_parse(&sink_pipeline);

        h1.set_src_caps(gst::Caps::builder("text/plain").build());

        h1.play();

        assert!(h1.push(make_buffer(content)) == Ok(gst::FlowSuccess::Ok));

        h1.push_event(gst::event::Eos::new());

        h1.element().unwrap().set_state(gst::State::Null).unwrap();

        drop(h1);
    });

    let mut h2 = gst_check::Harness::new_empty();
    h2.add_parse(&src_pipeline);

    h2.play();

    let buf = h2.pull_until_eos().unwrap().unwrap();

    assert_eq!(
        content,
        buf.into_mapped_buffer_readable().unwrap().as_slice()
    );

    h2.element().unwrap().set_state(gst::State::Null).unwrap();

    drop(h2);
}

#[test]
#[serial]
fn test_send_receive_without_datagram() {
    send_receive("url=https://127.0.0.1:7770", "port=7770");
}

#[test]
#[serial]
fn test_send_receive_with_datagram() {
    send_receive(
        "use-datagram=true url=https://127.0.0.1:7771",
        "use-datagram=true port=7771",
    );
}

#[test]
#[serial]
fn test_send_receive_with_hostname() {
    send_receive("url=https://localhost:7772", "port=7772");
}
