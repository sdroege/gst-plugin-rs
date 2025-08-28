// GStreamer
//
// Copyright (C) 2015-2023 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//FIXME: Remove this when https://github.com/mmastrac/getifaddrs/issues/5 is fixed in the `getifaddrs` crate
#[cfg(target_os = "android")]
pub mod getifaddrs {
    use bitflags::bitflags;

    bitflags! {
        /// Flags representing the status and capabilities of a network interface.
        ///
        /// These flags provide information about the current state and features of a network interface.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct InterfaceFlags: u32 {
            /// The interface is up and running.
            const UP = 0x1;
            /// The interface is in a running state.
            const RUNNING = 0x2;
            /// The interface supports broadcast.
            const BROADCAST = 0x4;
            /// The interface is a loopback interface.
            const LOOPBACK = 0x8;
            /// The interface is a point-to-point link.
            const POINTTOPOINT = 0x10;
            /// The interface supports multicast.
            const MULTICAST = 0x20;
        }
    }

    /// Represents a network interface.
    ///
    /// This struct contains information about a network interface, including its name,
    /// IP address, netmask, flags, and index.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Interface {
        /// The name of the interface.
        pub name: String,
        /// The description of the interface (Windows-specific).
        #[cfg(windows)]
        pub description: String,
        /// The IP address associated with the interface.
        pub address: std::net::IpAddr,
        // TODO: This may be implementable for Windows.
        #[cfg(not(windows))]
        /// The associated address of the interface. For broadcast interfaces, this
        /// is the broadcast address. For point-to-point interfaces, this is the
        /// peer address.
        pub associated_address: Option<std::net::IpAddr>,
        /// The netmask of the interface, if available.
        pub netmask: Option<std::net::IpAddr>,
        /// The flags indicating the interface's properties and state.
        pub flags: InterfaceFlags,
        /// The index of the interface, if available.
        pub index: Option<u32>,
    }
}

use getifaddrs::Interface;

#[cfg(unix)]
pub mod imp {
    use super::*;

    use std::{io, mem, net::UdpSocket, os::unix::io::AsRawFd};

    use libc::{
        in_addr, ip_mreqn, setsockopt, IPPROTO_IP, IP_ADD_MEMBERSHIP, IP_DROP_MEMBERSHIP,
        IP_MULTICAST_IF,
    };

    #[cfg(target_os = "macos")]
    use libc::ip_mreq;

    #[cfg(any(target_os = "solaris", target_os = "illumos", target_os = "macos"))]
    use std::net::Ipv4Addr;

    /// Join multicast address for a given interface.
    pub fn join_multicast_v4(
        socket: &UdpSocket,
        addr: &Ipv4Addr,
        iface: &Interface,
    ) -> Result<(), io::Error> {
        multicast_group_operation_v4(socket, addr, iface, true)
    }

    /// Leave multicast address for a given interface.
    pub fn leave_multicast_v4(
        socket: &UdpSocket,
        addr: &Ipv4Addr,
        iface: &Interface,
    ) -> Result<(), io::Error> {
        multicast_group_operation_v4(socket, addr, iface, false)
    }

    fn multicast_group_operation_v4(
        socket: &UdpSocket,
        addr: &Ipv4Addr,
        iface: &Interface,
        join: bool,
    ) -> Result<(), io::Error> {
        let index = iface.index.unwrap_or(0);

        #[cfg(not(any(target_os = "solaris", target_os = "illumos", target_os = "macos")))]
        {
            let group_op: i32 = if join {
                IP_ADD_MEMBERSHIP
            } else {
                IP_DROP_MEMBERSHIP
            };

            let mreqn = ip_mreqn {
                imr_multiaddr: in_addr {
                    s_addr: u32::from_ne_bytes(addr.octets()),
                },
                imr_address: in_addr {
                    s_addr: u32::from_ne_bytes(Ipv4Addr::UNSPECIFIED.octets()),
                },
                imr_ifindex: index as _,
            };

            // SAFETY: Requires a valid ip_mreq or ip_mreqn struct to be passed together
            // with its size for checking which of the two it is. On errors a negative
            // integer is returned.
            unsafe {
                if setsockopt(
                    socket.as_raw_fd(),
                    IPPROTO_IP,
                    group_op,
                    &mreqn as *const _ as *const _,
                    mem::size_of_val(&mreqn) as _,
                ) < 0
                {
                    return Err(io::Error::last_os_error());
                }
            }

            #[cfg(not(any(target_os = "openbsd", target_os = "dragonfly", target_os = "netbsd")))]
            {
                let mreqn = ip_mreqn {
                    imr_multiaddr: in_addr {
                        s_addr: u32::from_ne_bytes(Ipv4Addr::UNSPECIFIED.octets()),
                    },
                    imr_address: in_addr {
                        s_addr: u32::from_ne_bytes(Ipv4Addr::UNSPECIFIED.octets()),
                    },
                    imr_ifindex: index as _,
                };

                // SAFETY: Requires a valid ip_mreq or ip_mreqn struct to be passed together
                // with its size for checking which of the two it is. On errors a negative
                // integer is returned.
                unsafe {
                    if setsockopt(
                        socket.as_raw_fd(),
                        IPPROTO_IP,
                        IP_MULTICAST_IF,
                        &mreqn as *const _ as *const _,
                        mem::size_of_val(&mreqn) as _,
                    ) < 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
            }
            #[cfg(any(target_os = "openbsd", target_os = "dragonfly"))]
            {
                let addr = in_addr {
                    s_addr: u32::from_ne_bytes(ip_addr.octets()),
                };

                // SAFETY: Requires a valid in_addr struct to be passed together with its size for
                // checking which of the two it is. On errors a negative integer is returned.
                unsafe {
                    if setsockopt(
                        socket.as_raw_fd(),
                        IPPROTO_IP,
                        IP_MULTICAST_IF,
                        &addr as *const _ as *const _,
                        mem::size_of_val(&addr) as _,
                    ) < 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
            }
            #[cfg(target_os = "netbsd")]
            {
                let idx = (index as u32).to_be();

                // SAFETY: Requires a valid in_addr struct or interface index in network byte order
                // to be passed together with its size for checking which of the two it is. On
                // errors a negative integer is returned.
                unsafe {
                    if setsockopt(
                        socket.as_raw_fd(),
                        IPPROTO_IP,
                        IP_MULTICAST_IF,
                        &idx as *const _ as *const _,
                        mem::size_of_val(&idx) as _,
                    ) < 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
            }

            Ok(())
        }

        #[cfg(any(target_os = "solaris", target_os = "illumos"))]
        {
            let ip_addr = match iface.address {
                IpAddr::V4(ipv4_addr) => ipv4_addr,
                IpAddr::V6(_) => return Err(io::Error::other("Interface address is IPv6")),
            };

            if join {
                socket.join_multicast_v4(addr, &ip_addr).with_context(|| {
                    format!(
                        "Failed joining multicast group for interface {} at address {}",
                        iface.name, ip_addr
                    )
                })?;
            } else {
                socket.leave_multicast_v4(addr, &ip_addr).with_context(|| {
                    format!(
                        "Failed leave multicast group for interface {} at address {}",
                        iface.name, ip_addr
                    )
                })?;
            }

            // SAFETY: Requires a valid in_addr struct to be passed together with its size for
            // checking which of the two it is. On errors a negative integer is returned.
            unsafe {
                if setsockopt(
                    socket.as_raw_fd(),
                    IPPROTO_IP,
                    IP_MULTICAST_IF,
                    &addr as *const _ as *const _,
                    mem::size_of_val(&addr) as _,
                ) < 0
                {
                    return Err(io::Error::last_os_error());
                }
            }

            Ok(())
        }

        #[cfg(target_os = "macos")]
        {
            use getifaddrs::Address;

            let ip_addr = match &iface.address {
                Address::V4(ifaddr) => ifaddr.address,
                Address::V6(_) => return Err(io::Error::other("Interface address is IPv6")),
                Address::Mac(_) => return Err(io::Error::other("Interface address is Mac")),
            };

            let mreq = ip_mreq {
                imr_multiaddr: in_addr {
                    s_addr: u32::from_ne_bytes(addr.octets()),
                },
                imr_interface: in_addr {
                    s_addr: u32::from_ne_bytes(ip_addr.octets()),
                },
            };

            let mreqn = ip_mreqn {
                imr_multiaddr: in_addr {
                    s_addr: u32::from_ne_bytes(Ipv4Addr::UNSPECIFIED.octets()),
                },
                imr_address: in_addr {
                    s_addr: u32::from_ne_bytes(Ipv4Addr::UNSPECIFIED.octets()),
                },
                imr_ifindex: index as _,
            };

            let group_op: i32 = if join {
                IP_ADD_MEMBERSHIP
            } else {
                IP_DROP_MEMBERSHIP
            };

            // SAFETY: Requires a valid ip_mreq struct to be passed together with its size for checking
            // validity. On errors a negative integer is returned.
            unsafe {
                if setsockopt(
                    socket.as_raw_fd(),
                    IPPROTO_IP,
                    group_op,
                    &mreq as *const _ as *const _,
                    mem::size_of_val(&mreq) as _,
                ) < 0
                {
                    return Err(io::Error::last_os_error());
                }
            }

            // SAFETY: Requires a valid ip_mreqn struct to be passed together
            // with its size for checking which of the two it is. On errors a negative
            // integer is returned.
            unsafe {
                if setsockopt(
                    socket.as_raw_fd(),
                    IPPROTO_IP,
                    IP_MULTICAST_IF,
                    &mreqn as *const _ as *const _,
                    mem::size_of_val(&mreqn) as _,
                ) < 0
                {
                    return Err(io::Error::last_os_error());
                }
            }

            Ok(())
        }
    }
}

#[cfg(windows)]
pub mod imp {
    use super::*;

    use std::{
        io, mem,
        net::{Ipv4Addr, UdpSocket},
        os::windows::io::AsRawSocket,
    };

    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, WSAGetLastError, IN_ADDR, IN_ADDR_0, IPPROTO_IP, IP_ADD_MEMBERSHIP,
        IP_DROP_MEMBERSHIP, IP_MREQ, IP_MULTICAST_IF,
    };

    /// Join multicast address for a given interface.
    pub fn join_multicast_v4(
        socket: &UdpSocket,
        addr: &Ipv4Addr,
        iface: &Interface,
    ) -> Result<(), io::Error> {
        multicast_group_operation_v4(socket, addr, iface, IP_ADD_MEMBERSHIP)
        // let ip_addr = Ipv4Addr::new(0, 0, 0, iface.index.unwrap() as u8);
        // socket.join_multicast_v4(addr, &ip_addr).unwrap();
        // return Ok(());
    }

    /// Leave multicast address for a given interface.
    pub fn leave_multicast_v4(
        socket: &UdpSocket,
        addr: &Ipv4Addr,
        iface: &Interface,
    ) -> Result<(), io::Error> {
        multicast_group_operation_v4(socket, addr, iface, IP_DROP_MEMBERSHIP)
        // let ip_addr = Ipv4Addr::new(0, 0, 0, iface.index.unwrap() as u8);
        // socket.leave_multicast_v4(addr, &ip_addr).unwrap();
        // return Ok(());
    }

    fn multicast_group_operation_v4(
        socket: &UdpSocket,
        addr: &Ipv4Addr,
        iface: &Interface,
        group_op: i32,
    ) -> Result<(), io::Error> {
        let index = iface.index.unwrap_or(0);

        let mreq = IP_MREQ {
            imr_multiaddr: IN_ADDR {
                S_un: IN_ADDR_0 {
                    S_addr: u32::from_ne_bytes(addr.octets()),
                },
            },
            imr_interface: IN_ADDR {
                S_un: IN_ADDR_0 {
                    S_addr: u32::from_ne_bytes(Ipv4Addr::new(0, 0, 0, index as u8).octets()),
                },
            },
        };

        // SAFETY: Requires a valid ip_mreq struct to be passed together with its size for checking
        // validity. On errors a negative integer is returned.
        unsafe {
            if setsockopt(
                socket.as_raw_socket() as usize,
                IPPROTO_IP,
                group_op,
                &mreq as *const _ as *const _,
                mem::size_of_val(&mreq) as _,
            ) < 0
            {
                return Err(io::Error::from_raw_os_error(WSAGetLastError()));
            }
        }

        let ip_addr = IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes(Ipv4Addr::new(0, 0, 0, index as u8).octets()),
            },
        };

        // SAFETY: Requires a valid IN_ADDR struct to be passed together with its size for checking
        // which of the two it is. On errors a negative integer is returned.
        unsafe {
            if setsockopt(
                socket.as_raw_socket() as usize,
                IPPROTO_IP,
                IP_MULTICAST_IF,
                &ip_addr as *const _ as *const _,
                mem::size_of_val(&ip_addr) as _,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
        }

        Ok(())
    }
}
