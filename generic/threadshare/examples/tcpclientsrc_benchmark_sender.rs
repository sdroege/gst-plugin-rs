// Copyright (C) 2018 LEE Dongjun <redongjun@gmail.com>
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

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::{net, thread, time};

fn main() {
    let sockets: Arc<Mutex<Vec<net::TcpStream>>> = Arc::new(Mutex::new(vec![]));

    let streams = sockets.clone();
    let _handler = thread::spawn(move || {
        let listener = net::TcpListener::bind("0.0.0.0:40000").unwrap();
        for stream in listener.incoming() {
            streams.lock().unwrap().push(stream.unwrap());
        }
    });

    let buffer = [0; 160];
    let wait = time::Duration::from_millis(20);

    loop {
        let now = time::Instant::now();

        for mut socket in sockets.lock().unwrap().iter() {
            let _ = socket.write(&buffer);
        }

        let elapsed = now.elapsed();
        if elapsed < wait {
            thread::sleep(wait - elapsed);
        }
    }
}
