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
fn test_push() {
    init();

    let mut h = gst_check::Harness::new("ts-udpsrc");

    let caps = gst::Caps::new_simple("foo/bar", &[]);
    {
        let udpsrc = h.element().unwrap();
        udpsrc.set_property("caps", &caps).unwrap();
        udpsrc.set_property("port", &5000i32).unwrap();
        udpsrc.set_property("context", &"test-push").unwrap();
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
        udpsrc.set_property("port", &6000i32).unwrap();
        udpsrc
            .set_property("context", &"test-socket-reuse")
            .unwrap();
    }
    ts_src_h.play();

    {
        let udpsrc = ts_src_h.element().unwrap();
        let socket = udpsrc
            .property("used-socket")
            .unwrap()
            .get::<gio::Socket>()
            .unwrap();

        let udpsink = sink_h.element().unwrap();
        udpsink.set_property("socket", &socket).unwrap();
        udpsink.set_property("host", &"127.0.0.1").unwrap();
        udpsink.set_property("port", &6001i32).unwrap();
    }
    sink_h.play();
    sink_h.set_src_caps_str("application/test");

    {
        let udpsrc = ts_src_h2.element().unwrap();
        udpsrc.set_property("port", &6001i32).unwrap();
        udpsrc
            .set_property("context", &"test-socket-reuse")
            .unwrap();
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
