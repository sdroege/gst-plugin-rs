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
//
// SPDX-License-Identifier: LGPL-2.1-or-later

use gst::prelude::*;

use std::thread;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare udpsrc test");
    });
}

#[test]
#[cfg(not(windows))]
fn test_push() {
    init();

    let mut h = gst_check::Harness::new("ts-udpsrc");

    let caps = gst::Caps::builder("foo/bar").build();
    {
        let udpsrc = h.element().unwrap();
        udpsrc.set_property("caps", &caps);
        udpsrc.set_property("port", 5000i32);
        udpsrc.set_property("context", "test-push");
    }

    h.play();

    thread::spawn(move || {
        use std::net;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use std::time;

        // Sleep 50ms to allow for the udpsrc to be ready to actually receive data
        thread::sleep(time::Duration::from_millis(50));

        let buffer = [0; 160];
        let socket = net::UdpSocket::bind("0.0.0.0:0").unwrap();

        let ipaddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let dest = SocketAddr::new(ipaddr, 5000u16);

        for _ in 0..3 {
            socket.send_to(&buffer, dest).unwrap();
        }
    });

    for _ in 0..3 {
        let buffer = h.pull().unwrap();
        assert_eq!(buffer.size(), 160);
    }

    let mut n_events = 0;
    loop {
        use gst::EventView;

        let event = h.pull_event().unwrap();
        match event.view() {
            EventView::StreamStart(..) => {
                assert_eq!(n_events, 0);
            }
            EventView::Caps(ev) => {
                assert_eq!(n_events, 1);
                let event_caps = ev.caps();
                assert_eq!(caps.as_ref(), event_caps);
            }
            EventView::Segment(..) => {
                assert_eq!(n_events, 2);
                break;
            }
            _ => (),
        }
        n_events += 1;
    }
    assert!(n_events >= 2);
}

#[test]
#[cfg(not(windows))]
fn test_socket_reuse() {
    init();

    let mut ts_src_h = gst_check::Harness::new("ts-udpsrc");
    let mut sink_h = gst_check::Harness::new("udpsink");
    let mut ts_src_h2 = gst_check::Harness::new("ts-udpsrc");

    {
        let udpsrc = ts_src_h.element().unwrap();
        udpsrc.set_property("port", 6000i32);
        udpsrc.set_property("context", "test-socket-reuse");
    }
    ts_src_h.play();

    {
        let udpsrc = ts_src_h.element().unwrap();
        let socket = udpsrc.property::<gio::Socket>("used-socket");

        let udpsink = sink_h.element().unwrap();
        udpsink.set_property("socket", &socket);
        udpsink.set_property("host", "127.0.0.1");
        udpsink.set_property("port", 6001i32);
    }
    sink_h.play();
    sink_h.set_src_caps_str("application/test");

    {
        let udpsrc = ts_src_h2.element().unwrap();
        udpsrc.set_property("port", 6001i32);
        udpsrc.set_property("context", "test-socket-reuse");
    }
    ts_src_h2.play();

    thread::spawn(move || {
        use std::net;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use std::time;

        // Sleep 50ms to allow for the udpsrc to be ready to actually receive data
        thread::sleep(time::Duration::from_millis(50));

        let buffer = [0; 160];
        let socket = net::UdpSocket::bind("0.0.0.0:0").unwrap();

        let ipaddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let dest = SocketAddr::new(ipaddr, 6000u16);

        for _ in 0..3 {
            socket.send_to(&buffer, dest).unwrap();
        }
    });

    for _ in 0..3 {
        let buffer = ts_src_h.pull().unwrap();
        sink_h.push(buffer).unwrap();
        let buffer = ts_src_h2.pull().unwrap();

        assert_eq!(buffer.size(), 160);
    }
}

#[test]
#[ignore = "In order to test this properly, the UNREACHABLE_DESTINATION must point to an unknown host in the subnet"]
fn icmp_destination_unreachable() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
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

    let udpsrc = gst::ElementFactory::make("ts-udpsrc")
        .property("context", "icmp-dest-unreachable")
        .property("socket", socket)
        .build()
        .unwrap();
    let bus = gst::glib::Object::new::<gst::Bus>();
    udpsrc.set_bus(Some(&bus));

    let mut ts_src_h = gst_check::Harness::with_element(&udpsrc, None, Some("src"));
    ts_src_h.play();

    let done = Arc::new(AtomicBool::new(false));
    thread::spawn({
        let done = done.clone();
        move || {
            let udpsink = gst::ElementFactory::make("ts-udpsink")
                .property("context", "icmp-dest-unreachable")
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
                thread::sleep(std::time::Duration::from_millis(500));
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
