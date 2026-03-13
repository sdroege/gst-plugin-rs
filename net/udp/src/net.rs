// GStreamer
//
// Copyright (C) 2015-2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::{
    collections::VecDeque,
    io, mem,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    sync::LazyLock,
};

use gst::prelude::*;

use getifaddrs::Interface;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "udp2",
        gst::DebugColorFlags::empty(),
        Some("Rust UDP Plugin"),
    )
});

pub struct UdpSocket {
    element: gst::Element,
    socket: SocketAndWrappedSocket,
    local_addr: SocketAddr,
    #[allow(unused)]
    preserve_packetization: bool,
    #[allow(clippy::type_complexity)]
    multicast_joined: Option<(IpAddr, Vec<(getifaddrs::Interface, Vec<IpAddr>)>)>,
    source_filter: Vec<IpAddr>,
    source_filter_exclusive: bool,
    buffer_pool: gst::BufferPool,
    batch_size: u32,
    #[allow(unused)]
    num_ctrl: usize,
    skip_first_bytes: u32,
    buffers_cache: VecDeque<gst::MappedBuffer<gst::buffer::Writable>>,
    sender_address_lru: lru::LruCache<SocketAddr, gio::SocketAddress>,
    #[cfg(any(target_os = "android", target_os = "linux"))]
    #[allow(clippy::type_complexity)]
    recvmmsg_cache: (
        Vec<libc::sockaddr_storage>,
        Vec<Ctrl>,
        Vec<libc::iovec>,
        Vec<libc::mmsghdr>,
        Vec<gst::MappedBuffer<gst::buffer::Writable>>,
    ),
    #[cfg(not(any(windows, target_os = "android", target_os = "linux")))]
    recvmsg_cache: Vec<Ctrl>,
}

unsafe impl Send for UdpSocket {}
unsafe impl Sync for UdpSocket {}

impl UdpSocket {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn socket_wrapper(&self) -> &GioSocketWrapper {
        &self.socket.wrapped_socket
    }

    #[allow(clippy::too_many_arguments)]
    pub fn bind(
        element: &impl AsRef<gst::Element>,
        saddr: SocketAddr,
        allow_reuse: bool,
        multicast_iface: Option<&str>,
        source_filter: &[IpAddr],
        source_filter_exclusive: bool,
        auto_multicast: bool,
        multicast_loop: bool,
        buffer_size: u32,
        batch_size: u32,
        allow_gro: bool,
        mtu: u32,
        preserve_packetization: bool,
        skip_first_bytes: u32,
    ) -> Result<Self, anyhow::Error> {
        use anyhow::Context as _;

        gst::debug!(CAT, obj = element.as_ref(), "Creating socket for {saddr}");

        let bind_saddr = if saddr.ip().is_multicast() {
            if saddr.is_ipv4() {
                SocketAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, saddr.port()))
            } else {
                SocketAddr::from(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, saddr.port(), 0, 0))
            }
        } else {
            saddr
        };

        let socket = socket2::Socket::new(
            if bind_saddr.is_ipv4() {
                socket2::Domain::IPV4
            } else {
                socket2::Domain::IPV6
            },
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .with_context(|| {
            format!(
                "Failed to create {} UDP socket",
                if bind_saddr.is_ipv4() { "IPv4" } else { "IPv6" }
            )
        })?;

        socket
            .set_nonblocking(true)
            .context("Failed to set socket non-blocking")?;

        if buffer_size > 0 {
            #[cfg(any(target_os = "android", target_os = "linux"))]
            {
                let mut force = false;
                if let Err(err) = socket.set_recv_buffer_size(buffer_size as usize) {
                    gst::warning!(
                        CAT,
                        obj = element.as_ref(),
                        "Failed to set receive buffer size of {buffer_size}: {err}"
                    );
                    force = true;
                }

                if let Ok(set_buffer_size) = socket.recv_buffer_size()
                    && set_buffer_size < buffer_size as usize
                {
                    gst::warning!(
                        CAT,
                        obj = element.as_ref(),
                        "Tried to set {buffer_size} as buffer size but only got {set_buffer_size}"
                    );
                    force = true;
                }

                if force {
                    unsafe {
                        use std::{mem, os::fd::AsRawFd};

                        let raw_fd = socket.as_raw_fd();
                        if libc::setsockopt(
                            raw_fd,
                            libc::SOL_SOCKET,
                            libc::SO_RCVBUFFORCE,
                            &buffer_size as *const u32 as *const _,
                            mem::size_of_val(&buffer_size) as u32,
                        ) != 0
                        {
                            let err = io::Error::last_os_error();
                            return Err(err).context("Failed to set receive buffer size");
                        }
                    }
                }
            }
            #[cfg(not(any(target_os = "android", target_os = "linux")))]
            {
                socket
                    .set_recv_buffer_size(buffer_size as usize)
                    .context("Failed to set receive buffer size")?;
            }

            if let Ok(set_buffer_size) = socket.recv_buffer_size()
                && set_buffer_size < buffer_size as usize
            {
                gst::warning!(
                    CAT,
                    obj = element.as_ref(),
                    "Tried to set {buffer_size} as buffer size but only got {set_buffer_size}"
                );
            }
        }

        if allow_reuse {
            if saddr.port() == 0 && !saddr.ip().is_multicast() {
                gst::warning!(
                    CAT,
                    obj = element.as_ref(),
                    "Disabling port reuse for dynamically allocated port to avoid potential conflicts"
                );
            } else {
                socket
                    .set_reuse_address(true)
                    .context("Failed to set SO_REUSEADDR")?;
                #[cfg(not(any(
                    windows,
                    target_os = "solaris",
                    target_os = "illumos",
                    target_os = "cygwin"
                )))]
                {
                    socket
                        .set_reuse_port(true)
                        .context("Failed to set SO_REUSEPORT")?;
                }
            }
        }

        #[allow(unused_mut)]
        let mut num_ctrl = 0;

        if saddr.ip().is_multicast() {
            #[cfg(any(target_os = "android", target_os = "linux"))]
            {
                if bind_saddr.is_ipv4() {
                    socket
                        .set_multicast_all_v4(false)
                        .context("Failed to disable IP_MULTICAST_ALL")?;
                } else {
                    socket
                        .set_multicast_all_v6(false)
                        .context("Failed to disable IPV6_MULTICAST_ALL")?;
                }
            }

            #[cfg(any(
                target_os = "android",
                target_os = "linux",
                target_os = "solaris",
                target_os = "illumos",
                target_os = "nto",
                target_os = "cygwin",
                target_os = "netbsd",
                target_os = "macos",
                target_os = "ios",
                target_os = "tvos",
                target_os = "watchos",
                target_os = "visionos"
            ))]
            if bind_saddr.is_ipv4() {
                unsafe {
                    use std::{mem, os::fd::AsRawFd};

                    let raw_fd = socket.as_raw_fd();

                    let v = 1u32;
                    if libc::setsockopt(
                        raw_fd,
                        libc::IPPROTO_IP,
                        libc::IP_PKTINFO,
                        &v as *const u32 as *const _,
                        mem::size_of_val(&v) as u32,
                    ) != 0
                    {
                        let err = io::Error::last_os_error();
                        return Err(err).context("Failed to enable IP_PKTINFO");
                    }
                    num_ctrl += 1;
                }
            }

            #[cfg(any(
                target_os = "hurd",
                target_os = "openbsd",
                target_os = "freebsd",
                target_os = "dragonfly",
            ))]
            if bind_saddr.is_ipv4() {
                unsafe {
                    use std::{mem, os::fd::AsRawFd};

                    let raw_fd = socket.as_raw_fd();

                    let v = 1u32;
                    if libc::setsockopt(
                        raw_fd,
                        libc::IPPROTO_IP,
                        libc::IP_RECVDSTADDR,
                        &v as *const u32 as *const _,
                        mem::size_of_val(&v) as u32,
                    ) != 0
                    {
                        let err = io::Error::last_os_error();
                        return Err(err).context("Failed to enable IP_RECVDSTADDR");
                    }
                    num_ctrl += 1;
                }
            }

            #[cfg(any(
                target_os = "android",
                target_os = "linux",
                target_os = "solaris",
                target_os = "illumos",
                target_os = "nto",
                target_os = "netbsd",
                target_os = "macos",
                target_os = "ios",
                target_os = "tvos",
                target_os = "watchos",
                target_os = "visionos",
                target_os = "hurd",
                target_os = "openbsd",
                target_os = "freebsd",
                target_os = "dragonfly",
            ))]
            if bind_saddr.is_ipv6() {
                unsafe {
                    use std::{mem, os::fd::AsRawFd};

                    let raw_fd = socket.as_raw_fd();
                    let v = 1u32;

                    if libc::setsockopt(
                        raw_fd,
                        libc::IPPROTO_IPV6,
                        libc::IPV6_RECVPKTINFO,
                        &v as *const u32 as *const _,
                        mem::size_of_val(&v) as u32,
                    ) != 0
                    {
                        let err = io::Error::last_os_error();
                        return Err(err).context("Failed to enable IPV6_RECVPKTINFO");
                    }
                    num_ctrl += 1;
                }
            }

            if bind_saddr.is_ipv4() {
                socket.set_multicast_loop_v4(multicast_loop)
            } else {
                socket.set_multicast_loop_v6(multicast_loop)
            }
            .with_context(|| {
                format!(
                    "Failed to {}able IP_MULTICAST_LOOP",
                    if multicast_loop { "en" } else { "dis" }
                )
            })?;
        }

        socket
            .bind(&bind_saddr.into())
            .with_context(|| format!("Failed to bind to {bind_saddr}"))?;

        let mut socket = Self::wrap_socket_internal(
            element.as_ref(),
            SocketAndWrappedSocket::from_socket2(socket)?,
            num_ctrl,
            batch_size,
            allow_gro,
            mtu,
            preserve_packetization,
            skip_first_bytes,
            source_filter,
            source_filter_exclusive,
        )?;

        if saddr.ip().is_multicast() && auto_multicast {
            socket.join_multicast(
                saddr.ip(),
                multicast_iface,
                source_filter,
                source_filter_exclusive,
            )?;
        }

        Ok(socket)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn wrap_socket(
        element: &impl AsRef<gst::Element>,
        socket: &GioSocketWrapper,
        source_filter: &[IpAddr],
        source_filter_exclusive: bool,
        batch_size: u32,
        allow_gro: bool,
        mtu: u32,
        preserve_packetization: bool,
        skip_first_bytes: u32,
    ) -> Result<Self, anyhow::Error> {
        let socket = Self::wrap_socket_internal(
            element.as_ref(),
            SocketAndWrappedSocket::from_wrapped_socket(socket)?,
            1, // One extra in case the application enabled IP_PKTINFO / IPV6_PKTINFO
            batch_size,
            allow_gro,
            mtu,
            preserve_packetization,
            skip_first_bytes,
            source_filter,
            source_filter_exclusive,
        )?;

        Ok(socket)
    }

    #[allow(clippy::too_many_arguments)]
    fn wrap_socket_internal(
        element: &gst::Element,
        socket: SocketAndWrappedSocket,
        #[allow(unused_mut)] mut num_ctrl: usize,
        batch_size: u32,
        #[allow(unused)]
        #[allow(unused_mut)]
        allow_gro: bool,
        mtu: u32,
        preserve_packetization: bool,
        skip_first_bytes: u32,
        source_filter: &[IpAddr],
        source_filter_exclusive: bool,
    ) -> Result<Self, anyhow::Error> {
        use anyhow::Context as _;

        #[allow(unused_mut)]
        let mut gro_segments = 1;
        #[cfg(any(target_os = "android", target_os = "linux"))]
        if allow_gro {
            unsafe {
                use std::{mem, os::fd::AsRawFd};

                let raw_fd = socket.as_raw_fd();
                let enabled = 1i32;
                if libc::setsockopt(
                    raw_fd,
                    libc::IPPROTO_UDP,
                    libc::UDP_GRO,
                    &enabled as *const i32 as *const _,
                    mem::size_of_val(&enabled) as u32,
                ) != 0
                {
                    let err = io::Error::last_os_error();
                    gst::warning!(CAT, obj = element, "Failed enabling UDP_GRO: {err}");
                } else {
                    // Maximum number of GRO segments per message that Linux would output.
                    // We need to have enough space for that many packets per message.
                    gst::debug!(CAT, obj = element, "Enabled UDP_GRO");
                    gro_segments = 64;
                    num_ctrl += 1;
                }
            }
        }

        #[cfg(any(target_os = "android", target_os = "linux"))]
        unsafe {
            use std::{mem, os::fd::AsRawFd};

            let raw_fd = socket.as_raw_fd();
            let v = 1u32;
            if libc::setsockopt(
                raw_fd,
                libc::SOL_SOCKET,
                libc::SO_TIMESTAMPNS,
                &v as *const u32 as *const _,
                mem::size_of_val(&v) as u32,
            ) != 0
            {
                let err = io::Error::last_os_error();
                gst::warning!(CAT, obj = element, "Failed enabling SO_TIMESTAMPNS: {err}");
            } else {
                gst::debug!(CAT, obj = element, "Enabled SO_TIMESTAMPNS");
                num_ctrl += 1;
            }
        }

        let buffer_pool = buffer_pool::BufferPool::new();
        let mut config = buffer_pool.config();
        config.set_params(None, mtu * gro_segments, batch_size * 2, 0);
        buffer_pool
            .set_config(config)
            .context("Failed to configure buffer pool")?;
        buffer_pool
            .set_active(true)
            .context("Failed to start buffer pool")?;

        let mut buffers_cache = VecDeque::with_capacity(batch_size as usize);
        for _ in 0..batch_size {
            let buffer = buffer_pool
                .acquire_buffer(None)
                .map_err(|_| anyhow::anyhow!("Failed to allocate buffer"))?;

            buffers_cache.push_back(buffer.into_mapped_buffer_writable().unwrap());
        }

        let local_addr = socket.local_addr().context("Can't get local address")?;

        Ok(Self {
            element: element.clone(),
            socket,
            local_addr,
            preserve_packetization,
            multicast_joined: None,
            source_filter: source_filter.to_vec(),
            source_filter_exclusive,
            buffer_pool: buffer_pool.upcast(),
            num_ctrl,
            batch_size,
            skip_first_bytes,
            buffers_cache,
            sender_address_lru: lru::LruCache::new(std::num::NonZero::new(16).unwrap()),
            #[cfg(any(target_os = "android", target_os = "linux"))]
            recvmmsg_cache: {
                use itertools::izip;
                use std::mem;

                let mut names = vec![
                    unsafe {
                        std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed().assume_init()
                    };
                    batch_size as usize
                ];

                let mut ctrls =
                    vec![
                        unsafe { std::mem::MaybeUninit::<Ctrl>::zeroed().assume_init() };
                        batch_size as usize * num_ctrl
                    ];

                let mut iovecs =
                    vec![
                        unsafe { std::mem::MaybeUninit::<libc::iovec>::zeroed().assume_init() };
                        batch_size as usize
                    ];

                let mut hdrs =
                    vec![
                        unsafe { std::mem::MaybeUninit::<libc::mmsghdr>::zeroed().assume_init() };
                        batch_size as usize
                    ];

                for (hdr, iovec, name, ctrls) in izip!(
                    hdrs.iter_mut(),
                    iovecs.iter_mut(),
                    names.iter_mut(),
                    ctrls.chunks_exact_mut(num_ctrl)
                ) {
                    hdr.msg_hdr.msg_iov = iovec;
                    hdr.msg_hdr.msg_iovlen = 1;

                    hdr.msg_hdr.msg_name = name as *mut libc::sockaddr_storage as *mut _;
                    hdr.msg_hdr.msg_namelen = mem::size_of_val(name) as u32;

                    hdr.msg_hdr.msg_control = ctrls.as_mut_ptr() as *mut _;
                    hdr.msg_hdr.msg_controllen = num_ctrl * mem::size_of::<Ctrl>();

                    hdr.msg_hdr.msg_flags = 0;
                }

                (
                    names,
                    ctrls,
                    iovecs,
                    hdrs,
                    Vec::with_capacity(batch_size as usize),
                )
            },
            #[cfg(not(any(windows, target_os = "android", target_os = "linux")))]
            recvmsg_cache: vec![
                unsafe { std::mem::MaybeUninit::<Ctrl>::zeroed().assume_init() };
                num_ctrl
            ],
        })
    }

    fn should_filter_packet(
        source_filter: &[IpAddr],
        source_filter_exclusive: bool,
        sender_addr: &SocketAddr,
    ) -> bool {
        if source_filter.is_empty() {
            return false;
        }

        let sender_ip = sender_addr.ip();

        if source_filter_exclusive {
            source_filter.contains(&sender_ip)
        } else {
            !source_filter.contains(&sender_ip)
        }
    }

    fn join_multicast(
        &mut self,
        addr: IpAddr,
        ifaces: Option<&str>,
        source_filter: &[IpAddr],
        source_filter_exclusive: bool,
    ) -> Result<(), anyhow::Error> {
        use anyhow::Context as _;

        if let Some(ifaces) = ifaces {
            gst::debug!(
                CAT,
                obj = self.element,
                "Joining multicast group {addr} for interfaces {ifaces}"
            );
        } else {
            gst::debug!(CAT, obj = self.element, "Joining multicast group {addr}");
        }

        if !source_filter.is_empty() {
            gst::debug!(
                CAT,
                obj = self.element,
                "Using source-filter {source_filter:?} (exclusive: {source_filter_exclusive})"
            );
        }

        if !addr.is_multicast() {
            gst::warning!(CAT, obj = self.element, "{addr} is not a multicast address");
            return Ok(());
        }

        let mut joined_ifaces = vec![];

        if let Some(ifaces) = ifaces {
            let ifaces = ifaces
                .split(',')
                .map(|s| s.to_string())
                .collect::<Vec<String>>();

            let iter = getifaddrs::getifaddrs().context("Failed to get interfaces")?;

            for iface in iter {
                // Skip interfaces of the wrong address family
                if iface.address.is_ipv4() && addr.is_ipv6()
                    || iface.address.is_ipv6() && addr.is_ipv4()
                    || (!iface.address.is_ipv6() && !iface.address.is_ipv4())
                {
                    continue;
                }

                if !ifaces.iter().any(|selected_iface| {
                    if &iface.name == selected_iface {
                        return true;
                    }

                    // check if name matches the interface description (Friendly name) on Windows
                    #[cfg(windows)]
                    if &iface.description == selected_iface {
                        return true;
                    }

                    false
                }) {
                    continue;
                }

                if !iface.flags.contains(getifaddrs::InterfaceFlags::MULTICAST) {
                    gst::warning!(
                        CAT,
                        obj = self.element,
                        "Skipping interface {}: does not support multicast",
                        iface.name
                    );
                    continue;
                }

                if !iface.flags.contains(getifaddrs::InterfaceFlags::UP) {
                    gst::warning!(
                        CAT,
                        obj = self.element,
                        "Skipping interface {}: not up",
                        iface.name
                    );
                    continue;
                }

                gst::trace!(
                    CAT,
                    obj = self.element,
                    "Selecting interface {}",
                    iface.name
                );

                joined_ifaces.push(iface);
            }
        }

        if joined_ifaces.is_empty() {
            joined_ifaces.push(getifaddrs::Interface {
                name: "default".to_owned(),
                #[cfg(windows)]
                description: "default".to_owned(),

                address: if addr.is_ipv4() {
                    getifaddrs::Address::V4(getifaddrs::NetworkAddress {
                        address: Ipv4Addr::UNSPECIFIED,
                        netmask: None,
                        associated_address: None,
                    })
                } else {
                    getifaddrs::Address::V6(getifaddrs::NetworkAddress {
                        address: Ipv6Addr::UNSPECIFIED,
                        netmask: None,
                        associated_address: None,
                    })
                },
                flags: getifaddrs::InterfaceFlags::UP,
                index: Some(0),
            });
        }

        let mut joined_ifaces_and_sources = Vec::new();

        match addr {
            IpAddr::V4(ref addr) => {
                for iface in &joined_ifaces {
                    // Only use source filter if there is one, it's not
                    // exclusive and this platform supports it.
                    //
                    // Otherwise just join the multicast group as usual.
                    let use_source_filter = !source_filter.is_empty()
                        && !source_filter_exclusive
                        && cfg!(not(any(
                            target_os = "dragonfly",
                            target_os = "haiku",
                            target_os = "hurd",
                            target_os = "netbsd",
                            target_os = "openbsd",
                            target_os = "redox",
                            target_os = "fuchsia",
                            target_os = "nto",
                            target_os = "espidf",
                            target_os = "vita",
                        )));

                    if !use_source_filter {
                        gst::trace!(
                            CAT,
                            obj = self.element,
                            "Joining the multicast group {addr} with interface {}",
                            iface.name
                        );

                        self.multicast_group_operation_v4(addr, iface, true)
                            .with_context(|| {
                                format!(
                                    "Joining the multicast group {addr} with interface {}",
                                    iface.name
                                )
                            })?;
                        joined_ifaces_and_sources.push((iface.clone(), Vec::new()));
                    } else {
                        let socket = socket2::SockRef::from(&*self.socket);
                        let mut sources = Vec::new();
                        for source in source_filter {
                            let source = match source {
                                IpAddr::V4(ip) => ip,
                                IpAddr::V6(_) => continue,
                            };

                            let iface_addr = match iface.address {
                                getifaddrs::Address::V4(ref network_address) => {
                                    network_address.address
                                }
                                getifaddrs::Address::V6(_) | getifaddrs::Address::Mac(_) => {
                                    unreachable!()
                                }
                            };

                            gst::trace!(
                                CAT,
                                obj = self.element,
                                "Joining the multicast group {addr} with interface {} and source {source}",
                                iface.name
                            );

                            #[cfg(not(any(
                                target_os = "dragonfly",
                                target_os = "haiku",
                                target_os = "hurd",
                                target_os = "netbsd",
                                target_os = "openbsd",
                                target_os = "redox",
                                target_os = "fuchsia",
                                target_os = "nto",
                                target_os = "espidf",
                                target_os = "vita",
                            )))]
                            {
                                socket.join_ssm_v4(source, addr, &iface_addr)
                                    .with_context(|| {
                                        format!(
                                            "Joining the multicast group {addr} with interface {} and source {source}",
                                            iface.name
                                        )
                                    })?;
                            }
                            #[cfg(any(
                                target_os = "dragonfly",
                                target_os = "haiku",
                                target_os = "hurd",
                                target_os = "netbsd",
                                target_os = "openbsd",
                                target_os = "redox",
                                target_os = "fuchsia",
                                target_os = "nto",
                                target_os = "espidf",
                                target_os = "vita",
                            ))]
                            {
                                // Checked above
                                unreachable!();
                            }

                            sources.push(IpAddr::V4(*source));
                        }
                        joined_ifaces_and_sources.push((iface.clone(), sources));
                    }
                }
            }
            IpAddr::V6(ref addr) => {
                for iface in &joined_ifaces {
                    let socket = socket2::SockRef::from(&*self.socket);

                    // Only use source filter if there is one, it's not
                    // exclusive and this platform supports it.
                    //
                    // Otherwise just join the multicast group as usual.
                    let use_source_filter = !source_filter.is_empty()
                        && !source_filter_exclusive
                        && cfg!(any(target_os = "linux", target_os = "android"));

                    if !use_source_filter {
                        gst::trace!(
                            CAT,
                            obj = self.element,
                            "Joining the multicast group {addr} with interface {}",
                            iface.name
                        );

                        socket
                            .join_multicast_v6(addr, iface.index.unwrap_or(0))
                            .with_context(|| {
                                format!(
                                    "Joining the multicast group {addr} with interface {}",
                                    iface.name
                                )
                            })?;
                        joined_ifaces_and_sources.push((iface.clone(), Vec::new()));
                    } else {
                        let mut sources = Vec::new();
                        for source in source_filter {
                            let source = match source {
                                IpAddr::V6(ip) => ip,
                                IpAddr::V4(_) => continue,
                            };

                            gst::trace!(
                                CAT,
                                obj = self.element,
                                "Joining the multicast group {addr} with interface {} and source {source}",
                                iface.name
                            );

                            self.multicast_group_operation_v6_ssm(addr, iface, source, true)
                                    .with_context(|| {
                                        format!(
                                            "Joining the multicast group {addr} with interface {} and source {source}",
                                            iface.name
                                        )
                                    })?;
                            sources.push(IpAddr::V6(*source));
                        }
                        joined_ifaces_and_sources.push((iface.clone(), sources));
                    }
                }
            }
        }

        self.multicast_joined = Some((addr, joined_ifaces_and_sources));

        Ok(())
    }

    pub fn leave_multicast(&mut self) {
        let Some((addr, joined_ifaces)) = self.multicast_joined.take() else {
            return;
        };

        match addr {
            IpAddr::V4(addr) => {
                for (iface, sources) in joined_ifaces {
                    if sources.is_empty() {
                        gst::debug!(
                            CAT,
                            obj = self.element,
                            "interface {} leaving the multicast {addr}",
                            iface.name
                        );

                        // use the custom written API to be able to pass the interface index
                        // for all types of target OS
                        if let Err(err) = self.multicast_group_operation_v4(&addr, &iface, false) {
                            gst::warning!(
                                CAT,
                                obj = self.element,
                                "Failed to leave multicast group: {err}"
                            );
                        }
                    } else {
                        for source in sources {
                            let source = match source {
                                IpAddr::V4(ip) => ip,
                                IpAddr::V6(_) => continue,
                            };

                            let iface_addr = match iface.address {
                                getifaddrs::Address::V4(ref network_address) => {
                                    network_address.address
                                }
                                getifaddrs::Address::V6(_) | getifaddrs::Address::Mac(_) => {
                                    unreachable!()
                                }
                            };

                            gst::debug!(
                                CAT,
                                obj = self.element,
                                "interface {} leaving the multicast {addr} and source {source}",
                                iface.name
                            );

                            #[cfg(not(any(
                                target_os = "dragonfly",
                                target_os = "haiku",
                                target_os = "hurd",
                                target_os = "netbsd",
                                target_os = "openbsd",
                                target_os = "redox",
                                target_os = "fuchsia",
                                target_os = "nto",
                                target_os = "espidf",
                                target_os = "vita",
                            )))]
                            {
                                let socket = socket2::SockRef::from(&*self.socket);
                                // use the custom written API to be able to pass the interface index
                                // for all types of target OS
                                if let Err(err) = socket.leave_ssm_v4(&source, &addr, &iface_addr) {
                                    gst::warning!(
                                        CAT,
                                        obj = self.element,
                                        "Failed to leave multicast group: {err}"
                                    );
                                }
                            }
                            #[cfg(any(
                                target_os = "dragonfly",
                                target_os = "haiku",
                                target_os = "hurd",
                                target_os = "netbsd",
                                target_os = "openbsd",
                                target_os = "redox",
                                target_os = "fuchsia",
                                target_os = "nto",
                                target_os = "espidf",
                                target_os = "vita",
                            ))]
                            {
                                unreachable!();
                            }
                        }
                    }
                }
            }
            IpAddr::V6(addr) => {
                for (iface, sources) in joined_ifaces {
                    let socket = socket2::SockRef::from(&*self.socket);
                    if sources.is_empty() {
                        gst::debug!(
                            CAT,
                            obj = self.element,
                            "interface {} leaving the multicast {addr}",
                            iface.name
                        );
                        if let Err(err) = socket.leave_multicast_v6(&addr, iface.index.unwrap_or(0))
                        {
                            gst::warning!(
                                CAT,
                                obj = self.element,
                                "Failed to leave multicast group: {err}"
                            );
                        };
                    } else {
                        for source in sources {
                            let source = match source {
                                IpAddr::V6(ip) => ip,
                                IpAddr::V4(_) => continue,
                            };

                            gst::debug!(
                                CAT,
                                obj = self.element,
                                "interface {} leaving the multicast {addr} and source {source}",
                                iface.name
                            );

                            // use the custom written API to be able to pass the interface index
                            // for all types of target OS
                            if let Err(err) =
                                self.multicast_group_operation_v6_ssm(&addr, &iface, &source, false)
                            {
                                gst::warning!(
                                    CAT,
                                    obj = self.element,
                                    "Failed to leave multicast group: {err}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    fn multicast_group_operation_v4(
        &mut self,
        addr: &Ipv4Addr,
        iface: &Interface,
        join: bool,
    ) -> Result<(), io::Error> {
        let socket = socket2::SockRef::from(&*self.socket);

        #[cfg(not(any(
            target_os = "aix",
            target_os = "haiku",
            target_os = "illumos",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "redox",
            target_os = "solaris",
            target_os = "nto",
            target_os = "espidf",
            target_os = "vita",
            target_os = "cygwin",
        )))]
        {
            let index = iface.index.unwrap_or(0);

            if join {
                socket
                    .join_multicast_v4_n(addr, &socket2::InterfaceIndexOrAddress::Index(index))?;
            } else {
                socket
                    .leave_multicast_v4_n(addr, &socket2::InterfaceIndexOrAddress::Index(index))?;
            }

            Ok(())
        }

        #[cfg(any(
            target_os = "aix",
            target_os = "haiku",
            target_os = "illumos",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "redox",
            target_os = "solaris",
            target_os = "nto",
            target_os = "espidf",
            target_os = "vita",
            target_os = "cygwin",
        ))]
        {
            let ip_addr = match iface.address {
                getifaddrs::Address::V4(ref network_address) => network_address.address,
                getifaddrs::Address::V6(_) | getifaddrs::Address::Mac(_) => unreachable!(),
            };

            if join {
                socket.join_multicast_v4(addr, &ip_addr)?;
            } else {
                socket.leave_multicast_v4(addr, &ip_addr)?;
            }

            Ok(())
        }
    }

    fn multicast_group_operation_v6_ssm(
        &mut self,
        #[allow(unused)] addr: &Ipv6Addr,
        #[allow(unused)] iface: &Interface,
        #[allow(unused)] source: &Ipv6Addr,
        #[allow(unused)] join: bool,
    ) -> Result<(), io::Error> {
        #[cfg(any(target_os = "linux", target_os = "android",))]
        {
            let socket = socket2::SockRef::from(&*self.socket);

            #[repr(C)]
            struct group_source_req {
                gsr_interface: u32,
                gsr_group: libc::sockaddr_storage,
                gsr_source: libc::sockaddr_storage,
            }

            let index = iface.index.unwrap_or(0);

            unsafe {
                use std::os::fd::AsRawFd;

                let mut req = group_source_req {
                    gsr_interface: index,
                    gsr_group: mem::zeroed(),
                    gsr_source: mem::zeroed(),
                };

                let gsr_group = &mut *(&mut req.gsr_group as *mut libc::sockaddr_storage
                    as *mut libc::sockaddr_in6);
                gsr_group.sin6_family = libc::AF_INET6 as u16;
                gsr_group.sin6_addr.s6_addr = addr.octets();
                gsr_group.sin6_port = 0;
                gsr_group.sin6_flowinfo = 0;
                gsr_group.sin6_scope_id = 0;

                let gsr_source = &mut *(&mut req.gsr_source as *mut libc::sockaddr_storage
                    as *mut libc::sockaddr_in6);
                gsr_source.sin6_family = libc::AF_INET6 as u16;
                gsr_source.sin6_addr.s6_addr = source.octets();
                gsr_source.sin6_port = 0;
                gsr_source.sin6_flowinfo = 0;
                gsr_source.sin6_scope_id = 0;

                let raw_fd = socket.as_raw_fd();
                if join {
                    if libc::setsockopt(
                        raw_fd,
                        libc::IPPROTO_IPV6,
                        libc::MCAST_JOIN_SOURCE_GROUP,
                        &req as *const group_source_req as *const _,
                        mem::size_of_val(&req) as u32,
                    ) != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                } else if libc::setsockopt(
                    raw_fd,
                    libc::IPPROTO_IPV6,
                    libc::MCAST_LEAVE_SOURCE_GROUP,
                    &req as *const group_source_req as *const _,
                    mem::size_of_val(&req) as u32,
                ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
            }

            Ok(())
        }

        #[cfg(not(any(target_os = "linux", target_os = "android",)))]
        {
            // Checked in the caller
            unreachable!();
        }
    }

    pub fn recv(&mut self) -> Result<Option<BufferOrList>, anyhow::Error> {
        #[cfg(any(
            target_os = "android",
            target_os = "linux",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "freebsd",
        ))]
        {
            self.recv_mmsg()
        }
        #[cfg(not(any(
            windows,
            target_os = "android",
            target_os = "linux",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "freebsd",
        )))]
        {
            self.recv_msg()
        }
        #[cfg(windows)]
        {
            self.recv_from()
        }
    }

    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    ))]
    pub fn recv_mmsg(&mut self) -> Result<Option<BufferOrList>, anyhow::Error> {
        use anyhow::Context as _;
        use itertools::izip;

        let Some(clock) = self.element.clock() else {
            gst::error!(CAT, obj = self.element, "Need a clock");
            anyhow::bail!("Need a clock!");
        };
        let Some(base_time) = self.element.base_time() else {
            gst::error!(CAT, obj = self.element, "Need a base time");
            anyhow::bail!("Need a base time!");
        };

        let address = self.multicast_joined.as_ref().map(|(addr, _)| *addr);

        let mut res = None;
        let (names, ctrls, iovecs, hdrs, buffers) = &mut self.recvmmsg_cache;

        buffers.clear();
        for _ in 0..self.batch_size {
            buffers.push({
                let Some(buffer) = self.buffers_cache.pop_back().or_else(|| {
                    self.buffer_pool
                        .acquire_buffer(None)
                        .ok()
                        .map(|b| b.into_mapped_buffer_writable().unwrap())
                }) else {
                    anyhow::bail!("Failed to allocate buffer");
                };

                buffer
            });
        }

        while self.buffers_cache.len() < self.batch_size as usize {
            self.buffers_cache.push_back(
                self.buffer_pool
                    .acquire_buffer(None)
                    .map(|b| b.into_mapped_buffer_writable().unwrap())
                    .map_err(|_| anyhow::anyhow!("Failed to allocate buffer"))?,
            );
        }

        let recv_res = self.socket.try_io(|| unsafe {
            use std::{os::fd::AsRawFd, ptr};

            assert_eq!(iovecs.len(), buffers.len());
            assert_eq!(hdrs.len(), iovecs.len());
            assert_eq!(hdrs.len(), names.len());
            assert_eq!(hdrs.len() * self.num_ctrl, ctrls.len());

            for (buffer, iovec) in Iterator::zip(buffers.iter_mut(), iovecs.iter_mut()) {
                iovec.iov_base = buffer.as_mut_slice().as_mut_ptr() as *mut _;
                iovec.iov_len = buffer.len();
            }

            // hdrs are already pre-initialized correctly but reset the flags just in case
            for hdr in hdrs.iter_mut() {
                hdr.msg_len = 0;
                hdr.msg_hdr.msg_flags = 0;
            }

            loop {
                let n_msgs = libc::recvmmsg(
                    self.socket.as_raw_fd(),
                    hdrs.as_mut_ptr(),
                    hdrs.len() as u32,
                    0,
                    ptr::null_mut(),
                );

                if n_msgs == -1 {
                    let err = io::Error::last_os_error();
                    match err.kind() {
                        io::ErrorKind::Interrupted => continue,
                        io::ErrorKind::HostUnreachable | io::ErrorKind::ConnectionReset => {
                            // ICMP error
                            // this can happen when the port is reused and shared with a UDP sender
                            self.buffers_cache.extend(buffers.drain(..));
                            gst::warning!(CAT, obj = self.element, "Read error: {err}");
                            continue;
                        }
                        io::ErrorKind::WouldBlock => (),
                        _ => gst::error!(CAT, obj = self.element, "Read error: {err}"),
                    }

                    self.buffers_cache.extend(buffers.drain(..));
                    return Err(err);
                } else if n_msgs == 0 {
                    continue;
                }

                return Ok(n_msgs as usize);
            }
        });

        let n_msgs = match recv_res {
            Ok(n_msg) => n_msg,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                return Ok(None);
            }
            Err(err) => {
                return Err(err).context("Receiving packets");
            }
        };

        gst::trace!(CAT, obj = self.element, "Read {n_msgs} messages");

        let now = clock.time();
        let real_time = unsafe {
            use std::mem;

            let mut timespec = mem::MaybeUninit::uninit();
            libc::clock_gettime(libc::CLOCK_REALTIME, timespec.as_mut_ptr());
            timespec.assume_init()
        };
        let real_time_ns = gst::ClockTime::from_seconds(real_time.tv_sec as u64)
            + gst::ClockTime::from_nseconds(real_time.tv_nsec as u64);
        let running_time = now.saturating_sub(base_time);
        let mut previous_receive_time = None;

        'next_packet: for (hdr, name, buffer) in
            izip!(hdrs.iter(), names.iter(), buffers.drain(..)).take(n_msgs)
        {
            if hdr.msg_hdr.msg_flags & libc::MSG_TRUNC != 0 {
                gst::warning!(CAT, obj = self.element, "Message truncated");
                self.buffers_cache.push_back(buffer);
                continue;
            }
            assert!(hdr.msg_hdr.msg_flags & libc::MSG_CTRUNC == 0);

            let read = hdr.msg_len as usize;
            let mut stride = read;
            let mut receive_time = None;

            unsafe {
                use std::ptr;

                let mut cmsg = libc::CMSG_FIRSTHDR(&hdr.msg_hdr);
                while !cmsg.is_null() {
                    match ((*cmsg).cmsg_level, (*cmsg).cmsg_type) {
                        #[cfg(any(target_os = "android", target_os = "linux"))]
                        (libc::IPPROTO_UDP, libc::UDP_GRO) => {
                            stride = ptr::read(libc::CMSG_DATA(cmsg) as *const i32) as usize;
                        }
                        #[cfg(any(
                            target_os = "android",
                            target_os = "linux",
                            target_os = "netbsd"
                        ))]
                        (libc::IPPROTO_IP, libc::IP_PKTINFO) => {
                            let pkt_info = &*(libc::CMSG_DATA(cmsg) as *const libc::in_pktinfo);
                            let dst_addr = Ipv4Addr::from(pkt_info.ipi_addr.s_addr.to_ne_bytes());
                            if address.as_ref().is_some_and(|addr| *addr != dst_addr) {
                                self.buffers_cache.push_back(buffer);
                                continue 'next_packet;
                            }
                        }
                        #[cfg(any(target_os = "openbsd", target_os = "freebsd"))]
                        (libc::IPPROTO_IP, libc::IP_RECVDSTADDR) => {
                            let dst_addr = &*(libc::CMSG_DATA(cmsg) as *const libc::in_addr);
                            let dst_addr = Ipv4Addr::from(addr.s_addr.to_ne_bytes());
                            if address.as_ref().is_some_and(|addr| *addr != dst_addr) {
                                continue 'next_packet;
                            }
                        }
                        #[cfg(any(
                            target_os = "android",
                            target_os = "linux",
                            target_os = "netbsd",
                            target_os = "openbsd",
                            target_os = "freebsd",
                        ))]
                        (libc::IPPROTO_IPV6, libc::IPV6_PKTINFO) => {
                            let pkt_info = &*(libc::CMSG_DATA(cmsg) as *const libc::in6_pktinfo);
                            let dst_addr = Ipv6Addr::from_octets(pkt_info.ipi6_addr.s6_addr);
                            if address.as_ref().is_some_and(|addr| *addr != dst_addr) {
                                self.buffers_cache.push_back(buffer);
                                continue 'next_packet;
                            }
                        }
                        #[cfg(any(target_os = "android", target_os = "linux"))]
                        (libc::SOL_SOCKET, libc::SO_TIMESTAMPNS) => {
                            let timespec = &*(libc::CMSG_DATA(cmsg) as *const libc::timespec);
                            receive_time = Some(*timespec);
                        }
                        (level, type_) => {
                            gst::warning!(
                                CAT,
                                obj = self.element,
                                "Unknown control message {level}:{type_}"
                            );
                        }
                    }

                    cmsg = libc::CMSG_NXTHDR(&hdr.msg_hdr, cmsg);
                }
            }

            let addr = unsafe {
                match name.ss_family as i32 {
                    libc::AF_INET => {
                        let addr: &libc::sockaddr_in =
                            &*(name as *const libc::sockaddr_storage as *const libc::sockaddr_in);
                        SocketAddr::V4(SocketAddrV4::new(
                            Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()),
                            u16::from_be(addr.sin_port),
                        ))
                    }
                    libc::AF_INET6 => {
                        let addr: &libc::sockaddr_in6 =
                            &*(name as *const libc::sockaddr_storage as *const libc::sockaddr_in6);
                        SocketAddr::V6(SocketAddrV6::new(
                            Ipv6Addr::from(addr.sin6_addr.s6_addr),
                            u16::from_be(addr.sin6_port),
                            addr.sin6_flowinfo,
                            addr.sin6_scope_id,
                        ))
                    }
                    _ => unreachable!("name.ss_family == {}", name.ss_family),
                }
            };

            if Self::should_filter_packet(&self.source_filter, self.source_filter_exclusive, &addr)
            {
                gst::trace!(CAT, obj = self.element, "Filtering packet from {addr}");
                self.buffers_cache.push_back(buffer);
                continue;
            }

            if stride == 0 {
                self.buffers_cache.push_back(buffer);
                continue;
            }

            let num_packets = read.div_ceil(stride);
            if num_packets == 0 {
                self.buffers_cache.push_back(buffer);
                continue;
            }

            let running_time = if let Some(receive_time) = receive_time {
                previous_receive_time = Some(receive_time);

                let receive_time_ns = gst::ClockTime::from_seconds(receive_time.tv_sec as u64)
                    + gst::ClockTime::from_nseconds(receive_time.tv_nsec as u64);

                if receive_time_ns < real_time_ns {
                    let diff = real_time_ns - receive_time_ns;
                    if running_time < diff {
                        // Discard old packets from before running time 0
                        self.buffers_cache.push_back(buffer);
                        continue;
                    }
                    running_time - diff
                } else {
                    let diff = receive_time_ns - real_time_ns;
                    running_time + diff
                }
            } else if let Some(receive_time) = previous_receive_time {
                let receive_time_ns = gst::ClockTime::from_seconds(receive_time.tv_sec as u64)
                    + gst::ClockTime::from_nseconds(receive_time.tv_nsec as u64);

                if receive_time_ns < real_time_ns {
                    let diff = real_time_ns - receive_time_ns;
                    running_time.saturating_sub(diff)
                } else {
                    let diff = receive_time_ns - real_time_ns;
                    running_time + diff
                }
            } else {
                running_time
            };

            gst::trace!(
                CAT,
                obj = self.element,
                "Read {num_packets} packets (bytes {read}, stride {stride}) from {addr} for running time {running_time}",
            );

            if num_packets == 1 || !self.preserve_packetization {
                if self.skip_first_bytes as usize >= read {
                    self.buffers_cache.push_back(buffer);
                    continue;
                }

                let mut buffer = buffer.into_buffer();
                let buffer_ref = buffer.get_mut().unwrap();

                buffer_ref.set_dts(running_time);
                buffer_ref
                    .resize(self.skip_first_bytes as usize..read)
                    .unwrap();

                let mut meta = buffer_ref.meta_mut::<gst_net::NetAddressMeta>().unwrap();
                let addr = self
                    .sender_address_lru
                    .get_or_insert(addr, || gio::InetSocketAddress::from(addr).upcast());
                meta.set_addr(addr.clone());

                match res {
                    None => {
                        res = Some(BufferOrList::Buffer(buffer));
                    }
                    Some(BufferOrList::Buffer(first_buffer)) => {
                        let mut list = gst::BufferList::new_sized(n_msgs);
                        let list_ref = list.get_mut().unwrap();
                        list_ref.add(first_buffer);
                        list_ref.add(buffer);
                        res = Some(BufferOrList::List(list));
                    }
                    Some(BufferOrList::List(ref mut list)) => {
                        list.get_mut().unwrap().add(buffer);
                    }
                }
            } else {
                use std::cmp;

                let buffer = buffer.into_buffer();

                for idx in 0..num_packets {
                    let start = idx * stride + self.skip_first_bytes as usize;
                    let end = cmp::min((idx + 1) * stride, read);

                    if start >= end {
                        break;
                    }

                    let mut sub_buffer = buffer
                        .copy_region(gst::BufferCopyFlags::MEMORY, start..end)
                        .unwrap();

                    let sub_buffer_ref = sub_buffer.get_mut().unwrap();
                    sub_buffer_ref.set_dts(running_time);

                    let mut meta = sub_buffer_ref
                        .meta_mut::<gst_net::NetAddressMeta>()
                        .unwrap();
                    let addr = self
                        .sender_address_lru
                        .get_or_insert(addr, || gio::InetSocketAddress::from(addr).upcast());
                    meta.set_addr(addr.clone());

                    match res {
                        None => {
                            res = Some(BufferOrList::Buffer(sub_buffer));
                        }
                        Some(BufferOrList::Buffer(first_buffer)) => {
                            let mut list = gst::BufferList::new_sized(n_msgs * num_packets);
                            let list_ref = list.get_mut().unwrap();
                            list_ref.add(first_buffer);
                            list_ref.add(sub_buffer);
                            res = Some(BufferOrList::List(list));
                        }
                        Some(BufferOrList::List(ref mut list)) => {
                            list.get_mut().unwrap().add(sub_buffer);
                        }
                    }
                }
            }
        }

        match res {
            Some(res) => {
                gst::trace!(
                    CAT,
                    obj = self.element,
                    "Outputting buffer list with {} buffers",
                    res.len()
                );

                Ok(Some(res))
            }
            None => {
                gst::trace!(CAT, obj = self.element, "No packets available currently");

                Ok(None)
            }
        }
    }

    #[cfg(not(any(
        windows,
        target_os = "android",
        target_os = "linux",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "freebsd",
    )))]
    pub fn recv_msg(&mut self) -> Result<Option<BufferOrList>, anyhow::Error> {
        use anyhow::Context as _;

        let Some(clock) = self.element.clock() else {
            gst::error!(CAT, obj = self.element, "Need a clock");
            anyhow::bail!("Need a clock!");
        };
        let Some(base_time) = self.element.base_time() else {
            gst::error!(CAT, obj = self.element, "Need a base time");
            anyhow::bail!("Need a base time!");
        };

        let address = self.multicast_joined.as_ref().map(|(addr, _)| *addr);

        let mut res = None;
        loop {
            let Some(mut buffer) = self.buffers_cache.pop_back().or_else(|| {
                self.buffer_pool
                    .acquire_buffer(None)
                    .ok()
                    .map(|b| b.into_mapped_buffer_writable().unwrap())
            }) else {
                anyhow::bail!("Failed to allocate buffer");
            };

            let recv_res = self.socket.try_io(|| unsafe {
                use std::{
                    mem,
                    net::{SocketAddrV4, SocketAddrV6},
                    os::fd::AsRawFd,
                };

                'next_packet: loop {
                    let mut name = mem::MaybeUninit::<libc::sockaddr_storage>::uninit();
                    let mut iovec = libc::iovec {
                        iov_base: buffer.as_mut_slice().as_mut_ptr() as *mut _,
                        iov_len: buffer.len(),
                    };
                    let mut hdr = mem::zeroed::<libc::msghdr>();

                    hdr.msg_iov = &mut iovec;
                    hdr.msg_iovlen = 1;

                    hdr.msg_name = name.as_mut_ptr() as *mut _;
                    hdr.msg_namelen = mem::size_of_val(&name) as u32;

                    hdr.msg_control = self.recvmsg_cache.as_mut_ptr() as *mut _;
                    hdr.msg_controllen = (self.num_ctrl * mem::size_of::<Ctrl>()) as _;

                    hdr.msg_flags = 0;

                    let bytes_read = libc::recvmsg(self.socket.as_raw_fd(), &mut hdr, 0);

                    if bytes_read == -1 {
                        return Err(io::Error::last_os_error());
                    } else if hdr.msg_flags & libc::MSG_TRUNC != 0 {
                        gst::warning!(CAT, obj = self.element, "Packet truncated");
                        // Just try again...
                        continue 'next_packet;
                    } else {
                        assert!(hdr.msg_flags & libc::MSG_CTRUNC == 0);

                        let mut cmsg = libc::CMSG_FIRSTHDR(&hdr);
                        while !cmsg.is_null() {
                            match ((*cmsg).cmsg_level, (*cmsg).cmsg_type) {
                                #[cfg(any(
                                    target_os = "solaris",
                                    target_os = "illumos",
                                    target_os = "nto",
                                    target_os = "cygwin",
                                    target_os = "macos",
                                    target_os = "ios",
                                    target_os = "tvos",
                                    target_os = "watchos",
                                    target_os = "visionos"
                                ))]
                                (libc::IPPROTO_IP, libc::IP_PKTINFO) => {
                                    let pkt_info =
                                        &*(libc::CMSG_DATA(cmsg) as *const libc::in_pktinfo);
                                    let dst_addr =
                                        Ipv4Addr::from(pkt_info.ipi_addr.s_addr.to_ne_bytes());
                                    if address.as_ref().is_some_and(|addr| *addr != dst_addr) {
                                        continue 'next_packet;
                                    }
                                }
                                #[cfg(any(target_os = "hurd", target_os = "dragonfly"))]
                                (libc::IPPROTO_IP, libc::IP_RECVDSTADDR) => {
                                    let dst_addr =
                                        &*(libc::CMSG_DATA(cmsg) as *const libc::in_addr);
                                    let dst_addr = Ipv4Addr::from(addr.s_addr.to_ne_bytes());
                                    if address.as_ref().is_some_and(|addr| *addr != dst_addr) {
                                        continue 'next_packet;
                                    }
                                }
                                #[cfg(any(
                                    target_os = "solaris",
                                    target_os = "illumos",
                                    target_os = "nto",
                                    target_os = "macos",
                                    target_os = "ios",
                                    target_os = "tvos",
                                    target_os = "watchos",
                                    target_os = "visionos",
                                    target_os = "hurd",
                                    target_os = "dragonfly",
                                ))]
                                (libc::IPPROTO_IPV6, libc::IPV6_PKTINFO) => {
                                    let pkt_info =
                                        &*(libc::CMSG_DATA(cmsg) as *const libc::in6_pktinfo);
                                    let dst_addr =
                                        Ipv6Addr::from_octets(pkt_info.ipi6_addr.s6_addr);
                                    if address.as_ref().is_some_and(|addr| *addr != dst_addr) {
                                        continue 'next_packet;
                                    }
                                }
                                (level, type_) => {
                                    gst::warning!(
                                        CAT,
                                        obj = self.element,
                                        "Unknown control message {level}:{type_}"
                                    );
                                }
                            }

                            cmsg = libc::CMSG_NXTHDR(&hdr, cmsg);
                        }

                        let name = name.assume_init();

                        let addr = match name.ss_family as i32 {
                            libc::AF_INET => {
                                let addr: &libc::sockaddr_in = &*(&name
                                    as *const libc::sockaddr_storage
                                    as *const libc::sockaddr_in);
                                SocketAddr::V4(SocketAddrV4::new(
                                    Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()),
                                    u16::from_be(addr.sin_port),
                                ))
                            }
                            libc::AF_INET6 => {
                                let addr: &libc::sockaddr_in6 = &*(&name
                                    as *const libc::sockaddr_storage
                                    as *const libc::sockaddr_in6);
                                SocketAddr::V6(SocketAddrV6::new(
                                    Ipv6Addr::from(addr.sin6_addr.s6_addr),
                                    u16::from_be(addr.sin6_port),
                                    addr.sin6_flowinfo,
                                    addr.sin6_scope_id,
                                ))
                            }
                            _ => unreachable!("name.ss_family == {}", name.ss_family),
                        };

                        if Self::should_filter_packet(
                            &self.source_filter,
                            self.source_filter_exclusive,
                            &addr,
                        ) {
                            gst::trace!(CAT, obj = self.element, "Filtering packet from {addr}");
                            continue 'next_packet;
                        }

                        return Ok((bytes_read as usize, addr));
                    }
                }
            });

            match recv_res {
                Ok((read, addr)) => {
                    let now = clock.time();
                    let running_time = now.saturating_sub(base_time);
                    gst::trace!(
                        CAT,
                        obj = self.element,
                        "Read {read} bytes from {addr} for running time {running_time}",
                    );

                    if self.skip_first_bytes as usize >= read {
                        self.buffers_cache.push_back(buffer);
                        continue;
                    }

                    let mut buffer = buffer.into_buffer();
                    let buffer_ref = buffer.get_mut().unwrap();

                    buffer_ref.set_dts(running_time);
                    buffer_ref
                        .resize(self.skip_first_bytes as usize..read)
                        .unwrap();

                    let mut meta = buffer_ref.meta_mut::<gst_net::NetAddressMeta>().unwrap();
                    let addr = self
                        .sender_address_lru
                        .get_or_insert(addr, || gio::InetSocketAddress::from(addr).upcast());
                    meta.set_addr(addr.clone());

                    match res {
                        None => {
                            res = Some(BufferOrList::Buffer(buffer));
                            if self.batch_size == 1 {
                                break;
                            }
                        }
                        Some(BufferOrList::Buffer(first_buffer)) => {
                            let mut list = gst::BufferList::new_sized(self.batch_size as usize);
                            let list_ref = list.get_mut().unwrap();
                            list_ref.add(first_buffer);
                            list_ref.add(buffer);
                            res = Some(BufferOrList::List(list));

                            if self.batch_size == 2 {
                                break;
                            }
                        }
                        Some(BufferOrList::List(ref mut list)) => {
                            list.get_mut().unwrap().add(buffer);
                            if self.batch_size as usize == list.len() {
                                break;
                            }
                        }
                    }

                    continue;
                }
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                    gst::debug!(CAT, obj = self.element, "Socket closed");
                    self.buffers_cache.push_back(buffer);
                    return Err(err).context("Receiving packet");
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    self.buffers_cache.push_back(buffer);
                    continue;
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::HostUnreachable | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    // ICMP error
                    // this can happen when the port is reused and shared with a UDP sender
                    gst::warning!(CAT, obj = self.element, "Read error: {err}");
                    self.buffers_cache.push_back(buffer);
                    continue;
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.buffers_cache.push_back(buffer);
                    break;
                }
                Err(err) => {
                    gst::error!(CAT, obj = self.element, "Read error: {err}");
                    self.buffers_cache.push_back(buffer);
                    return Err(err).context("Receiving packet");
                }
            }
        }

        match res {
            Some(res) => {
                gst::trace!(
                    CAT,
                    obj = self.element,
                    "Outputting buffer list with {} buffers",
                    res.len()
                );

                Ok(Some(res))
            }
            None => {
                gst::trace!(CAT, obj = self.element, "No packets available currently");

                Ok(None)
            }
        }
    }

    #[cfg(windows)]
    pub fn recv_from(&mut self) -> Result<Option<BufferOrList>, anyhow::Error> {
        use anyhow::Context as _;

        let Some(clock) = self.element.clock() else {
            gst::error!(CAT, obj = self.element, "Need a clock");
            anyhow::bail!("Need a clock!");
        };
        let Some(base_time) = self.element.base_time() else {
            gst::error!(CAT, obj = self.element, "Need a base time");
            anyhow::bail!("Need a base time!");
        };

        let mut res = None;
        loop {
            let Some(mut buffer) = self.buffers_cache.pop_back().or_else(|| {
                self.buffer_pool
                    .acquire_buffer(None)
                    .ok()
                    .map(|b| b.into_mapped_buffer_writable().unwrap())
            }) else {
                anyhow::bail!("Failed to allocate buffer");
            };

            match self.socket.recv_from(&mut buffer) {
                Ok((read, addr)) => {
                    if Self::should_filter_packet(
                        &self.source_filter,
                        self.source_filter_exclusive,
                        &addr,
                    ) {
                        gst::trace!(CAT, obj = self.element, "Filtering packet from {addr}");
                        self.buffers_cache.push_back(buffer);
                        continue;
                    }
                    let now = clock.time();
                    let running_time = now.saturating_sub(base_time);
                    gst::trace!(
                        CAT,
                        obj = self.element,
                        "Read {read} bytes from {addr} for running time {running_time}",
                    );

                    if self.skip_first_bytes as usize >= read {
                        self.buffers_cache.push_back(buffer);
                        continue;
                    }

                    let mut buffer = buffer.into_buffer();
                    let buffer_ref = buffer.get_mut().unwrap();

                    buffer_ref.set_dts(running_time);
                    buffer_ref
                        .resize(self.skip_first_bytes as usize..read)
                        .unwrap();

                    let mut meta = buffer_ref.meta_mut::<gst_net::NetAddressMeta>().unwrap();
                    let addr = self
                        .sender_address_lru
                        .get_or_insert(addr, || gio::InetSocketAddress::from(addr).upcast());
                    meta.set_addr(addr.clone());

                    match res {
                        None => {
                            res = Some(BufferOrList::Buffer(buffer));
                            if self.batch_size == 1 {
                                break;
                            }
                        }
                        Some(BufferOrList::Buffer(first_buffer)) => {
                            let mut list = gst::BufferList::new_sized(self.batch_size as usize);
                            let list_ref = list.get_mut().unwrap();
                            list_ref.add(first_buffer);
                            list_ref.add(buffer);
                            res = Some(BufferOrList::List(list));

                            if self.batch_size == 2 {
                                break;
                            }
                        }
                        Some(BufferOrList::List(ref mut list)) => {
                            list.get_mut().unwrap().add(buffer);
                            if self.batch_size as usize == list.len() {
                                break;
                            }
                        }
                    }

                    continue;
                }
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                    gst::debug!(CAT, obj = self.element, "Socket closed");
                    return Err(err).context("Receiving packet");
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    self.buffers_cache.push_back(buffer);
                    continue;
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::HostUnreachable | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    // ICMP error (ConnectionReset matches CONNECTION_CLOSED)
                    // this can happen when the port is reused and shared with a UDP sender
                    gst::warning!(CAT, obj = self.element, "Read error: {err}");
                    self.buffers_cache.push_back(buffer);
                    continue;
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.buffers_cache.push_back(buffer);
                    break;
                }
                Err(err) if err.raw_os_error() == Some(10040) => {
                    // WSAEMSGSIZE
                    gst::warning!(CAT, obj = self.element, "Packet truncated");
                    self.buffers_cache.push_back(buffer);
                    continue;
                }
                Err(err) => {
                    gst::error!(CAT, obj = self.element, "Read error: {err}");
                    return Err(err).context("Receiving packet");
                }
            }
        }

        match res {
            Some(res) => {
                gst::trace!(
                    CAT,
                    obj = self.element,
                    "Outputting buffer list with {} buffers",
                    res.len()
                );

                Ok(Some(res))
            }
            None => {
                gst::trace!(CAT, obj = self.element, "No packets available currently");

                Ok(None)
            }
        }
    }
}

pub enum BufferOrList {
    Buffer(gst::Buffer),
    List(gst::BufferList),
}

impl BufferOrList {
    fn len(&self) -> usize {
        match self {
            BufferOrList::Buffer(_) => 1,
            BufferOrList::List(buffer_list) => buffer_list.len(),
        }
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        self.leave_multicast();
    }
}

impl mio::event::Source for UdpSocket {
    fn register(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        self.socket.socket.register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &mio::Registry,
        token: mio::Token,
        interests: mio::Interest,
    ) -> io::Result<()> {
        self.socket.socket.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &mio::Registry) -> io::Result<()> {
        self.socket.socket.deregister(registry)
    }
}

#[cfg(not(windows))]
#[repr(C)]
#[derive(Clone, Copy)]
union Ctrl {
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "solaris",
        target_os = "illumos",
        target_os = "nto",
        target_os = "cygwin",
        target_os = "netbsd",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))]
    _buf_in_pktinfo:
        [u8; unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::in_pktinfo>() as u32) } as usize],
    #[cfg(any(
        target_os = "hurd",
        target_os = "openbsd",
        target_os = "freebsd",
        target_os = "dragonfly",
    ))]
    _buf_in_addr:
        [u8; unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::in_addr>() as u32) } as usize],
    #[cfg(any(
        target_os = "android",
        target_os = "linux",
        target_os = "solaris",
        target_os = "illumos",
        target_os = "nto",
        target_os = "netbsd",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "hurd",
        target_os = "openbsd",
        target_os = "freebsd",
        target_os = "dragonfly",
    ))]
    _buf_in6_pktinfo:
        [u8; unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::in6_pktinfo>() as u32) } as usize],
    #[cfg(any(target_os = "android", target_os = "linux"))]
    _buf_gro: [u8; unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize],
    #[cfg(any(target_os = "android", target_os = "linux"))]
    _buf_timespec:
        [u8; unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::timespec>() as u32) } as usize],
    _align: libc::cmsghdr,
}

/// Send/Sync struct for passing around a gio::Socket
/// and getting the fd from it
///
/// gio::Socket is not Send/Sync as it's generally unsafe
/// to access it from multiple threads. Getting the underlying raw
/// fd is safe though, as is receiving/sending from two different threads
#[derive(Debug, Clone)]
pub struct GioSocketWrapper {
    socket: gio::Socket,
    external_socket: bool,
}

unsafe impl Send for GioSocketWrapper {}
unsafe impl Sync for GioSocketWrapper {}

impl GioSocketWrapper {
    pub fn new(socket: gio::Socket, external_socket: bool) -> Self {
        Self {
            socket,
            external_socket,
        }
    }

    #[cfg(unix)]
    unsafe fn from_raw(
        socket: &mem::ManuallyDrop<impl std::os::fd::AsRawFd>,
    ) -> Result<Self, anyhow::Error> {
        use anyhow::Context as _;

        let socket = unsafe {
            gio::Socket::from_raw_fd(socket.as_raw_fd())
                .context("Failed to wrap GSocket around fd")?
        };
        Ok(Self {
            socket,
            external_socket: false,
        })
    }

    #[cfg(windows)]
    unsafe fn from_raw(
        socket: &mem::ManuallyDrop<impl std::os::windows::io::AsRawSocket>,
    ) -> Result<Self, anyhow::Error> {
        use anyhow::Context as _;

        let socket = unsafe {
            struct Wrapper(std::os::windows::io::RawSocket);
            impl std::os::windows::io::IntoRawSocket for Wrapper {
                fn into_raw_socket(self) -> std::os::windows::io::RawSocket {
                    self.0
                }
            }

            gio::Socket::from_raw_socket(Wrapper(socket.as_raw_socket()))
                .context("Failed to wrap GSocket around fd")?
        };

        Ok(Self {
            socket,
            external_socket: false,
        })
    }

    pub fn as_socket(&self) -> &gio::Socket {
        &self.socket
    }

    pub fn is_external(&self) -> bool {
        self.external_socket
    }
}

struct SocketAndWrappedSocket {
    socket: mem::ManuallyDrop<mio::net::UdpSocket>,
    wrapped_socket: GioSocketWrapper,
}

impl SocketAndWrappedSocket {
    fn from_socket2(socket: socket2::Socket) -> Result<Self, anyhow::Error> {
        let socket = mem::ManuallyDrop::new(mio::net::UdpSocket::from_std(socket.into()));
        let wrapped_socket = unsafe { GioSocketWrapper::from_raw(&socket)? };

        Ok(Self {
            socket,
            wrapped_socket,
        })
    }

    fn from_wrapped_socket(wrapped_socket: &GioSocketWrapper) -> Result<Self, anyhow::Error> {
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd, FromRawFd};

            use anyhow::Context as _;
            use gio::prelude::*;

            let fd = wrapped_socket.socket.fd();
            let socket = socket2::SockRef::from(&fd);

            socket
                .set_nonblocking(true)
                .context("Failed to set socket non-blocking")?;

            let socket =
                unsafe { mem::ManuallyDrop::new(mio::net::UdpSocket::from_raw_fd(fd.as_raw_fd())) };

            Ok(Self {
                socket,
                wrapped_socket: wrapped_socket.clone(),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawSocket, FromRawSocket};

            use anyhow::Context as _;
            use gio::prelude::*;

            let fd = wrapped_socket.socket.socket();
            let socket = socket2::SockRef::from(&fd);

            socket
                .set_nonblocking(true)
                .context("Failed to set socket non-blocking")?;

            let socket = unsafe {
                mem::ManuallyDrop::new(mio::net::UdpSocket::from_raw_socket(fd.as_raw_socket()))
            };

            Ok(Self {
                socket,
                wrapped_socket: wrapped_socket.clone(),
            })
        }
    }
}

impl std::ops::Deref for SocketAndWrappedSocket {
    type Target = mio::net::UdpSocket;

    fn deref(&self) -> &mio::net::UdpSocket {
        &self.socket
    }
}

impl Drop for SocketAndWrappedSocket {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            use std::os::fd::IntoRawFd;

            let socket = mem::ManuallyDrop::take(&mut self.socket);
            // Release all resources except for the actual fd. The fd
            // is closed by the gio::Socket.
            let _raw_fd = socket.into_raw_fd();
        }
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::IntoRawSocket;

            let socket = mem::ManuallyDrop::take(&mut self.socket);
            // Release all resources except for the actual fd. The fd
            // is closed by the gio::Socket.
            let _raw_fd = socket.into_raw_socket();
        }
    }
}

mod buffer_pool {
    use super::*;

    mod imp {
        use super::*;

        use gst::subclass::prelude::*;

        #[derive(Default)]
        pub struct BufferPool {}

        impl ObjectImpl for BufferPool {}

        impl GstObjectImpl for BufferPool {}
        impl BufferPoolImpl for BufferPool {
            fn alloc_buffer(
                &self,
                params: Option<&gst::BufferPoolAcquireParams>,
            ) -> Result<gst::Buffer, gst::FlowError> {
                static ANY_ADDRESS: LazyLock<gio::SocketAddress> = LazyLock::new(|| {
                    gio::InetSocketAddress::new(
                        &gio::InetAddress::new_any(gio::SocketFamily::Ipv4),
                        0,
                    )
                    .upcast()
                });

                let mut buffer = self.parent_alloc_buffer(params)?;

                let buffer_ref = buffer.get_mut().unwrap();
                gst_net::NetAddressMeta::add(buffer_ref, &*ANY_ADDRESS);

                Ok(buffer)
            }
        }

        #[glib::object_subclass]
        impl ObjectSubclass for BufferPool {
            const NAME: &'static str = "GstUdpSrc2BufferPool";
            type Type = super::BufferPool;
            type ParentType = gst::BufferPool;
        }
    }

    glib::wrapper! {
        pub struct BufferPool(ObjectSubclass<imp::BufferPool>) @extends gst::BufferPool, gst::Object;
    }

    impl BufferPool {
        pub fn new() -> Self {
            glib::Object::builder().build()
        }
    }
}
