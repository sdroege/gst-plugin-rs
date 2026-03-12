// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gio::prelude::*;
use gst::prelude::*;
use std::{
    net,
    sync::{Arc, atomic},
};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsudp::plugin_register_static().unwrap();
    });
}

fn setup_udpsrc(configure: impl FnOnce(&gst::Element)) -> (gst_check::Harness, net::UdpSocket) {
    let mut h = gst_check::Harness::new("udpsrc2");

    let elem = h.element().unwrap();

    // Use a random port
    elem.set_property("port", 0u32);
    configure(&elem);

    // Check that the port is notified
    let port_notify_count = Arc::new(atomic::AtomicU32::new(0));
    let count_clone = port_notify_count.clone();
    elem.connect_notify(Some("port"), move |_, _| {
        count_clone.fetch_add(1, atomic::Ordering::SeqCst);
    });

    h.use_systemclock();
    h.play();

    // And grab the selected port
    let port = elem.property::<u32>("port");
    assert_ne!(port, 0, "port should have been allocated");
    assert_eq!(
        port_notify_count.load(atomic::Ordering::SeqCst),
        1,
        "port property should have been notified once after binding"
    );

    // Create a new socket for sending packets
    let socket = net::UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.connect(format!("127.0.0.1:{port}")).unwrap();
    (h, socket)
}

#[test]
fn test_udpsrc_empty_packet() {
    init();

    let (mut h, socket) = setup_udpsrc(|_| {});

    // Send an empty packet followed by a non-empty one.
    socket.send(&[]).unwrap();
    socket.send(b"HeLL0\0").unwrap();

    // Make sure the empty packet is discarded and we only receive the packet with payload.
    // A read of 0 can't be distinguished from EOF conditions with recv() and friends.
    let buf = h.pull().unwrap();
    assert_ne!(buf.size(), 0);
    let map = buf.map_readable().unwrap();
    assert_eq!(map.as_slice(), b"HeLL0\0");
}

#[test]
fn test_udpsrc_large_packets() {
    init();

    let (mut h, socket) = setup_udpsrc(|elem| {
        elem.set_property("mtu", 65536u32);
    });

    // Try sending big and small packets and make sure they all arrive.
    let sizes = [48_000, 21_000, 500, 1_600, 1_400];
    let mut sent_sizes = Vec::new();
    for size in sizes {
        let data = (0..size).map(|i| (i & 0xff) as u8).collect::<Vec<u8>>();
        // Sending too big packets can fail depending on the platform so we simply ignore errors
        // here and only expect to receive the packets that are actually sent.
        if socket.send(&data).is_ok() {
            sent_sizes.push(size);
        }
    }

    for expected_size in sent_sizes {
        let buf = h.pull().unwrap();
        assert_eq!(buf.size(), expected_size);

        let map = buf.map_readable().unwrap();
        let expected = (0..expected_size)
            .map(|i| (i & 0xff) as u8)
            .collect::<Vec<u8>>();
        assert_eq!(map.as_slice(), expected.as_slice());
    }
}

#[test]
fn test_udpsrc_large_packets_discard() {
    init();

    let (mut h, socket) = setup_udpsrc(|elem| {
        elem.set_property("mtu", 1_500u32);
    });

    // Try sending big and small packets and make sure the too big ones are discarded.
    let sizes = [48_000, 21_000, 500, 1_600, 1_400];
    let mut sent_sizes = Vec::new();
    for size in sizes {
        let data = (0..size).map(|i| (i & 0xff) as u8).collect::<Vec<u8>>();
        // Sending too big packets can fail depending on the platform so we simply ignore errors
        // here and only expect to receive the packets that are actually sent.
        if socket.send(&data).is_ok() {
            sent_sizes.push(size);
        }
    }

    for expected_size in sent_sizes {
        if expected_size > 1_500 {
            continue;
        }

        let buf = h.pull().unwrap();
        assert_eq!(buf.size(), expected_size);

        let map = buf.map_readable().unwrap();

        let expected = (0..expected_size)
            .map(|i| (i & 0xff) as u8)
            .collect::<Vec<u8>>();
        assert_eq!(map.as_slice(), expected.as_slice());
    }
}

#[test]
fn test_udpsrc_caps() {
    init();

    let caps = gst::Caps::builder("application/x-rtp").build();

    let (mut h, socket) = setup_udpsrc(|elem| {
        elem.set_property("caps", &caps);
    });

    socket.send(b"test").unwrap();

    let buf = h.pull().unwrap();
    assert_eq!(buf.map_readable().unwrap().as_slice(), b"test");

    // Check that caps are correctly set from the property.
    let src_pad = h.element().unwrap().static_pad("src").unwrap();
    assert_eq!(src_pad.current_caps().unwrap(), caps);
}

#[test]
fn test_udpsrc_uri() {
    init();

    let elem = gst::ElementFactory::make("udpsrc2").build().unwrap();

    let address_notify_count = Arc::new(atomic::AtomicU32::new(0));
    let count_clone = address_notify_count.clone();
    elem.connect_notify(Some("address"), move |_, _| {
        count_clone.fetch_add(1, atomic::Ordering::SeqCst);
    });

    let port_notify_count = Arc::new(atomic::AtomicU32::new(0));
    let count_clone = port_notify_count.clone();
    elem.connect_notify(Some("port"), move |_, _| {
        count_clone.fetch_add(1, atomic::Ordering::SeqCst);
    });

    let source_filter_notify_count = Arc::new(atomic::AtomicU32::new(0));
    let count_clone = source_filter_notify_count.clone();
    elem.connect_notify(Some("source-filter"), move |_, _| {
        count_clone.fetch_add(1, atomic::Ordering::SeqCst);
    });

    let source_filter_exclusive_notify_count = Arc::new(atomic::AtomicU32::new(0));
    let count_clone = source_filter_exclusive_notify_count.clone();
    elem.connect_notify(Some("source-filter-exclusive"), move |_, _| {
        count_clone.fetch_add(1, atomic::Ordering::SeqCst);
    });

    // Set URI and verify address/port and their notifications
    elem.set_property("uri", "udp://127.0.0.1:5004");
    assert_eq!(elem.property::<String>("address"), "127.0.0.1");
    assert_eq!(elem.property::<u32>("port"), 5004);
    assert_eq!(address_notify_count.load(atomic::Ordering::SeqCst), 1);
    assert_eq!(port_notify_count.load(atomic::Ordering::SeqCst), 1);
    assert_eq!(source_filter_notify_count.load(atomic::Ordering::SeqCst), 1);
    assert_eq!(
        source_filter_exclusive_notify_count.load(atomic::Ordering::SeqCst),
        1
    );

    // Setting a different URI updates and notifies again
    elem.set_property("uri", "udp://192.168.1.1:6000");
    assert_eq!(elem.property::<String>("address"), "192.168.1.1");
    assert_eq!(elem.property::<u32>("port"), 6000);
    assert_eq!(address_notify_count.load(atomic::Ordering::SeqCst), 2);
    assert_eq!(port_notify_count.load(atomic::Ordering::SeqCst), 2);
    assert_eq!(source_filter_notify_count.load(atomic::Ordering::SeqCst), 2);
    assert_eq!(
        source_filter_exclusive_notify_count.load(atomic::Ordering::SeqCst),
        2
    );

    // URI with source-filter query parameter
    elem.set_property(
        "uri",
        "udp://127.0.0.1:5004?source-filter=127.0.0.2,127.0.0.3",
    );
    assert_eq!(elem.property::<String>("address"), "127.0.0.1");
    assert_eq!(elem.property::<u32>("port"), 5004);
    assert_eq!(
        elem.property::<Option<String>>("source-filter").as_deref(),
        Some("127.0.0.2,127.0.0.3")
    );
    assert!(!elem.property::<bool>("source-filter-exclusive"));
    assert_eq!(address_notify_count.load(atomic::Ordering::SeqCst), 3);
    assert_eq!(port_notify_count.load(atomic::Ordering::SeqCst), 3);
    assert_eq!(source_filter_notify_count.load(atomic::Ordering::SeqCst), 3);
    assert_eq!(
        source_filter_exclusive_notify_count.load(atomic::Ordering::SeqCst),
        3
    );

    // URI with different source-filter query parameter
    elem.set_property("uri", "udp://127.0.0.1:5004?source-filter=127.0.0.2");
    assert_eq!(elem.property::<String>("address"), "127.0.0.1");
    assert_eq!(elem.property::<u32>("port"), 5004);
    assert_eq!(
        elem.property::<Option<String>>("source-filter").as_deref(),
        Some("127.0.0.2")
    );
    assert!(!elem.property::<bool>("source-filter-exclusive"));
    assert_eq!(address_notify_count.load(atomic::Ordering::SeqCst), 4);
    assert_eq!(port_notify_count.load(atomic::Ordering::SeqCst), 4);
    assert_eq!(source_filter_notify_count.load(atomic::Ordering::SeqCst), 4);
    assert_eq!(
        source_filter_exclusive_notify_count.load(atomic::Ordering::SeqCst),
        4
    );

    // URI with different source-filter query parameter and exclusive source filter
    elem.set_property(
        "uri",
        "udp://127.0.0.1:5004?source-filter=127.0.0.2&source-filter-exclusive=true",
    );
    assert_eq!(elem.property::<String>("address"), "127.0.0.1");
    assert_eq!(elem.property::<u32>("port"), 5004);
    assert_eq!(
        elem.property::<Option<String>>("source-filter").as_deref(),
        Some("127.0.0.2")
    );
    assert!(elem.property::<bool>("source-filter-exclusive"));
    assert_eq!(address_notify_count.load(atomic::Ordering::SeqCst), 5);
    assert_eq!(port_notify_count.load(atomic::Ordering::SeqCst), 5);
    assert_eq!(source_filter_notify_count.load(atomic::Ordering::SeqCst), 5);
    assert_eq!(
        source_filter_exclusive_notify_count.load(atomic::Ordering::SeqCst),
        5
    );

    // Setting URI without query resets source-filter
    elem.set_property("uri", "udp://127.0.0.1:5004");
    assert_eq!(elem.property::<String>("address"), "127.0.0.1");
    assert_eq!(elem.property::<u32>("port"), 5004);
    assert!(
        elem.property::<Option<String>>("source-filter").is_none(),
        "source-filter should be reset to None"
    );
    assert!(!elem.property::<bool>("source-filter-exclusive"));
    assert_eq!(address_notify_count.load(atomic::Ordering::SeqCst), 6);
    assert_eq!(port_notify_count.load(atomic::Ordering::SeqCst), 6);
    assert_eq!(source_filter_notify_count.load(atomic::Ordering::SeqCst), 6);
    assert_eq!(
        source_filter_exclusive_notify_count.load(atomic::Ordering::SeqCst),
        6
    );
}

#[test]
fn test_udpsrc_skip_first_bytes() {
    init();

    let (mut h, socket) = setup_udpsrc(|elem| {
        elem.set_property("skip-first-bytes", 4u32);
    });

    socket.send(b"SKIPpayload!").unwrap();

    let buf = h.pull().unwrap();
    let map = buf.map_readable().unwrap();
    assert_eq!(map.as_slice(), b"payload!");
}

#[test]
fn test_udpsrc_timeout() {
    init();

    let timeout = gst::ClockTime::from_mseconds(200);

    let pipeline = gst::Pipeline::new();
    let udpsrc = gst::ElementFactory::make("udpsrc2")
        .property("port", 0u32)
        .property("timeout", timeout)
        .build()
        .unwrap();
    let fakesink = gst::ElementFactory::make("fakesink").build().unwrap();

    pipeline.add_many([&udpsrc, &fakesink]).unwrap();
    udpsrc.link(&fakesink).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    // Wait for the timeout message on the bus
    let bus = pipeline.bus().unwrap();
    let msg = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Element],
        )
        .expect("should have received a timeout message");

    let structure = msg.structure().unwrap();
    assert_eq!(structure.name().as_str(), "GstUDPSrcTimeout");
    assert_eq!(structure.get::<gst::ClockTime>("timeout").unwrap(), timeout);

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_udpsrc_used_socket() {
    init();

    let elem = gst::ElementFactory::make("udpsrc2")
        .property("port", 0u32)
        .build()
        .unwrap();

    assert!(
        elem.property::<Option<gio::Socket>>("used-socket")
            .is_none()
    );

    elem.set_state(gst::State::Playing).unwrap();

    let used_socket = elem.property::<Option<gio::Socket>>("used-socket").unwrap();

    // Check that the socket is bound correctly
    let local_addr = used_socket
        .local_address()
        .unwrap()
        .downcast::<gio::InetSocketAddress>()
        .unwrap();

    assert_eq!(
        elem.property::<String>("address"),
        local_addr.address().to_string()
    );
    assert_eq!(elem.property::<u32>("port"), local_addr.port() as u32);

    assert_eq!(used_socket.family(), gio::SocketFamily::Ipv4);
    assert_eq!(used_socket.socket_type(), gio::SocketType::Datagram);
    assert_eq!(used_socket.protocol(), gio::SocketProtocol::Udp);

    elem.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_udpsrc_external_socket() {
    init();

    // Create a gio::Socket
    let gio_socket = gio::Socket::new(
        gio::SocketFamily::Ipv4,
        gio::SocketType::Datagram,
        gio::SocketProtocol::Udp,
    )
    .unwrap();

    let bind_addr =
        gio::InetSocketAddress::new(&gio::InetAddress::from_string("127.0.0.1").unwrap(), 0);
    gio_socket.bind(&bind_addr, false).unwrap();

    let local_addr = gio_socket
        .local_address()
        .unwrap()
        .downcast::<gio::InetSocketAddress>()
        .unwrap();

    let port = local_addr.port();
    assert_ne!(port, 0);

    let (mut h, socket) = setup_udpsrc(|elem| {
        elem.set_property("socket", &gio_socket);
        elem.set_property("close-socket", false);
    });

    // Check properties
    let elem = h.element().unwrap();
    assert_eq!(
        elem.property::<String>("address"),
        local_addr.address().to_string()
    );
    assert_eq!(elem.property::<u32>("port"), local_addr.port() as u32);

    // Check that used-socket is updated accordingly
    let used_socket = elem
        .property::<Option<gio::Socket>>("used-socket")
        .expect("used-socket should be set");
    assert_eq!(used_socket, gio_socket);

    // Check that receiving packets actually works
    socket.send(b"packet").unwrap();

    let buf = h.pull().unwrap();
    let map = buf.map_readable().unwrap();
    assert_eq!(map.as_slice(), b"packet");
}

#[test]
fn test_udpsrc_net_address_meta() {
    init();

    let (mut h, socket) = setup_udpsrc(|_| {});

    socket.send(b"packet").unwrap();

    let buf = h.pull().unwrap();
    assert_eq!(buf.map_readable().unwrap().as_slice(), b"packet");

    let meta = buf
        .meta::<gst_net::NetAddressMeta>()
        .expect("buffer should have NetAddressMeta");

    let local_addr = socket.local_addr().unwrap();
    let addr = meta.addr().downcast::<gio::InetSocketAddress>().unwrap();
    assert_eq!(std::net::IpAddr::from(addr.address()), local_addr.ip());
    assert_eq!(addr.port(), local_addr.port());
}

#[test]
#[ignore = "In order to test this properly, the UNREACHABLE_DESTINATION must point to an unknown host in the subnet"]
fn icmp_destination_unreachable() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use gio::prelude::*;

    const UNREACHABLE_DESTINATION: &str = "192.168.1.15:6003";
    const BIND_PORT: i32 = 6002;
    const MAX_SEND: usize = 15;

    init();

    let socket = gio::Socket::new(
        gio::SocketFamily::Ipv4,
        gio::SocketType::Datagram,
        gio::SocketProtocol::Udp,
    )
    .unwrap();
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    ))]
    socket
        .set_option(libc::IPPROTO_IP, libc::IP_RECVERR, 1)
        .unwrap();
    let addr = gio::InetAddress::new_any(gio::SocketFamily::Ipv4);
    socket
        .bind(&gio::InetSocketAddress::new(&addr, BIND_PORT as u16), true)
        .unwrap();

    let udpsrc = gst::ElementFactory::make("udpsrc2")
        .property("socket", socket)
        .build()
        .unwrap();
    let bus = gst::glib::Object::new::<gst::Bus>();
    udpsrc.set_bus(Some(&bus));

    let mut ts_src_h = gst_check::Harness::with_element(&udpsrc, None, Some("src"));
    ts_src_h.play();

    let done = Arc::new(AtomicBool::new(false));
    std::thread::spawn({
        let done = done.clone();
        move || {
            let udpsink = gst::ElementFactory::make("udpsink")
                .property("bind-port", BIND_PORT)
                .property("clients", UNREACHABLE_DESTINATION)
                .build()
                .unwrap();

            let mut ts_sink_h = gst_check::Harness::with_element(&udpsink, Some("sink"), None);
            ts_sink_h.set_src_caps_str("foo/bar");
            ts_sink_h.play();

            for i in 0..MAX_SEND {
                ts_sink_h.push(gst::Buffer::from_slice([42])).unwrap();
                println!("udp send: {i}");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }

            done.store(true, Ordering::SeqCst);
        }
    });

    while !done.load(Ordering::SeqCst) {
        while let Some(msg) = bus.pop() {
            if let gst::MessageView::Error(msg) = msg.view() {
                panic!("udp recv: {msg:?}");
            }
        }
    }
}
