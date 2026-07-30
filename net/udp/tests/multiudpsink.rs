// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use std::{
    net,
    sync::{Arc, atomic},
    time::Duration,
};

const RECV_TIMEOUT: Duration = Duration::from_secs(5);
const NO_RECV_TIMEOUT: Duration = Duration::from_millis(500);

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsudp::plugin_register_static().unwrap();
    });
}

fn bind_receiver() -> net::UdpSocket {
    let socket = net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_read_timeout(Some(RECV_TIMEOUT)).unwrap();
    socket
}

#[track_caller]
fn recv(socket: &net::UdpSocket, expected: &[u8]) {
    let mut data = vec![0u8; expected.len()];
    let (n, _) = socket
        .recv_from(&mut data)
        .expect("timed out waiting for packet");
    assert_eq!(n, expected.len());
    assert_eq!(&data[..n], expected);
}

#[track_caller]
fn recv_nothing(socket: &net::UdpSocket) {
    socket.set_read_timeout(Some(NO_RECV_TIMEOUT)).unwrap();
    let mut data = [0u8; 65536];
    assert!(
        socket.recv_from(&mut data).is_err(),
        "expected no packet but one was received"
    );
    socket.set_read_timeout(Some(RECV_TIMEOUT)).unwrap();
}

fn setup(
    clients: &str,
    configure: impl FnOnce(&gst::Element),
) -> (gst_check::Harness, gst::Element) {
    init();
    let elem = gst::ElementFactory::make("multiudpsink2")
        .property("clients", clients)
        .build()
        .unwrap();
    configure(&elem);
    let mut h = gst_check::Harness::with_element(&elem, Some("sink"), None);
    h.set_src_caps_str("foo/bar");
    h.use_systemclock();
    h.play();
    (h, elem)
}

fn start_manual(elem: &gst::Element) {
    elem.set_state(gst::State::Playing).unwrap();
    let pad = elem.static_pad("sink").unwrap();
    assert!(
        pad.send_event(gst::event::StreamStart::new("test")),
        "stream-start event"
    );
    assert!(
        pad.send_event(gst::event::Caps::new(
            &gst::Caps::builder("foo/bar").build()
        )),
        "caps event"
    );
    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(
        pad.send_event(gst::event::Segment::new(&segment)),
        "segment event"
    );
}

fn setup_manual(clients: &str, configure: impl FnOnce(&gst::Element)) -> gst::Element {
    init();
    let elem = gst::ElementFactory::make("multiudpsink2")
        .property("clients", clients)
        // So that set_state(Playing) is synchronous
        .property("async", false)
        .build()
        .unwrap();
    configure(&elem);
    start_manual(&elem);
    elem
}

#[track_caller]
fn push_list(elem: &gst::Element, list: gst::BufferList) {
    elem.static_pad("sink")
        .unwrap()
        .chain_list(list)
        .expect("chain_list");
}

fn buffer_with_mems(mems: &[&[u8]]) -> gst::Buffer {
    let mut buf = gst::Buffer::new();
    for mem in mems {
        buf.get_mut()
            .unwrap()
            .append_memory(gst::Memory::from_slice(mem.to_vec()));
    }
    buf
}

fn payload(mems: &[&[u8]]) -> Vec<u8> {
    mems.iter().flat_map(|m| m.iter().copied()).collect()
}

fn emit_add(elem: &gst::Element, host: &str, port: u16) {
    elem.emit_by_name::<()>("add", &[&host, &(port as i32)]);
}

fn emit_remove(elem: &gst::Element, host: &str, port: u16) {
    elem.emit_by_name::<()>("remove", &[&host, &(port as i32)]);
}

fn get_stats(elem: &gst::Element, host: &str, port: u16) -> gst::Structure {
    elem.emit_by_name("get-stats", &[&host, &(port as i32)])
}

fn stat_u64(stats: &gst::Structure, field: &str) -> u64 {
    stats.get::<u64>(field).unwrap()
}

fn clients_str(elem: &gst::Element) -> Option<String> {
    elem.property::<Option<String>>("clients")
}

#[track_caller]
fn wait_for(cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + RECV_TIMEOUT;
    while !cond() {
        assert!(std::time::Instant::now() < deadline, "condition timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn test_multiudpsink_basic() {
    let rx = bind_receiver();
    let port = rx.local_addr().unwrap().port();

    let (mut h, _elem) = setup(&format!("127.0.0.1:{port}"), |_| {});

    // Each buffer is one UDP packet.
    h.push(gst::Buffer::from_slice(b"first packet")).unwrap();
    h.push(gst::Buffer::from_slice(b"second packet")).unwrap();

    recv(&rx, b"first packet");
    recv(&rx, b"second packet");
}

#[test]
fn test_multiudpsink_multiple_clients() {
    let rx1 = bind_receiver();
    let p1 = rx1.local_addr().unwrap().port();
    let rx2 = bind_receiver();
    let p2 = rx2.local_addr().unwrap().port();

    let (mut h, _elem) = setup(&format!("127.0.0.1:{p1},127.0.0.1:{p2}"), |_| {});

    h.push(gst::Buffer::from_slice(b"hello")).unwrap();

    // Both clients receive the same packet.
    recv(&rx1, b"hello");
    recv(&rx2, b"hello");
}

#[test]
fn test_multiudpsink_large_buffer() {
    const SIZE: usize = 48_000;
    let rx = bind_receiver();
    let port = rx.local_addr().unwrap().port();

    let data = (0..SIZE).map(|i| (i & 0xff) as u8).collect::<Vec<u8>>();

    let (mut h, _elem) = setup(&format!("127.0.0.1:{port}"), |_| {});

    // A large buffer is sent as a single packet.
    h.push(gst::Buffer::from_slice(data.clone())).unwrap();

    recv(&rx, &data);
}

#[test]
fn test_multiudpsink_buffer_list() {
    let rx = bind_receiver();
    let port = rx.local_addr().unwrap().port();

    // (a) two single-memory buffers; (b) a two- and a three-memory buffer,
    // testing the different sendmsg() / sendmmsg() code paths.
    let configs: &[Vec<Vec<&[u8]>>] = &[
        vec![vec![b"single one"], vec![b"single two"]],
        vec![
            vec![b"two a", b"two b"],
            vec![b"three a", b"three b", b"three c"],
        ],
    ];

    let elem = setup_manual(&format!("127.0.0.1:{port}"), |_| {});

    for config in configs {
        let list = config
            .iter()
            .map(|mems| buffer_with_mems(mems))
            .collect::<gst::BufferList>();
        push_list(&elem, list);
        for mems in config {
            recv(&rx, &payload(mems));
        }
    }

    elem.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_multiudpsink_bytes_properties() {
    const N: usize = 1234;
    let rx = bind_receiver();
    let port = rx.local_addr().unwrap().port();
    let data = [0xABu8; N];

    let (mut h, elem) = setup(&format!("127.0.0.1:{port}"), |_| {});

    h.push(gst::Buffer::from_slice(data)).unwrap();
    h.push(gst::Buffer::from_slice(data)).unwrap();

    recv(&rx, &data);
    recv(&rx, &data);

    // After both packets are sent the property values must match.
    wait_for(|| {
        elem.property::<u64>("bytes-to-serve") == 2 * N as u64
            && elem.property::<u64>("bytes-served") == 2 * N as u64
    });

    // Stopping resets the counters.
    elem.set_state(gst::State::Null).unwrap();
    assert_eq!(elem.property::<u64>("bytes-to-serve"), 0);
    assert_eq!(elem.property::<u64>("bytes-served"), 0);
}

#[test]
fn test_multiudpsink_clients_roundtrip() {
    init();

    // No clients set yet.
    let elem = gst::ElementFactory::make("multiudpsink2").build().unwrap();
    assert_eq!(clients_str(&elem).as_deref(), None);

    // Check that whitespace is trimmed.
    let elem = gst::ElementFactory::make("multiudpsink2")
        .property("clients", "127.0.0.1:5001, 127.0.0.1:5002")
        .build()
        .unwrap();
    assert_eq!(
        clients_str(&elem).as_deref(),
        Some("127.0.0.1:5001,127.0.0.1:5002")
    );

    // Empty string again means no clients.
    let elem = gst::ElementFactory::make("multiudpsink2")
        .property("clients", "")
        .build()
        .unwrap();
    assert_eq!(clients_str(&elem).as_deref(), None);

    // A resolvable hostname needs to stay as such.
    let elem = gst::ElementFactory::make("multiudpsink2")
        .property("clients", "localhost:5001")
        .build()
        .unwrap();
    assert_eq!(clients_str(&elem).as_deref(), Some("localhost:5001"));
}

#[test]
fn test_multiudpsink_clients_invalid() {
    init();

    // Port 0, port out of range, and a missing port are all rejected.
    // Only the valid entries remain.
    let elem = gst::ElementFactory::make("multiudpsink2")
        .property(
            "clients",
            "127.0.0.1:0,127.0.0.1:99999,127.0.0.1,127.0.0.1:5001",
        )
        .build()
        .unwrap();
    assert_eq!(clients_str(&elem).as_deref(), Some("127.0.0.1:5001"));
}

#[test]
fn test_multiudpsink_add_remove() {
    init();

    let elem = gst::ElementFactory::make("multiudpsink2")
        .property("clients", "127.0.0.1:5001")
        .build()
        .unwrap();

    let added = Arc::new(atomic::AtomicU32::new(0));
    let removed = Arc::new(atomic::AtomicU32::new(0));
    let added_c = added.clone();
    elem.connect("client-added", false, move |_args| {
        added_c.fetch_add(1, atomic::Ordering::SeqCst);
        None
    });
    let removed_c = removed.clone();
    elem.connect("client-removed", false, move |_args| {
        removed_c.fetch_add(1, atomic::Ordering::SeqCst);
        None
    });

    // A full remove emits client-removed.
    emit_remove(&elem, "127.0.0.1", 5001);
    assert_eq!(clients_str(&elem).as_deref(), None);
    assert_eq!(removed.load(atomic::Ordering::SeqCst), 1);

    // Adding emits client-added every time, even for duplicates.
    emit_add(&elem, "127.0.0.1", 5001);
    assert_eq!(clients_str(&elem).as_deref(), Some("127.0.0.1:5001"));
    assert_eq!(added.load(atomic::Ordering::SeqCst), 1);
    emit_add(&elem, "127.0.0.1", 5001);
    assert_eq!(clients_str(&elem).as_deref(), Some("127.0.0.1:5001"));
    assert_eq!(added.load(atomic::Ordering::SeqCst), 2);

    // Removing a duplicate once emits nothing and only decreases the counter.
    emit_remove(&elem, "127.0.0.1", 5001);
    assert_eq!(clients_str(&elem).as_deref(), Some("127.0.0.1:5001"));
    assert_eq!(removed.load(atomic::Ordering::SeqCst), 1);
    assert_eq!(added.load(atomic::Ordering::SeqCst), 2);

    // The second remove drops the entry and emits client-removed.
    emit_remove(&elem, "127.0.0.1", 5001);
    assert_eq!(clients_str(&elem).as_deref(), None);
    assert_eq!(removed.load(atomic::Ordering::SeqCst), 2);

    // Removing a nonexistent client emits nothing.
    emit_add(&elem, "127.0.0.1", 5001);
    assert_eq!(added.load(atomic::Ordering::SeqCst), 3);
    emit_remove(&elem, "127.0.0.1", 5002);
    assert_eq!(clients_str(&elem).as_deref(), Some("127.0.0.1:5001"));
    assert_eq!(removed.load(atomic::Ordering::SeqCst), 2);
    emit_remove(&elem, "127.0.0.1", 5001);
    assert_eq!(clients_str(&elem).as_deref(), None);
    assert_eq!(removed.load(atomic::Ordering::SeqCst), 3);

    // Multiple different clients (same host, and different hosts).
    emit_add(&elem, "127.0.0.1", 5001);
    emit_add(&elem, "127.0.0.1", 5002);
    assert_eq!(
        clients_str(&elem).as_deref(),
        Some("127.0.0.1:5001,127.0.0.1:5002")
    );
    emit_add(&elem, "127.0.0.2", 5001);
    assert_eq!(
        clients_str(&elem).as_deref(),
        Some("127.0.0.1:5001,127.0.0.1:5002,127.0.0.2:5001")
    );
    emit_remove(&elem, "127.0.0.1", 5001);
    assert_eq!(
        clients_str(&elem).as_deref(),
        Some("127.0.0.1:5002,127.0.0.2:5001")
    );
}

#[test]
fn test_multiudpsink_clear() {
    init();

    let elem = gst::ElementFactory::make("multiudpsink2").build().unwrap();
    emit_add(&elem, "127.0.0.1", 5001);
    emit_add(&elem, "127.0.0.1", 5002);
    assert_eq!(
        clients_str(&elem).as_deref(),
        Some("127.0.0.1:5001,127.0.0.1:5002")
    );

    let removed = Arc::new(atomic::AtomicU32::new(0));
    let removed_c = removed.clone();
    elem.connect("client-removed", false, move |_args| {
        removed_c.fetch_add(1, atomic::Ordering::SeqCst);
        None
    });

    elem.emit_by_name::<()>("clear", &[]);

    // Both clients should be removed.
    assert_eq!(clients_str(&elem).as_deref(), None);
    assert_eq!(removed.load(atomic::Ordering::SeqCst), 2);
}

#[test]
fn test_multiudpsink_get_stats() {
    const N: usize = 1000;
    let rx = bind_receiver();
    let port = rx.local_addr().unwrap().port();
    let data = [0x5Au8; N];

    let (mut h, elem) = setup(&format!("127.0.0.1:{port}"), |_| {});

    let stats = get_stats(&elem, "127.0.0.1", port);
    assert_eq!(stats.name(), "multiudpsink-stats");
    assert_eq!(stat_u64(&stats, "bytes-sent"), 0);
    assert_eq!(stat_u64(&stats, "packets-sent"), 0);
    assert!(stat_u64(&stats, "connect-time") > 0);
    assert_eq!(stat_u64(&stats, "disconnect-time"), 0);

    h.push(gst::Buffer::from_slice(data)).unwrap();
    h.push(gst::Buffer::from_slice(data)).unwrap();
    wait_for(|| {
        let stats = get_stats(&elem, "127.0.0.1", port);
        stat_u64(&stats, "bytes-sent") == 2 * N as u64 && stat_u64(&stats, "packets-sent") == 2
    });

    // An unknown client reports the default stats.
    let unknown = get_stats(&elem, "127.0.0.1", 5999);
    assert_eq!(stat_u64(&unknown, "bytes-sent"), 0);
    assert_eq!(stat_u64(&unknown, "packets-sent"), 0);
    assert!(stat_u64(&unknown, "connect-time") > 0);

    // Add the same client again and then remove it again, keeping it
    // once. This should preserve the stats.
    emit_add(&elem, "127.0.0.1", port);
    emit_remove(&elem, "127.0.0.1", port);
    let stats = get_stats(&elem, "127.0.0.1", port);
    assert_eq!(stat_u64(&stats, "bytes-sent"), 2 * N as u64);
    assert_eq!(stat_u64(&stats, "packets-sent"), 2);

    // Fully removing the client drops its stats back to the default.
    emit_remove(&elem, "127.0.0.1", port);
    emit_add(&elem, "127.0.0.1", port);
    let stats = get_stats(&elem, "127.0.0.1", port);
    assert_eq!(stat_u64(&stats, "bytes-sent"), 0);
    assert_eq!(stat_u64(&stats, "packets-sent"), 0);
}

#[test]
fn test_multiudpsink_send_duplicates() {
    let rx = bind_receiver();
    let port = rx.local_addr().unwrap().port();

    // List the same client twice, then add it once more. With send-duplicates each gets its own
    // copy, so every packet is received 3 times.
    let elem = setup_manual(&format!("127.0.0.1:{port},127.0.0.1:{port}"), |_| {});
    emit_add(&elem, "127.0.0.1", port);

    // One buffer with 2 memories: 3 packets should be received, each with both memories
    // concatenated.
    let buf = buffer_with_mems(&[b"part a", b"part b"]);
    push_list(&elem, [buf].into_iter().collect());
    let expected = payload(&[b"part a", b"part b"]);
    for _ in 0..3 {
        recv(&rx, &expected);
    }

    // A buffer list with 2 buffers: 3 packets per buffer, sent in order.
    let list = gst::BufferList::from([buffer_with_mems(&[b"one"]), buffer_with_mems(&[b"two"])]);
    push_list(&elem, list);
    for _ in 0..3 {
        recv(&rx, b"one");
    }
    for _ in 0..3 {
        recv(&rx, b"two");
    }

    // After disabling send-duplicates each different client should only receive one packet.
    elem.set_property("send-duplicates", false);
    push_list(&elem, [buffer_with_mems(&[b"solo"])].into_iter().collect());
    recv(&rx, b"solo");

    elem.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_multiudpsink_runtime_clients() {
    let rx1 = bind_receiver();
    let p1 = rx1.local_addr().unwrap().port();
    let rx2 = bind_receiver();
    let p2 = rx2.local_addr().unwrap().port();

    let (mut h, elem) = setup(&format!("127.0.0.1:{p1}"), |_| {});

    h.push(gst::Buffer::from_slice(b"one")).unwrap();
    recv(&rx1, b"one");
    recv_nothing(&rx2);

    emit_add(&elem, "127.0.0.1", p2);
    h.push(gst::Buffer::from_slice(b"two")).unwrap();
    recv(&rx1, b"two");
    recv(&rx2, b"two");

    emit_remove(&elem, "127.0.0.1", p1);
    h.push(gst::Buffer::from_slice(b"three")).unwrap();
    recv_nothing(&rx1);
    recv(&rx2, b"three");
}

#[cfg(unix)]
#[test]
fn test_multiudpsink_custom_socket() {
    use gio::prelude::*;

    init();
    // Provide an external socket and check that the element uses it and closes
    // it on stop (close-socket default true).
    for close_socket in [true, false] {
        let rx = bind_receiver();
        let port = rx.local_addr().unwrap().port();

        let provided = net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let gio_socket = gio::Socket::from_fd(provided.into()).unwrap();

        let elem = gst::ElementFactory::make("multiudpsink2")
            .property("clients", format!("127.0.0.1:{port}"))
            .property("socket", &gio_socket)
            .property("close-socket", close_socket)
            .property("async", false)
            .build()
            .unwrap();
        start_manual(&elem);

        // The external socket is used for v4. There should be no v6 socket.
        let used = elem.property::<Option<gio::Socket>>("used-socket");
        let used_v6 = elem.property::<Option<gio::Socket>>("used-socket-v6");
        assert!(used.is_some());
        assert!(used_v6.is_none());

        // Data flows through the provided socket.
        push_list(
            &elem,
            [buffer_with_mems(&[b"via socket"])].into_iter().collect(),
        );
        recv(&rx, b"via socket");

        // stop() runs on READY->NULL and closes the external socket.
        elem.set_state(gst::State::Null).unwrap();

        assert_eq!(
            gio_socket.is_closed(),
            close_socket,
            "socket closed state on close_socket={close_socket}"
        );
    }
}

#[cfg(unix)]
#[test]
fn test_multiudpsink_bind() {
    use gio::prelude::*;
    use socket2::SockRef;

    init();
    let rx = bind_receiver();
    let port = rx.local_addr().unwrap().port();

    let elem = gst::ElementFactory::make("multiudpsink2")
        .property("clients", format!("127.0.0.1:{port}"))
        .property("bind-address", "127.0.0.1")
        .property("bind-port", 0u32)
        .property("async", false)
        .build()
        .unwrap();
    start_manual(&elem);

    // The allocated socket is bound to the requested address and a dynamic non-zero port.
    let used = elem.property::<gio::Socket>("used-socket");
    let addr = SockRef::from(&used.fd())
        .local_addr()
        .unwrap()
        .as_socket()
        .unwrap();
    assert_eq!(
        addr.ip(),
        net::IpAddr::V4(net::Ipv4Addr::LOCALHOST),
        "bind address"
    );
    assert_ne!(addr.port(), 0, "bind port");

    push_list(&elem, [buffer_with_mems(&[b"bound"])].into_iter().collect());
    recv(&rx, b"bound");

    elem.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_multiudpsink_ipv6() {
    init();

    // Skip there is no IPv6 loopback.
    let rx6_probe = match net::UdpSocket::bind("[::1]:0") {
        Ok(s) => Some(s),
        Err(_) => {
            eprintln!("skipping test_multiudpsink_ipv6: no IPv6 loopback");
            return;
        }
    };
    drop(rx6_probe);

    let rx4 = bind_receiver();
    let p4 = rx4.local_addr().unwrap().port();
    let rx6 = net::UdpSocket::bind("[::1]:0").unwrap();
    let p6 = rx6.local_addr().unwrap().port();
    rx6.set_read_timeout(Some(RECV_TIMEOUT)).unwrap();

    let (mut h, elem) = setup(&format!("127.0.0.1:{p4},[::1]:{p6}"), |_| {});

    h.push(gst::Buffer::from_slice(b"dual stack")).unwrap();
    recv(&rx4, b"dual stack");
    recv(&rx6, b"dual stack");

    // The IPv6 IP is stored without brackets.
    assert_eq!(clients_str(&elem), Some(format!("127.0.0.1:{p4},::1:{p6}")));
}

#[test]
#[ignore]
fn test_multiudpsink_multicast() {
    init();

    const MCAST_GROUP: &str = "239.192.0.1";

    // Skip if multicast over loopback is not supported.
    let rx = net::UdpSocket::bind("0.0.0.0:0").unwrap();
    if rx
        .join_multicast_v4(&MCAST_GROUP.parse().unwrap(), &net::Ipv4Addr::LOCALHOST)
        .is_err()
    {
        eprintln!("skipping test_multiudpsink_multicast: no multicast on loopback");
        return;
    }
    let port = rx.local_addr().unwrap().port();
    rx.set_read_timeout(Some(RECV_TIMEOUT)).unwrap();

    let elem = setup_manual(&format!("{MCAST_GROUP}:{port}"), |_| {});

    // The sink auto-joins the group, so the receiver should receive the packet.
    push_list(
        &elem,
        [buffer_with_mems(&[b"multicast"])].into_iter().collect(),
    );
    recv(&rx, b"multicast");

    // Leaving the clients leaves the multicast group.
    elem.emit_by_name::<()>("clear", &[]);
    push_list(
        &elem,
        [buffer_with_mems(&[b"after leave"])].into_iter().collect(),
    );
    recv_nothing(&rx);

    elem.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_multiudpsink_no_clients() {
    init();

    // Pushing with no configured clients should simply do nothing.
    let rx = bind_receiver();

    let (mut h, _elem) = setup("", |_| {});

    h.push(gst::Buffer::from_slice(b"nowhere")).unwrap();
    recv_nothing(&rx);
}
