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

use std::net;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::{env, thread, time};

fn main() {
    let args = env::args().collect::<Vec<_>>();
    assert_eq!(args.len(), 2);
    let n_streams: u16 = args[1].parse().unwrap();

    let buffer = [0; 160];
    let socket = net::UdpSocket::bind("0.0.0.0:0").unwrap();

    let ipaddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let destinations = (40000..(40000 + n_streams))
        .map(|port| SocketAddr::new(ipaddr, port))
        .collect::<Vec<_>>();

    let wait = time::Duration::from_millis(20);

    thread::sleep(time::Duration::from_millis(1000));

    loop {
        let now = time::Instant::now();

        for dest in &destinations {
            socket.send_to(&buffer, dest).unwrap();
        }

        let elapsed = now.elapsed();
        if elapsed < wait {
            thread::sleep(wait - elapsed);
        }
    }
}
