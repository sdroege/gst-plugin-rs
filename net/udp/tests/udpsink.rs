// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use std::net;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsudp::plugin_register_static().unwrap();
    });
}

#[track_caller]
fn recv(socket: &net::UdpSocket, expected: &[u8]) {
    let mut data = vec![0u8; expected.len()];
    let (n, _) = socket
        .recv_from(&mut data)
        .expect("timed out waiting for datagram");
    assert_eq!(n, expected.len());
    assert_eq!(&data[..n], expected);
}

#[test]
fn test_udpsink_dataflow() {
    init();

    let elem = gst::ElementFactory::make("udpsink2").build().unwrap();

    // constructed() installs the default client.
    assert_eq!(elem.property::<String>("host"), "127.0.0.1");
    assert_eq!(elem.property::<u32>("port"), 5004);
    assert_eq!(elem.property::<String>("uri"), "udp://127.0.0.1:5004");

    let socket = net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let port = socket.local_addr().unwrap().port();

    // Setting the port keeps the current host.
    elem.set_property("port", port as u32);
    assert_eq!(elem.property::<String>("host"), "127.0.0.1");
    assert_eq!(elem.property::<u32>("port"), port as u32);

    // Setting the host keeps the current port.
    elem.set_property("host", "127.0.0.1");
    assert_eq!(elem.property::<u32>("port"), port as u32);

    let mut h = gst_check::Harness::with_element(&elem, Some("sink"), None);
    h.set_src_caps_str("foo/bar");
    h.use_systemclock();
    h.play();

    h.push(gst::Buffer::from_slice(b"one".to_vec())).unwrap();
    h.push(gst::Buffer::from_slice(b"two".to_vec())).unwrap();

    recv(&socket, b"one");
    recv(&socket, b"two");
}

#[test]
fn test_udpsink_uri() {
    init();

    let elem = gst::ElementFactory::make("udpsink2").build().unwrap();

    // Setting a valid udp URI updates host and port.
    elem.set_property("uri", "udp://127.0.0.1:5001");
    assert_eq!(elem.property::<String>("host"), "127.0.0.1");
    assert_eq!(elem.property::<u32>("port"), 5001);
    assert_eq!(elem.property::<String>("uri"), "udp://127.0.0.1:5001");

    // Setting the host keeps the port and the URI is unchanged.
    elem.set_property("host", "127.0.0.1");
    assert_eq!(elem.property::<String>("uri"), "udp://127.0.0.1:5001");

    // An IPv6 literal is read back as-is via host but bracketed via uri.
    elem.set_property("uri", "udp://[::1]:5002");
    assert_eq!(elem.property::<String>("host"), "::1");
    assert_eq!(elem.property::<u32>("port"), 5002);
    assert_eq!(elem.property::<String>("uri"), "udp://[::1]:5002");

    // A non-udp scheme is rejected and the client stays as it was.
    elem.set_property("uri", "http://127.0.0.1:9999");
    assert_eq!(elem.property::<String>("host"), "::1");
    assert_eq!(elem.property::<u32>("port"), 5002);
    assert_eq!(elem.property::<String>("uri"), "udp://[::1]:5002");
}
