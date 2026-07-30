// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use atomic_refcell::AtomicRefCell;
use gst::{glib, prelude::*, subclass::prelude::*};
use gst_base::subclass::prelude::*;
use indexmap::{Equivalent, IndexMap};

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, LazyLock, Mutex},
};

use super::BaseUdpSinkImpl;
use crate::{
    net,
    uri::{self, Host},
};

#[cfg(target_os = "windows")]
use windows_sys::Win32::Networking::WinSock::{
    SOCKADDR_STORAGE, SOCKET, SOCKET_ERROR, WSABUF, WSAEINTR, WSAGetLastError, WSAMSG, WSASendMsg,
};

const WAKER_TOKEN: mio::Token = mio::Token(0);
const SOCKET_TOKEN: mio::Token = mio::Token(1);
const SOCKET_V6_TOKEN: mio::Token = mio::Token(2);

// Maximum number of messages per sendmmsg() call, limited by the kernel to
// UIO_MAXIOV on Linux/Android and IOV_MAX on the BSDs
#[cfg(any(target_os = "android", target_os = "linux"))]
const MAX_MESSAGES_PER_SENDMMSG: usize = libc::UIO_MAXIOV as usize;
#[cfg(any(target_os = "freebsd", target_os = "netbsd", target_os = "openbsd"))]
const MAX_MESSAGES_PER_SENDMMSG: usize = libc::IOV_MAX as usize;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "baseudpsink2",
        gst::DebugColorFlags::empty(),
        Some("Base UDP Sink"),
    )
});

#[derive(Debug)]
struct Settings {
    socket: Option<net::GioSocketWrapper>,
    socket_v6: Option<net::GioSocketWrapper>,
    close_socket: bool,
    used_socket: Option<net::GioSocketWrapper>,
    used_socket_v6: Option<net::GioSocketWrapper>,
    auto_multicast: bool,
    multicast_iface: Option<String>,
    ttl: u32,
    ttl_mc: u32,
    loop_: bool,
    qos_dscp: i32,
    send_duplicates: bool,
    buffer_size: u32,
    bind_address: Option<IpAddr>,
    bind_port: u16,

    clients: Arc<IndexMap<Client, ClientEntry>>,
    // true if clients were updated by the application since
    // the last time they were synced into the state
    clients_updated: bool,

    // Next generation to assign to new client entries
    next_generation: u64,

    bytes_to_serve: u64,
    bytes_served: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            socket: None,
            socket_v6: None,
            close_socket: true,
            used_socket: None,
            used_socket_v6: None,
            auto_multicast: true,
            multicast_iface: None,
            ttl: 64,
            ttl_mc: 1,
            loop_: true,
            qos_dscp: -1,
            send_duplicates: true,
            buffer_size: 0,
            bind_address: None,
            bind_port: 0,

            clients: Arc::new(IndexMap::new()),
            // Need to sync clients when we start
            clients_updated: true,

            next_generation: 0,

            bytes_to_serve: 0,
            bytes_served: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClientStats {
    pub bytes_sent: u64,
    pub packets_sent: u64,
    pub connect_time: u64,
    pub disconnect_time: u64,
}

impl Default for ClientStats {
    fn default() -> Self {
        Self {
            bytes_sent: 0,
            packets_sent: 0,
            connect_time: glib::real_time() as u64,
            disconnect_time: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Client {
    pub host: Host,
    pub port: u16,
}

/// A lookup key to find a `Client` by its host string and port without allocating one.
/// The `Equivalent`/`PartialEq`/`Hash` impls are consistent with `Client`'s as required by
/// `Equivalent`.
#[derive(Debug, Clone, Copy)]
pub struct ClientRef<'a>(&'a str, u16);

impl<'a> PartialEq<Client> for ClientRef<'a> {
    fn eq(&self, other: &Client) -> bool {
        self.1 == other.port && other.host == *self.0
    }
}

impl<'a> std::hash::Hash for ClientRef<'a> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Keep in sync with Host::hash, where a name never parses as an IpAddr
        match self.0.parse::<IpAddr>() {
            Ok(addr) => {
                0u8.hash(state);
                addr.hash(state);
            }
            Err(_) => {
                1u8.hash(state);
                self.0.hash(state);
            }
        }
        self.1.hash(state);
    }
}

impl<'a> Equivalent<Client> for ClientRef<'a> {
    fn equivalent(&self, key: &Client) -> bool {
        self == key
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClientEntry {
    pub add_count: usize,
    // Distinguishes a re-added client from the previous entry
    pub generation: u64,
    pub joined_multicast: bool,
    pub stats: ClientStats,
}

#[cfg(not(target_os = "windows"))]
#[repr(transparent)]
struct Iovec(libc::iovec);

#[cfg(target_os = "windows")]
#[repr(transparent)]
struct Iovec(WSABUF);

unsafe impl Send for Iovec {}
unsafe impl Sync for Iovec {}

#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "freebsd",
))]
#[repr(transparent)]
struct Mmsghdr(libc::mmsghdr);

#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "freebsd",
))]
unsafe impl Send for Mmsghdr {}
#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "freebsd",
))]
unsafe impl Sync for Mmsghdr {}

#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "freebsd",
))]
struct BufIovec {
    iovec_start: usize,
    iovec_count: usize,
    byte_length: usize,
}

struct State {
    poll: Option<mio::Poll>,
    events: mio::Events,
    socket: Option<net::UdpSendSocket>,
    socket_v6: Option<net::UdpSendSocket>,
    waker: Option<Arc<mio::Waker>>,
    clients: IndexMap<Client, ClientEntry>,
    // Cached allocations, cleared before re-use
    maps: Vec<gst::MappedMemory<gst::memory::Readable>>,
    iovecs: Vec<Iovec>,
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    ))]
    // Per-buffer (BufIovec into the flat iovecs array
    buf_iovecs: Vec<BufIovec>,
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    ))]
    mmsghdrs: Vec<Mmsghdr>,
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    ))]
    sockaddrs: Vec<libc::sockaddr_storage>,
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    ))]
    // Per-slot client index into clients, to match each sendmmsg() result to the
    // corresponding client
    msg_clients: Vec<usize>,
}

impl Default for State {
    fn default() -> Self {
        State {
            poll: None,
            events: mio::Events::with_capacity(2),
            socket: None,
            socket_v6: None,
            waker: None,
            clients: IndexMap::new(),
            maps: Vec::new(),
            iovecs: Vec::new(),
            #[cfg(any(
                target_os = "android",
                target_os = "linux",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "freebsd",
            ))]
            buf_iovecs: Vec::new(),
            #[cfg(any(
                target_os = "android",
                target_os = "linux",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "freebsd",
            ))]
            mmsghdrs: Vec::new(),
            #[cfg(any(
                target_os = "android",
                target_os = "linux",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "freebsd",
            ))]
            sockaddrs: Vec::new(),
            #[cfg(any(
                target_os = "android",
                target_os = "linux",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "freebsd",
            ))]
            msg_clients: Vec::new(),
        }
    }
}

pub struct BaseUdpSink {
    settings: Mutex<Settings>,
    state: AtomicRefCell<State>,
    waker: Mutex<Option<Arc<mio::Waker>>>,
}

impl Default for BaseUdpSink {
    fn default() -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
            state: AtomicRefCell::new(State::default()),
            waker: Mutex::new(None),
        }
    }
}

impl BaseUdpSink {
    /// Resolve a client host string, logging an error if it can't be resolved.
    fn resolve_client_host(host: &str) -> Option<Host> {
        match Host::resolve(host) {
            Ok(h) => Some(h),
            Err(err) => {
                gst::error!(CAT, "{err}");
                None
            }
        }
    }

    pub fn add_client(&self, host: &str, port: u16) -> bool {
        let Some(h) = Self::resolve_client_host(host) else {
            return false;
        };

        let mut settings = self.settings.lock().unwrap();
        let generation = settings.next_generation;
        let Some(is_new) =
            self.add_client_internal(h, port, Arc::make_mut(&mut settings.clients), generation)
        else {
            return false;
        };
        if is_new {
            settings.next_generation += 1;
        }
        // A re-add changes add_count in place, so existing clients need syncing too
        settings.clients_updated = true;
        true
    }

    fn add_client_internal(
        &self,
        host: Host,
        port: u16,
        clients: &mut IndexMap<Client, ClientEntry>,
        generation: u64,
    ) -> Option<bool> {
        if port == 0 {
            gst::error!(CAT, "Can't add client with port 0 for '{host}'");
            return None;
        }

        gst::info!(CAT, "Adding client {host}:{port}");
        let client = clients.entry(Client { host, port }).or_default();
        let is_new = client.add_count == 0;
        client.add_count += 1;
        if is_new {
            client.generation = generation;
        }
        Some(is_new)
    }

    pub fn remove_client(&self, host: &str, port: u16) -> bool {
        let mut settings = self.settings.lock().unwrap();

        let clients = Arc::make_mut(&mut settings.clients);
        let mut add_count_changed = false;
        let mut count_zero = false;
        if let Some(client) = clients.get_mut(&ClientRef(host, port)) {
            add_count_changed = true;
            client.add_count -= 1;
            count_zero = client.add_count == 0;
            if count_zero {
                client.stats.disconnect_time = glib::real_time() as u64;
            }
        }

        if count_zero {
            let _ = clients.shift_remove_entry(&ClientRef(host, port));
            gst::info!(CAT, "Removed client {host}:{port}");
        }

        // Need to update the add_count in the state clients copy
        if add_count_changed {
            settings.clients_updated = true;
        }

        count_zero
    }

    pub fn clear_clients(&self) -> Arc<IndexMap<Client, ClientEntry>> {
        let mut settings = self.settings.lock().unwrap();
        let clients = std::mem::take(&mut settings.clients);
        if !clients.is_empty() {
            gst::info!(CAT, "Cleared {} client(s)", clients.len());
            settings.clients_updated = true;
        }
        clients
    }

    pub fn clients(&self) -> Arc<IndexMap<Client, ClientEntry>> {
        self.settings.lock().unwrap().clients.clone()
    }

    pub fn set_clients(&self, clients_str: &str) {
        let mut settings = self.settings.lock().unwrap();
        let generation = settings.next_generation;
        let clients = Arc::make_mut(&mut settings.clients);
        let old = std::mem::replace(&mut *clients, IndexMap::new());

        for client in clients_str
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match uri::parse_client_str(client) {
                Ok((host, port)) => {
                    self.add_client_internal(host, port, clients, generation);
                }
                Err(err) => {
                    gst::error!(CAT, "Invalid client address '{client}': {err}");
                }
            }
        }

        gst::info!(
            CAT,
            "Replacing clients: {} -> {}",
            Self::format_clients(&old),
            Self::format_clients(clients)
        );
        settings.next_generation += 1;
        settings.clients_updated = true;
    }

    pub fn format_clients(clients: &IndexMap<Client, ClientEntry>) -> String {
        if clients.is_empty() {
            return "(none)".to_string();
        }
        clients
            .keys()
            .map(|c| format!("{}:{}", c.host.as_str(), c.port))
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn get_stats(&self, host: &str, port: u16) -> Option<ClientStats> {
        self.settings
            .lock()
            .unwrap()
            .clients
            .get(&ClientRef(host, port))
            .map(|client| client.stats.clone())
    }

    pub fn get_first_client(&self) -> Option<(Client, ClientEntry)> {
        let settings = self.settings.lock().unwrap();
        assert!(settings.clients.len() <= 1);
        settings
            .clients
            .first()
            .map(|(k, v)| (k.clone(), v.clone()))
    }

    pub fn set_client(&self, host: &str, port: u16) {
        let mut settings = self.settings.lock().unwrap();
        assert!(settings.clients.len() <= 1);

        if settings.clients.contains_key(&ClientRef(host, port)) {
            return;
        }

        let Some(h) = Self::resolve_client_host(host) else {
            return;
        };

        let generation = settings.next_generation;
        let clients = Arc::make_mut(&mut settings.clients);
        clients.clear();
        self.add_client_internal(h, port, clients, generation);
        settings.next_generation += 1;
        settings.clients_updated = true;
    }

    pub fn send_duplicates(&self) -> bool {
        let settings = self.settings.lock().unwrap();
        settings.send_duplicates
    }

    pub fn set_send_duplicates(&self, send_duplicates: bool) {
        let mut settings = self.settings.lock().unwrap();
        settings.send_duplicates = send_duplicates;
    }
}

#[glib::object_subclass]
impl ObjectSubclass for BaseUdpSink {
    const NAME: &'static str = "GstBaseUdpSink2";
    const ABSTRACT: bool = true;
    type Type = super::BaseUdpSink;
    type ParentType = gst_base::BaseSink;
    type Class = super::Class;
}

impl ObjectImpl for BaseUdpSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt64::builder("bytes-to-serve")
                    .nick("Bytes to serve")
                    .blurb("Number of bytes received to serve to clients")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("bytes-served")
                    .nick("Bytes served")
                    .blurb("Total number of bytes sent to all clients")
                    .read_only()
                    .build(),
                glib::ParamSpecObject::builder::<gio::Socket>("socket")
                    .nick("Socket")
                    .blurb("Socket to use for UDP sending. (None == allocate)")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecObject::builder::<gio::Socket>("socket-v6")
                    .nick("Socket IPv6")
                    .blurb("Socket to use for UDPv6 sending. (None == allocate)")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("close-socket")
                    .nick("Close socket")
                    .blurb("Close socket if passed as property on state change")
                    .default_value(Settings::default().close_socket)
                    .build(),
                glib::ParamSpecObject::builder::<gio::Socket>("used-socket")
                    .nick("Used Socket")
                    .blurb("Socket currently in use for UDP sending. (None = no socket)")
                    .read_only()
                    .build(),
                glib::ParamSpecObject::builder::<gio::Socket>("used-socket-v6")
                    .nick("Used Socket IPv6")
                    .blurb("Socket currently in use for UDPv6 sending. (None = no socket)")
                    .read_only()
                    .build(),
                glib::ParamSpecBoolean::builder("auto-multicast")
                    .nick("Auto multicast")
                    .blurb("Automatically join/leave multicast groups")
                    .default_value(Settings::default().auto_multicast)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("multicast-iface")
                    .nick("Multicast interface")
                    .blurb("The network interface on which to join the multicast group")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("ttl")
                    .nick("TTL")
                    .blurb("The unicast TTL parameter")
                    .minimum(0)
                    .maximum(255)
                    .default_value(Settings::default().ttl)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("ttl-mc")
                    .nick("TTL MC")
                    .blurb("The multicast TTL parameter")
                    .minimum(0)
                    .maximum(255)
                    .default_value(Settings::default().ttl_mc)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("loop")
                    .nick("Loop")
                    .blurb("Multicast loopback")
                    .default_value(Settings::default().loop_)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecInt::builder("qos-dscp")
                    .nick("QoS DSCP")
                    .blurb("Quality of Service differentiated services code point (-1 = default)")
                    .minimum(-1)
                    .maximum(63)
                    .default_value(Settings::default().qos_dscp)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("buffer-size")
                    .nick("Buffer size")
                    .blurb("Size of the kernel send buffer in bytes (0 = default)")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(Settings::default().buffer_size)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("bind-address")
                    .nick("Bind address")
                    .blurb("Address to bind the socket to")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("bind-port")
                    .nick("Bind port")
                    .blurb("Port to bind the socket to")
                    .minimum(0)
                    .maximum(65535)
                    .default_value(Settings::default().bind_port as u32)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "socket" => {
                let mut settings = self.settings.lock().unwrap();
                let socket = value
                    .get::<Option<gio::Socket>>()
                    .expect("type checked upstream")
                    .map(|s| net::GioSocketWrapper::new(s, true));
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing socket from {:?} to {:?}",
                    settings
                        .socket
                        .as_ref()
                        .map(net::GioSocketWrapper::as_socket),
                    socket.as_ref().map(net::GioSocketWrapper::as_socket),
                );
                settings.socket = socket;
            }
            "socket-v6" => {
                let mut settings = self.settings.lock().unwrap();
                let socket_v6 = value
                    .get::<Option<gio::Socket>>()
                    .expect("type checked upstream")
                    .map(|s| net::GioSocketWrapper::new(s, true));
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing socket-v6 from {:?} to {:?}",
                    settings
                        .socket_v6
                        .as_ref()
                        .map(net::GioSocketWrapper::as_socket),
                    socket_v6.as_ref().map(net::GioSocketWrapper::as_socket),
                );
                settings.socket_v6 = socket_v6;
            }
            "close-socket" => {
                let mut settings = self.settings.lock().unwrap();
                let close_socket = value.get::<bool>().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing close-socket from {} to {close_socket}",
                    settings.close_socket,
                );
                settings.close_socket = close_socket;
            }
            "auto-multicast" => {
                let mut settings = self.settings.lock().unwrap();
                let auto_multicast = value.get::<bool>().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing auto-multicast from {} to {auto_multicast}",
                    settings.auto_multicast,
                );
                settings.auto_multicast = auto_multicast;
                // Re-check multicast joins for existing clients
                settings.clients_updated = true;
            }
            "multicast-iface" => {
                let mut settings = self.settings.lock().unwrap();
                let multicast_iface = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing multicast-iface from {:?} to {multicast_iface:?}",
                    settings.multicast_iface,
                );
                settings.multicast_iface = multicast_iface;
            }
            "ttl" => {
                let mut settings = self.settings.lock().unwrap();
                let ttl = value.get::<u32>().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing ttl from {} to {ttl}",
                    settings.ttl,
                );
                settings.ttl = ttl;
            }
            "ttl-mc" => {
                let mut settings = self.settings.lock().unwrap();
                let ttl_mc = value.get::<u32>().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing ttl-mc from {} to {ttl_mc}",
                    settings.ttl_mc,
                );
                settings.ttl_mc = ttl_mc;
            }
            "loop" => {
                let mut settings = self.settings.lock().unwrap();
                let loop_ = value.get::<bool>().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing loop from {} to {loop_}",
                    settings.loop_,
                );
                settings.loop_ = loop_;
            }
            "qos-dscp" => {
                let mut settings = self.settings.lock().unwrap();
                let qos_dscp = value.get::<i32>().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing qos-dscp from {} to {qos_dscp}",
                    settings.qos_dscp,
                );
                settings.qos_dscp = qos_dscp;
            }
            "buffer-size" => {
                let mut settings = self.settings.lock().unwrap();
                let buffer_size = value.get::<u32>().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing buffer-size from {} to {buffer_size}",
                    settings.buffer_size,
                );
                settings.buffer_size = buffer_size;
            }
            "bind-address" => {
                let bind_address = value.get::<Option<&str>>().expect("type checked upstream");
                let mut settings = self.settings.lock().unwrap();
                if let Some(bind_address) = bind_address {
                    match bind_address.parse::<IpAddr>() {
                        Ok(addr) => {
                            gst::info!(
                                CAT,
                                imp = self,
                                "Changing bind-address from {:?} to {addr:?}",
                                settings.bind_address,
                            );
                            settings.bind_address = Some(addr);
                        }
                        Err(_) => {
                            gst::error!(
                                CAT,
                                imp = self,
                                "Couldn't parse bind-address '{bind_address}' as IP address"
                            );
                        }
                    }
                } else {
                    gst::info!(
                        CAT,
                        imp = self,
                        "Changing bind-address from {:?} to None",
                        settings.bind_address,
                    );
                    settings.bind_address = None;
                }
            }
            "bind-port" => {
                let mut settings = self.settings.lock().unwrap();
                let bind_port = value.get::<u32>().expect("type checked upstream") as u16;
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing bind-port from {} to {bind_port}",
                    settings.bind_port,
                );
                settings.bind_port = bind_port;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "bytes-to-serve" => {
                let settings = self.settings.lock().unwrap();
                settings.bytes_to_serve.to_value()
            }
            "bytes-served" => {
                let settings = self.settings.lock().unwrap();
                settings.bytes_served.to_value()
            }
            "socket" => {
                let settings = self.settings.lock().unwrap();
                settings
                    .socket
                    .as_ref()
                    .map(net::GioSocketWrapper::as_socket)
                    .to_value()
            }
            "socket-v6" => {
                let settings = self.settings.lock().unwrap();
                settings
                    .socket_v6
                    .as_ref()
                    .map(net::GioSocketWrapper::as_socket)
                    .to_value()
            }
            "close-socket" => {
                let settings = self.settings.lock().unwrap();
                settings.close_socket.to_value()
            }
            "used-socket" => {
                let settings = self.settings.lock().unwrap();
                settings
                    .used_socket
                    .as_ref()
                    .map(net::GioSocketWrapper::as_socket)
                    .to_value()
            }
            "used-socket-v6" => {
                let settings = self.settings.lock().unwrap();
                settings
                    .used_socket_v6
                    .as_ref()
                    .map(net::GioSocketWrapper::as_socket)
                    .to_value()
            }
            "auto-multicast" => {
                let settings = self.settings.lock().unwrap();
                settings.auto_multicast.to_value()
            }
            "multicast-iface" => {
                let settings = self.settings.lock().unwrap();
                settings.multicast_iface.to_value()
            }
            "ttl" => {
                let settings = self.settings.lock().unwrap();
                settings.ttl.to_value()
            }
            "ttl-mc" => {
                let settings = self.settings.lock().unwrap();
                settings.ttl_mc.to_value()
            }
            "loop" => {
                let settings = self.settings.lock().unwrap();
                settings.loop_.to_value()
            }
            "qos-dscp" => {
                let settings = self.settings.lock().unwrap();
                settings.qos_dscp.to_value()
            }
            "buffer-size" => {
                let settings = self.settings.lock().unwrap();
                settings.buffer_size.to_value()
            }
            "bind-address" => {
                let settings = self.settings.lock().unwrap();
                settings.bind_address.map(|a| a.to_string()).to_value()
            }
            "bind-port" => {
                let settings = self.settings.lock().unwrap();
                (settings.bind_port as u32).to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for BaseUdpSink {}

impl ElementImpl for BaseUdpSink {
    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::new_any();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseSinkImpl for BaseUdpSink {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.borrow_mut();
        let mut settings = self.settings.lock().unwrap();

        let poll = mio::Poll::new().map_err(|err| {
            gst::error_msg!(gst::ResourceError::OpenRead, ["Failed create poll: {err}"])
        })?;
        let waker = Arc::new(
            mio::Waker::new(poll.registry(), WAKER_TOKEN).map_err(|err| {
                gst::error_msg!(gst::ResourceError::OpenRead, ["Failed create waker: {err}"])
            })?,
        );

        {
            let mut waker_storage = self.waker.lock().unwrap();
            *waker_storage = Some(waker.clone());
        }

        let mut used_socket = None;
        let mut used_socket_v6 = None;

        if let Some(ref socket) = settings.socket {
            let socket = net::UdpSendSocket::wrap_socket(&*self.obj(), socket).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to wrap application socket: {err:?}"]
                )
            })?;

            let local_addr = socket.local_addr();
            gst::debug!(CAT, imp = self, "Application socket bound to {local_addr}");

            if local_addr.is_ipv4() {
                used_socket = Some(socket);
            } else {
                used_socket_v6 = Some(socket);
            }
        }

        if let Some(ref socket) = settings.socket_v6 {
            if used_socket_v6.is_some() {
                return Err(gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Application provided IPv6 socket as socket and socket-v6"]
                ));
            }

            let socket = net::UdpSendSocket::wrap_socket(&*self.obj(), socket).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to wrap application socket: {err:?}"]
                )
            })?;

            let local_addr = socket.local_addr();
            gst::debug!(CAT, imp = self, "Application socket bound to {local_addr}");

            if !local_addr.is_ipv6() {
                return Err(gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Application provided IPv4 socket as socket-v6"]
                ));
            }

            used_socket_v6 = Some(socket);
        }

        if used_socket.is_none() && used_socket_v6.is_none() {
            if let Some(bind_address) = settings.bind_address {
                let bind_saddr = SocketAddr::new(bind_address, settings.bind_port);

                let socket = net::UdpSendSocket::bind(
                    &*self.obj(),
                    settings.multicast_iface.as_deref(),
                    settings.ttl,
                    settings.ttl_mc,
                    settings.loop_,
                    settings.qos_dscp,
                    settings.buffer_size,
                    bind_saddr,
                )
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to create socket: {err:?}"]
                    )
                })?;

                let local_addr = socket.local_addr();
                gst::debug!(CAT, imp = self, "Socket bound to {local_addr}");

                if bind_saddr.is_ipv4() {
                    used_socket = Some(socket);
                } else {
                    used_socket_v6 = Some(socket);
                }
            } else {
                let bind_saddr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), settings.bind_port);
                let socket = net::UdpSendSocket::bind(
                    &*self.obj(),
                    settings.multicast_iface.as_deref(),
                    settings.ttl,
                    settings.ttl_mc,
                    settings.loop_,
                    settings.qos_dscp,
                    settings.buffer_size,
                    bind_saddr,
                )
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to create IPv4 socket: {err:?}"]
                    )
                })?;

                let local_addr = socket.local_addr();
                gst::debug!(CAT, imp = self, "IPv4 Socket bound to {local_addr}");

                used_socket = Some(socket);

                let bind_saddr = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), settings.bind_port);
                let res = net::UdpSendSocket::bind(
                    &*self.obj(),
                    settings.multicast_iface.as_deref(),
                    settings.ttl,
                    settings.ttl_mc,
                    settings.loop_,
                    settings.qos_dscp,
                    settings.buffer_size,
                    bind_saddr,
                );

                match res {
                    Ok(socket) => {
                        let local_addr = socket.local_addr();
                        gst::debug!(CAT, imp = self, "IPv6 Socket bound to {local_addr}");
                        used_socket_v6 = Some(socket);
                    }
                    Err(err) => {
                        gst::error!(CAT, imp = self, "Failed to create IPv6 socket: {err:?}");
                    }
                }
            }
        }

        if let Some(ref mut socket) = used_socket {
            poll.registry()
                .register(socket, SOCKET_TOKEN, mio::Interest::WRITABLE)
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to register socket with poll: {err}"]
                    )
                })?;
        }

        if let Some(ref mut socket) = used_socket_v6 {
            poll.registry()
                .register(socket, SOCKET_V6_TOKEN, mio::Interest::WRITABLE)
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to register socket with poll: {err}"]
                    )
                })?;
        }

        // Set up initial clients
        assert!(state.clients.is_empty());

        if settings.auto_multicast {
            let Settings {
                multicast_iface,
                clients,
                ..
            } = &mut *settings;
            let clients = Arc::make_mut(clients);

            for (client, entry) in clients.iter_mut() {
                if !client.host.addr.is_multicast() {
                    continue;
                }

                if client.host.addr.is_ipv4() {
                    if let Some(ref mut socket) = used_socket {
                        socket
                            .join_multicast(client.host.addr, multicast_iface.as_deref())
                            .map_err(|err| {
                                gst::error_msg!(
                                    gst::ResourceError::Settings,
                                    ["Failed to join multicast group: {err:?}"]
                                )
                            })?;
                        entry.joined_multicast = true;
                    }
                } else {
                    if let Some(ref mut socket) = used_socket_v6 {
                        socket
                            .join_multicast(client.host.addr, multicast_iface.as_deref())
                            .map_err(|err| {
                                gst::error_msg!(
                                    gst::ResourceError::Settings,
                                    ["Failed to join multicast group: {err:?}"]
                                )
                            })?;
                        entry.joined_multicast = true;
                    }
                }
            }
        }

        state.clients = IndexMap::clone(&settings.clients);
        settings.clients_updated = false;

        settings.used_socket = used_socket.as_ref().map(|s| s.socket_wrapper().clone());
        settings.used_socket_v6 = used_socket_v6.as_ref().map(|s| s.socket_wrapper().clone());

        state.poll = Some(poll);
        state.waker = Some(waker);
        state.socket = used_socket;
        state.socket_v6 = used_socket_v6;

        gst::info!(CAT, imp = self, "Started");

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.borrow_mut();
        let mut settings = self.settings.lock().unwrap();

        for socket in [settings.used_socket.take(), settings.used_socket_v6.take()]
            .into_iter()
            .flatten()
        {
            if socket.is_external() && settings.close_socket {
                use gio::prelude::*;

                let _ = socket.as_socket().close();
            }
        }

        *state = State::default();
        *self.waker.lock().unwrap() = None;

        // Reset stats and settings clients
        settings.bytes_served = 0;
        settings.bytes_to_serve = 0;
        for client in Arc::make_mut(&mut settings.clients).values_mut() {
            client.stats.bytes_sent = 0;
            client.stats.packets_sent = 0;
            client.joined_multicast = false;
        }

        // Need to sync clients again when we start
        settings.clients_updated = true;

        gst::info!(CAT, imp = self, "Stopped");

        Ok(())
    }

    fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        let send_duplicates;
        let mut bytes_to_serve;
        let mut bytes_served;
        {
            let mut settings = self.settings.lock().unwrap();
            send_duplicates = settings.send_duplicates;
            bytes_to_serve = settings.bytes_to_serve;
            bytes_served = settings.bytes_served;
            self.prepare(&mut state, &mut settings)?;
        }

        self.send(
            &mut state,
            send_duplicates,
            &mut bytes_to_serve,
            &mut bytes_served,
            &mut [buffer.as_ref()].into_iter(),
        )?;

        {
            let mut settings = self.settings.lock().unwrap();
            settings.bytes_to_serve = bytes_to_serve;
            settings.bytes_served = bytes_served;
            self.update_stats(&mut state, &mut settings)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn render_list(&self, list: &gst::BufferList) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        let send_duplicates;
        let mut bytes_to_serve;
        let mut bytes_served;
        {
            let mut settings = self.settings.lock().unwrap();
            send_duplicates = settings.send_duplicates;
            bytes_to_serve = settings.bytes_to_serve;
            bytes_served = settings.bytes_served;
            self.prepare(&mut state, &mut settings)?;
        }

        self.send(
            &mut state,
            send_duplicates,
            &mut bytes_to_serve,
            &mut bytes_served,
            &mut list.iter(),
        )?;

        {
            let mut settings = self.settings.lock().unwrap();
            settings.bytes_to_serve = bytes_to_serve;
            settings.bytes_served = bytes_served;
            self.update_stats(&mut state, &mut settings)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Unlocking");
        if let Some(waker) = self.waker.lock().unwrap().take() {
            let _ = waker.wake();
        }
        gst::debug!(CAT, imp = self, "Unlocked");

        Ok(())
    }

    fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Stopping unlocking");
        let state = self.state.borrow_mut();
        if let Some(ref waker) = state.waker {
            let mut waker_storage = self.waker.lock().unwrap();
            *waker_storage = Some(waker.clone());
        }
        gst::debug!(CAT, imp = self, "Stopped unlocking");

        Ok(())
    }
}

impl BaseUdpSinkImpl for BaseUdpSink {}

impl BaseUdpSink {
    fn prepare(
        &self,
        state: &mut State,
        settings: &mut Settings,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if settings.clients_updated {
            // First remove any clients that were removed from settings.
            let mut removed_clients = Vec::new();

            for (client, _entry) in state.clients.iter_mut() {
                if !settings.clients.contains_key(client) {
                    removed_clients.push(client.clone());
                }
            }

            for client in removed_clients {
                let entry = state.clients.shift_remove(&client).unwrap();

                if !entry.joined_multicast {
                    continue;
                }

                if client.host.addr.is_ipv4() {
                    if let Some(ref mut socket) = state.socket {
                        socket.leave_multicast(client.host.addr);
                    }
                } else {
                    if let Some(ref mut socket) = state.socket_v6 {
                        socket.leave_multicast(client.host.addr);
                    }
                }
            }

            // Now add any new clients and sync the changed ones. A removed and re-added client
            // has a new generation, so it is treated as new and its stats are reset.
            for (client, entry) in Arc::make_mut(&mut settings.clients).iter_mut() {
                if let Some(state_entry) = state.clients.get_mut(client)
                    && state_entry.generation != entry.generation
                {
                    // The client was re-created, so preserve its multicast
                    // membership to avoid unnecessary rejoining
                    entry.joined_multicast = state_entry.joined_multicast;
                }

                if settings.auto_multicast
                    && !entry.joined_multicast
                    && client.host.addr.is_multicast()
                {
                    if client.host.addr.is_ipv4() {
                        if let Some(ref mut socket) = state.socket {
                            if let Err(err) = socket.join_multicast(
                                client.host.addr,
                                settings.multicast_iface.as_deref(),
                            ) {
                                gst::element_imp_error!(
                                    self,
                                    gst::ResourceError::Settings,
                                    ["Failed to join multicast group: {err:?}"]
                                );
                                return Err(gst::FlowError::Error);
                            }
                            entry.joined_multicast = true;
                        }
                    } else {
                        if let Some(ref mut socket) = state.socket_v6 {
                            if let Err(err) = socket.join_multicast(
                                client.host.addr,
                                settings.multicast_iface.as_deref(),
                            ) {
                                gst::element_imp_error!(
                                    self,
                                    gst::ResourceError::Settings,
                                    ["Failed to join multicast group: {err:?}"]
                                );
                                return Err(gst::FlowError::Error);
                            }
                            entry.joined_multicast = true;
                        }
                    }
                }

                if let Some(state_entry) = state.clients.get_mut(client)
                    && state_entry.generation == entry.generation
                {
                    // Client with same generation already existed. Only add_count and (possibly)
                    // joined_multicast changed. Sync them in place to avoid re-cloning the entry
                    // and keep the existing state entry.
                    state_entry.add_count = entry.add_count;
                    state_entry.joined_multicast = entry.joined_multicast;
                } else {
                    // New or re-created client
                    state.clients.insert(client.clone(), entry.clone());
                }
            }
            settings.clients_updated = false;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn update_stats(
        &self,
        state: &mut State,
        settings: &mut Settings,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings_clients = Arc::make_mut(&mut settings.clients);

        for (client, entry) in state.clients.iter() {
            let Some(settings_entry) = settings_clients.get_mut(client) else {
                continue;
            };

            // A generation mismatch means the client was re-added after prepare() ran, so the
            // state stats are stale. prepare() will re-sync it on the next render.
            if settings_entry.generation != entry.generation {
                continue;
            }

            settings_entry.stats = entry.stats.clone();
        }

        Ok(gst::FlowSuccess::Ok)
    }

    /// Wait until the socket with `token` is writable, or a flush or error occurs.
    fn wait_for_writable(
        &self,
        poll: &mut mio::Poll,
        events: &mut mio::Events,
        other: Option<&mut net::UdpSendSocket>,
        other_token: mio::Token,
        token: mio::Token,
    ) -> Result<(), gst::FlowError> {
        if let Some(other) = other {
            // Idle UDP sockets are always writable, so deregister the other one
            let _ = poll.registry().deregister(&mut *other);
            let result = self.poll_until_writable(poll, events, token);
            let _ = poll
                .registry()
                .register(&mut *other, other_token, mio::Interest::WRITABLE);
            result
        } else {
            self.poll_until_writable(poll, events, token)
        }
    }

    /// Poll until the socket with `token` is writable, or a flush or error occurs.
    fn poll_until_writable(
        &self,
        poll: &mut mio::Poll,
        events: &mut mio::Events,
        token: mio::Token,
    ) -> Result<(), gst::FlowError> {
        loop {
            if let Err(err) = poll.poll(events, None) {
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                gst::error!(CAT, imp = self, "Poll error: {err:?}");
                return Err(gst::FlowError::Error);
            }

            for event in events.iter() {
                if event.token() == WAKER_TOKEN {
                    let waker_storage = self.waker.lock().unwrap();
                    if waker_storage.is_none() {
                        gst::debug!(CAT, imp = self, "Flushing");
                        return Err(gst::FlowError::Flushing);
                    }
                } else if event.token() == token {
                    if event.is_writable() || event.is_write_closed() {
                        // Write closed (e.g. ICMP error) too, so the retry surfaces the error
                        return Ok(());
                    } else if event.is_read_closed() || event.is_error() {
                        gst::error!(CAT, imp = self, "Socket error");
                        return Err(gst::FlowError::Error);
                    }
                }
                // Spurious or other-socket event. Keep polling
            }
        }
    }

    fn send(
        &self,
        state: &mut State,
        send_duplicates: bool,
        bytes_to_serve: &mut u64,
        bytes_served: &mut u64,
        buffers: &mut dyn Iterator<Item = &gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let result;
        #[cfg(any(
            target_os = "android",
            target_os = "linux",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "freebsd",
        ))]
        {
            result = self.sendmmsg(
                state,
                send_duplicates,
                bytes_to_serve,
                bytes_served,
                buffers,
            );
        }
        #[cfg(not(any(
            target_os = "android",
            target_os = "linux",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "freebsd",
        )))]
        {
            result = self.sendmsg(
                state,
                send_duplicates,
                bytes_to_serve,
                bytes_served,
                buffers,
            );
        }

        // Clear the cached allocations on all return paths, including errors and flushing
        Self::clear_send_caches(state);

        result
    }

    /// Map the buffer's memories into `state.maps` and `state.iovecs`, returning the total
    /// length
    #[cfg(not(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    )))]
    fn prepare_buffer(
        &self,
        state: &mut State,
        buffer: &gst::BufferRef,
        bytes_to_serve: &mut u64,
    ) -> Result<usize, gst::FlowError> {
        state.maps.clear();
        state.iovecs.clear();

        let mut length = 0;
        for mem in buffer.iter_memories_owned() {
            let Ok(map) = mem.into_mapped_memory_readable() else {
                gst::error!(CAT, imp = self, "Failed to map memory");
                return Err(gst::FlowError::Error);
            };

            length += map.size();

            state.maps.push(map);
        }

        *bytes_to_serve += length as u64;

        // At most 16 iovecs/memories per GStreamer buffer which is exactly the
        // minimum POSIX requirement for sendmsg(), so no further checks required here.
        for map in &state.maps {
            let slice = &map[..];
            #[cfg(not(target_os = "windows"))]
            state.iovecs.push(Iovec(libc::iovec {
                iov_base: slice.as_ptr() as *mut _,
                iov_len: slice.len(),
            }));
            #[cfg(target_os = "windows")]
            state.iovecs.push(Iovec(WSABUF {
                len: slice.len() as u32,
                buf: slice.as_ptr() as *mut _,
            }));
        }

        Ok(length)
    }

    /// Send a single datagram made up of the state iovecs to the given address with
    /// sendmsg(), returning the number of bytes sent
    #[cfg(not(any(
        target_os = "windows",
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    )))]
    fn send_one(
        iovecs: &mut Vec<Iovec>,
        fd: std::os::fd::RawFd,
        name: &libc::sockaddr_storage,
        namelen: u32,
    ) -> Result<usize, io::Error> {
        let mut hdr = unsafe { std::mem::zeroed::<libc::msghdr>() };
        hdr.msg_iov = iovecs.as_mut_ptr() as *mut libc::iovec;
        hdr.msg_iovlen = iovecs.len() as _;
        hdr.msg_name = name as *const _ as *mut _;
        hdr.msg_namelen = namelen;

        let n = unsafe { libc::sendmsg(fd, &hdr, 0) };
        if n == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    /// Send a single datagram made up of the state iovecs to the given address with
    /// WSASendMsg(), which works like sendmsg() but returns 0 on success and the number
    /// of bytes sent via lpdwBytesSent
    #[cfg(target_os = "windows")]
    fn send_one(
        iovecs: &mut Vec<Iovec>,
        socket: SOCKET,
        name: &SOCKADDR_STORAGE,
        namelen: u32,
    ) -> Result<usize, io::Error> {
        let mut msg = unsafe { std::mem::zeroed::<WSAMSG>() };
        msg.lpBuffers = iovecs.as_mut_ptr() as *mut WSABUF;
        msg.dwBufferCount = iovecs.len() as u32;
        msg.name = name as *const _ as *mut _;
        msg.namelen = namelen as i32;

        let mut sent = 0;
        let res = unsafe { WSASendMsg(socket, &msg, 0, &mut sent, std::ptr::null_mut(), None) };
        if res == SOCKET_ERROR {
            Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }))
        } else {
            Ok(sent as usize)
        }
    }

    #[cfg(not(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    )))]
    /// Map each buffer once, then send it to every IPv4 and IPv6 client with sendmsg(). The
    /// iovecs are shared by all clients since they only describe the data, not the
    /// destination.
    fn sendmsg(
        &self,
        state: &mut State,
        send_duplicates: bool,
        bytes_to_serve: &mut u64,
        bytes_served: &mut u64,
        buffers: &mut dyn Iterator<Item = &gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        for buffer in buffers {
            let length = self.prepare_buffer(state, buffer, bytes_to_serve)?;

            for (client, entry) in &mut state.clients {
                let dups = if send_duplicates { entry.add_count } else { 1 };
                let addr = SocketAddr::new(client.host.addr, client.port);

                let (socket, token) = if client.host.addr.is_ipv4() {
                    if let Some(ref mut socket) = state.socket {
                        (socket, SOCKET_TOKEN)
                    } else {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "No IPv4 socket available, not sending to {addr:?}"
                        );
                        continue;
                    }
                } else {
                    if let Some(ref mut socket) = state.socket_v6 {
                        (socket, SOCKET_V6_TOKEN)
                    } else {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "No IPv6 socket available, not sending to {addr:?}"
                        );
                        continue;
                    }
                };

                #[cfg(not(target_os = "windows"))]
                let fd = {
                    use std::os::fd::AsRawFd;
                    socket.socket().as_raw_fd()
                };
                #[cfg(target_os = "windows")]
                let fd = {
                    use std::os::windows::io::AsRawSocket;
                    socket.socket().as_raw_socket() as SOCKET
                };

                let (name, namelen) = net::fill_sockaddr(&addr);

                // Each dup is a separate datagram to the same address
                for _ in 0..dups {
                    'dup: loop {
                        let n = match Self::send_one(&mut state.iovecs, fd, &name, namelen) {
                            Ok(n) => n,
                            Err(err) => match err.kind() {
                                io::ErrorKind::Interrupted => continue 'dup,
                                #[cfg(target_os = "windows")]
                                _ if err.raw_os_error() == Some(WSAEINTR) => continue 'dup,
                                io::ErrorKind::WouldBlock => {
                                    // Send buffer full: wait until writable (or flushed), then retry
                                    let (other, other_token) = if token == SOCKET_TOKEN {
                                        (state.socket_v6.as_mut(), SOCKET_V6_TOKEN)
                                    } else {
                                        (state.socket.as_mut(), SOCKET_TOKEN)
                                    };
                                    self.wait_for_writable(
                                        state.poll.as_mut().unwrap(),
                                        &mut state.events,
                                        other,
                                        other_token,
                                        token,
                                    )?;
                                    continue 'dup;
                                }
                                _ => {
                                    gst::element_imp_error!(
                                        self,
                                        gst::ResourceError::Write,
                                        ["Failed to send data via sendmsg to {addr:?}: {err}"]
                                    );
                                    return Err(gst::FlowError::Error);
                                }
                            },
                        };

                        // UDP sends are atomic so n must equal the length
                        debug_assert_eq!(n, length, "UDP datagram not fully sent by sendmsg");

                        // Update the stats: bytes_served is the element total, entry.stats is
                        // per-client
                        *bytes_served += length as u64;
                        entry.stats.bytes_sent += length as u64;
                        entry.stats.packets_sent += 1;
                        break 'dup;
                    }
                }
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }

    #[cfg(not(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    )))]
    fn clear_send_caches(state: &mut State) {
        state.maps.clear();
        state.iovecs.clear();
    }

    /// Map all memories of all buffers into `state.maps` and `state.iovecs` and remember each
    /// buffer's iovec range in `state.buf_iovecs`
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    ))]
    fn prepare_all_buffers(
        &self,
        state: &mut State,
        buffers: &mut dyn Iterator<Item = &gst::BufferRef>,
        bytes_to_serve: &mut u64,
    ) -> Result<(), gst::FlowError> {
        state.maps.clear();
        state.iovecs.clear();
        state.buf_iovecs.clear();

        for buffer in buffers {
            let maps_start = state.maps.len();
            for mem in buffer.iter_memories_owned() {
                let Ok(map) = mem.into_mapped_memory_readable() else {
                    gst::error!(CAT, imp = self, "Failed to map memory");
                    return Err(gst::FlowError::Error);
                };
                state.maps.push(map);
            }

            let iovecs_start = state.iovecs.len();
            let mut length = 0;
            // At most 16 iovecs/memories per GStreamer buffer which is exactly the
            // minimum POSIX requirement for sendmsg(), so no further checks required here.
            for map in &state.maps[maps_start..] {
                let slice = &map[..];
                state.iovecs.push(Iovec(libc::iovec {
                    iov_base: slice.as_ptr() as *mut _,
                    iov_len: slice.len(),
                }));
                length += slice.len();
            }

            *bytes_to_serve += length as u64;
            let iovec_count = state.iovecs.len() - iovecs_start;
            state.buf_iovecs.push(BufIovec {
                iovec_start: iovecs_start,
                iovec_count,
                byte_length: length,
            });
        }

        Ok(())
    }

    /// Map all buffers once, then send them to every IPv4 and IPv6 client. The iovecs are
    /// shared by both families since they only describe the data, not the destination.
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    ))]
    fn sendmmsg(
        &self,
        state: &mut State,
        send_duplicates: bool,
        bytes_to_serve: &mut u64,
        bytes_served: &mut u64,
        buffers: &mut dyn Iterator<Item = &gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.prepare_all_buffers(state, buffers, bytes_to_serve)?;

        self.sendmmsg_family(state, socket2::Domain::IPV4, send_duplicates, bytes_served)?;
        self.sendmmsg_family(state, socket2::Domain::IPV6, send_duplicates, bytes_served)?;

        Ok(gst::FlowSuccess::Ok)
    }

    /// Send all prepared buffers via sendmmsg() to all clients of a single address family.
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    ))]
    fn sendmmsg_family(
        &self,
        state: &mut State,
        family: socket2::Domain,
        send_duplicates: bool,
        bytes_served: &mut u64,
    ) -> Result<(), gst::FlowError> {
        use std::os::fd::AsRawFd;

        let (socket, token, family_str) = match family {
            socket2::Domain::IPV4 => (state.socket.as_mut(), SOCKET_TOKEN, "IPv4"),
            socket2::Domain::IPV6 => (state.socket_v6.as_mut(), SOCKET_V6_TOKEN, "IPv6"),
            _ => unreachable!(),
        };

        let Some(socket) = socket else {
            for (client, _entry) in state.clients.iter() {
                if client.host.addr.is_ipv4() != matches!(family, socket2::Domain::IPV4) {
                    continue;
                }
                let addr = SocketAddr::new(client.host.addr, client.port);
                gst::warning!(
                    CAT,
                    imp = self,
                    "No {family_str} socket available, not sending to {addr:?}"
                );
            }
            return Ok(());
        };

        let fd = socket.socket().as_raw_fd();

        // Build the destination batch, one slot per (client, dup): a client with N dups
        // takes N contiguous slots sharing one address. sockaddrs[j] holds the address of
        // slot j and msg_clients[j] the corresponding client index, needed because a
        // client's dups are separate slots that all map to the same client.
        state.sockaddrs.clear();
        state.msg_clients.clear();

        // One address family per batch, so namelen is constant and we can take any
        let mut namelen = 0;
        for (idx, (client, entry)) in state.clients.iter().enumerate() {
            if client.host.addr.is_ipv4() != matches!(family, socket2::Domain::IPV4) {
                continue;
            }
            let dups = if send_duplicates { entry.add_count } else { 1 };
            let (name, nl) = net::fill_sockaddr(&SocketAddr::new(client.host.addr, client.port));
            namelen = nl;
            for _ in 0..dups {
                state.sockaddrs.push(name);
                state.msg_clients.push(idx);
            }
        }

        let family_total = state.sockaddrs.len();
        if family_total == 0 || state.buf_iovecs.is_empty() {
            return Ok(());
        }

        // Build one mmsghdr per (buffer, slot), forming a matrix with the buffers as rows
        // and the destination slots as columns. So message i is buffer i / family_total and
        // slot i % family_total (family_total is the number of slots). Messages of one
        // buffer share its iovec range and messages of one slot share its sockaddr.
        state.mmsghdrs.clear();
        state
            .mmsghdrs
            .reserve(state.buf_iovecs.len() * family_total);
        let iovec_base = state.iovecs.as_mut_ptr() as *mut libc::iovec;
        let name_base = state.sockaddrs.as_mut_ptr();
        for &BufIovec {
            iovec_start,
            iovec_count,
            byte_length: _,
        } in state.buf_iovecs.iter()
        {
            let iov_ptr = unsafe { iovec_base.add(iovec_start) };
            for i in 0..family_total {
                let mut msg_hdr = unsafe { std::mem::zeroed::<libc::msghdr>() };
                msg_hdr.msg_iov = iov_ptr;
                msg_hdr.msg_iovlen = iovec_count;
                msg_hdr.msg_name = unsafe { name_base.add(i) } as *mut _;
                msg_hdr.msg_namelen = namelen;
                state.mmsghdrs.push(Mmsghdr(libc::mmsghdr {
                    msg_hdr,
                    msg_len: 0,
                }));
            }
        }

        let total = state.mmsghdrs.len();
        let mmsg_ptr = state.mmsghdrs.as_mut_ptr() as *mut libc::mmsghdr;

        let mut start = 0;
        'batch: loop {
            // Clamp to the maximum number of messages per sendmmsg() and handle it like a short write
            let count = (total - start).min(MAX_MESSAGES_PER_SENDMMSG);
            let res = unsafe { libc::sendmmsg(fd, mmsg_ptr.add(start), count as u32, 0) };

            if res < 0 {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::Interrupted => continue 'batch,
                    io::ErrorKind::WouldBlock => {
                        // Send buffer full: wait until writable (or flushed), then retry
                        let (other, other_token) = if token == SOCKET_TOKEN {
                            (state.socket_v6.as_mut(), SOCKET_V6_TOKEN)
                        } else {
                            (state.socket.as_mut(), SOCKET_TOKEN)
                        };
                        self.wait_for_writable(
                            state.poll.as_mut().unwrap(),
                            &mut state.events,
                            other,
                            other_token,
                            token,
                        )?;
                        continue 'batch;
                    }
                    _ => {
                        gst::element_imp_error!(
                            self,
                            gst::ResourceError::Write,
                            ["Failed to send data via sendmmsg over {family_str}: {err}"]
                        );
                        return Err(gst::FlowError::Error);
                    }
                }
            }

            let n = res as usize;

            // Update stats for the sent messages, getting each one's buffer and slot from
            // its index. Use the kernel-reported mmsghdrs[i].msg_len as the byte count.
            for i in start..start + n {
                let sent_len = state.mmsghdrs[i].0.msg_len as usize;
                // UDP sends are atomic, so the kernel must have sent the whole datagram
                debug_assert_eq!(
                    sent_len,
                    state.buf_iovecs[i / family_total].byte_length,
                    "UDP datagram not fully sent by sendmmsg"
                );

                // i % family_total is the slot and msg_clients gives the corresponding client
                let client_idx = state.msg_clients[i % family_total];
                let entry = &mut state.clients[client_idx];
                *bytes_served += sent_len as u64;
                entry.stats.bytes_sent += sent_len as u64;
                entry.stats.packets_sent += 1;
            }

            start += n;

            if start >= total {
                break 'batch;
            }
        }

        Ok(())
    }

    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    ))]
    fn clear_send_caches(state: &mut State) {
        state.maps.clear();
        state.iovecs.clear();
        state.buf_iovecs.clear();
        state.mmsghdrs.clear();
        state.sockaddrs.clear();
        state.msg_clients.clear();
    }
}
