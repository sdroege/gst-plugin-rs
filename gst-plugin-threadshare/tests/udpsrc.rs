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

extern crate glib;
use glib::prelude::*;

extern crate gstreamer as gst;
extern crate gstreamer_check as gst_check;

use std::thread;

extern crate gstthreadshare;

fn init() {
    use std::sync::{Once, ONCE_INIT};
    static INIT: Once = ONCE_INIT;

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static();
    });
}

#[test]
fn test_push() {
    init();

    let mut h = gst_check::Harness::new("ts-udpsrc");

    let caps = gst::Caps::new_simple("foo/bar", &[]);
    {
        let udpsrc = h.get_element().unwrap();
        udpsrc.set_property("caps", &caps).unwrap();
        udpsrc.set_property("port", &(5000 as u32)).unwrap();
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
        let dest = SocketAddr::new(ipaddr, 5000 as u16);

        for _ in 0..3 {
            socket.send_to(&buffer, dest).unwrap();
        }
    });

    for _ in 0..3 {
        let buffer = h.pull().unwrap();
        assert_eq!(buffer.get_size(), 160);
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
                let event_caps = ev.get_caps();
                assert_eq!(caps.as_ref(), event_caps);
            }
            EventView::Segment(..) => {
                assert_eq!(n_events, 2);
            }
            EventView::Eos(..) => {
                break;
            }
            _ => (),
        }
        n_events += 1;
    }
    assert!(n_events >= 3);
}
