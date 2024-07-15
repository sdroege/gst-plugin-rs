// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
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
//
// SPDX-License-Identifier: LGPL-2.1-or-later

use futures::future::BoxFuture;

use gst::glib;
use gst::prelude::*;

use std::sync::LazyLock;

use std::error;
use std::fmt;
use std::io;
use std::net::UdpSocket;

use crate::runtime::Async;

#[cfg(unix)]
use std::os::{
    fd::BorrowedFd,
    unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket, RawSocket};

static SOCKET_CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ts-socket",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing Socket"),
    )
});

pub trait SocketRead: Send + Unpin {
    const DO_TIMESTAMP: bool;

    fn read<'buf>(
        &'buf mut self,
        buffer: &'buf mut [u8],
    ) -> BoxFuture<'buf, io::Result<(usize, Option<std::net::SocketAddr>)>>;
}

pub struct Socket<T: SocketRead> {
    element: gst::Element,
    buffer_pool: gst::BufferPool,
    reader: T,
    mapped_buffer: Option<gst::MappedBuffer<gst::buffer::Writable>>,
    clock: Option<gst::Clock>,
    base_time: Option<gst::ClockTime>,
}

impl<T: SocketRead> Socket<T> {
    pub fn try_new(
        element: gst::Element,
        buffer_pool: gst::BufferPool,
        reader: T,
    ) -> Result<Self, glib::BoolError> {
        // FIXME couldn't we just delegate this to caller?
        buffer_pool.set_active(true).map_err(|err| {
            gst::error!(
                SOCKET_CAT,
                obj = element,
                "Failed to prepare socket: {}",
                err
            );

            err
        })?;

        Ok(Socket::<T> {
            buffer_pool,
            element,
            reader,
            mapped_buffer: None,
            clock: None,
            base_time: None,
        })
    }

    pub fn set_clock(&mut self, clock: Option<gst::Clock>, base_time: Option<gst::ClockTime>) {
        self.clock = clock;
        self.base_time = base_time;
    }

    pub fn get(&self) -> &T {
        &self.reader
    }
}

#[derive(Debug)]
pub enum SocketError {
    Gst(gst::FlowError),
    Io(io::Error),
}

impl error::Error for SocketError {}

impl fmt::Display for SocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SocketError::Gst(err) => write!(f, "flow error: {err}"),
            SocketError::Io(err) => write!(f, "IO error: {err}"),
        }
    }
}

impl<T: SocketRead> Socket<T> {
    // Can't implement this as a Stream trait because we end up using things like
    // tokio::net::UdpSocket which don't implement pollable functions.
    #[allow(clippy::should_implement_trait)]
    pub async fn try_next(
        &mut self,
    ) -> Result<(gst::Buffer, Option<std::net::SocketAddr>), SocketError> {
        gst::log!(SOCKET_CAT, obj = self.element, "Trying to read data");

        if self.mapped_buffer.is_none() {
            match self.buffer_pool.acquire_buffer(None) {
                Ok(buffer) => {
                    self.mapped_buffer = Some(buffer.into_mapped_buffer_writable().unwrap());
                }
                Err(err) => {
                    gst::debug!(
                        SOCKET_CAT,
                        obj = self.element,
                        "Failed to acquire buffer {:?}",
                        err
                    );
                    return Err(SocketError::Gst(err));
                }
            }
        }

        match self
            .reader
            .read(self.mapped_buffer.as_mut().unwrap().as_mut_slice())
            .await
        {
            Ok((len, saddr)) => {
                let dts = if T::DO_TIMESTAMP {
                    let time = self.clock.as_ref().unwrap().time();
                    let running_time = time.opt_checked_sub(self.base_time).ok().flatten();
                    // FIXME maybe we should check if running_time.is_none
                    // so as to display another message
                    gst::debug!(
                        SOCKET_CAT,
                        obj = self.element,
                        "Read {} bytes at {} (clock {})",
                        len,
                        running_time.display(),
                        time.display(),
                    );
                    running_time
                } else {
                    gst::debug!(SOCKET_CAT, obj = self.element, "Read {} bytes", len);
                    gst::ClockTime::NONE
                };

                let mut buffer = self.mapped_buffer.take().unwrap().into_buffer();
                {
                    let buffer = buffer.get_mut().unwrap();
                    if len < buffer.size() {
                        buffer.set_size(len);
                    }
                    buffer.set_dts(dts);
                }

                Ok((buffer, saddr))
            }
            Err(err) => {
                gst::debug!(SOCKET_CAT, obj = self.element, "Read error {:?}", err);

                Err(SocketError::Io(err))
            }
        }
    }
}

impl<T: SocketRead> Drop for Socket<T> {
    fn drop(&mut self) {
        if let Err(err) = self.buffer_pool.set_active(false) {
            gst::error!(
                SOCKET_CAT,
                obj = self.element,
                "Failed to unprepare socket: {}",
                err
            );
        }
    }
}

// Send/Sync struct for passing around a gio::Socket
// and getting the raw fd from it
//
// gio::Socket is not Send/Sync as it's generally unsafe
// to access it from multiple threads. Getting the underlying raw
// fd is safe though, as is receiving/sending from two different threads
#[derive(Debug)]
pub struct GioSocketWrapper {
    socket: *mut gio::ffi::GSocket,
}

unsafe impl Send for GioSocketWrapper {}
unsafe impl Sync for GioSocketWrapper {}

impl GioSocketWrapper {
    pub fn new(socket: &gio::Socket) -> Self {
        use glib::translate::*;

        Self {
            socket: socket.to_glib_full(),
        }
    }

    pub fn as_socket(&self) -> gio::Socket {
        unsafe {
            use glib::translate::*;

            from_glib_none(self.socket)
        }
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "linux",
        target_os = "android",
        target_os = "aix",
        target_os = "fuchsia",
        target_os = "haiku",
        target_env = "newlib"
    ))]
    pub fn set_tos(&self, qos_dscp: i32) -> rustix::io::Result<()> {
        use gio::prelude::*;
        use rustix::net::sockopt;

        let tos = (qos_dscp & 0x3f) << 2;

        let socket = self.as_socket();

        sockopt::set_ip_tos(
            unsafe { BorrowedFd::borrow_raw(socket.as_raw_fd()) },
            tos as u8,
        )?;

        if socket.family() == gio::SocketFamily::Ipv6 {
            sockopt::set_ipv6_tclass(
                unsafe { BorrowedFd::borrow_raw(socket.as_raw_fd()) },
                tos as u32,
            )?;
        }

        Ok(())
    }

    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "linux",
        target_os = "android",
        target_os = "aix",
        target_os = "fuchsia",
        target_os = "haiku",
        target_env = "newlib"
    )))]
    pub fn set_tos(&self, _qos_dscp: i32) -> rustix::io::Result<()> {
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn get<T: FromRawFd>(&self) -> T {
        unsafe {
            let borrowed =
                rustix::fd::BorrowedFd::borrow_raw(gio::ffi::g_socket_get_fd(self.socket));

            let dupped = rustix::io::dup(borrowed).unwrap();
            let res = FromRawFd::from_raw_fd(dupped.as_raw_fd());

            // We transferred ownership to T so don't drop dupped
            std::mem::forget(dupped);

            res
        }
    }

    #[cfg(windows)]
    pub fn get<T: FromRawSocket>(&self) -> T {
        unsafe {
            FromRawSocket::from_raw_socket(
                dup_socket(gio::ffi::g_socket_get_fd(self.socket) as _) as _
            )
        }
    }
}

impl Clone for GioSocketWrapper {
    fn clone(&self) -> Self {
        Self {
            socket: unsafe { glib::gobject_ffi::g_object_ref(self.socket as *mut _) as *mut _ },
        }
    }
}

impl Drop for GioSocketWrapper {
    fn drop(&mut self) {
        unsafe {
            glib::gobject_ffi::g_object_unref(self.socket as *mut _);
        }
    }
}

#[cfg(windows)]
unsafe fn dup_socket(socket: usize) -> usize {
    use std::mem;
    use winapi::shared::ws2def;
    use winapi::um::processthreadsapi;
    use winapi::um::winsock2;

    let mut proto_info = mem::MaybeUninit::uninit();
    let ret = winsock2::WSADuplicateSocketA(
        socket,
        processthreadsapi::GetCurrentProcessId(),
        proto_info.as_mut_ptr(),
    );
    assert_eq!(ret, 0);
    let mut proto_info = proto_info.assume_init();

    let socket = winsock2::WSASocketA(
        ws2def::AF_INET,
        ws2def::SOCK_DGRAM,
        ws2def::IPPROTO_UDP as i32,
        &mut proto_info,
        0,
        0,
    );

    assert_ne!(socket, winsock2::INVALID_SOCKET);

    socket
}

pub fn wrap_socket(socket: &Async<UdpSocket>) -> Result<GioSocketWrapper, gst::ErrorMessage> {
    #[cfg(unix)]
    unsafe {
        let dupped = rustix::io::dup(socket).unwrap();

        // This is unsafe because it allows us to share the fd between the socket and the
        // GIO socket below, but safety of this is the job of the application
        struct FdConverter(RawFd);
        impl IntoRawFd for FdConverter {
            fn into_raw_fd(self) -> RawFd {
                self.0
            }
        }

        let fd = FdConverter(dupped.as_raw_fd());

        let gio_socket = gio::Socket::from_fd(fd);
        // We transferred ownership to gio_socket so don't drop dupped
        std::mem::forget(dupped);
        let gio_socket = gio_socket.map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::OpenWrite,
                ["Failed to create wrapped GIO socket: {}", err]
            )
        })?;

        Ok(GioSocketWrapper::new(&gio_socket))
    }
    #[cfg(windows)]
    unsafe {
        let fd = socket.as_raw_socket();

        // This is unsafe because it allows us to share the fd between the socket and the
        // GIO socket below, but safety of this is the job of the application
        struct SocketConverter(RawSocket);
        impl IntoRawSocket for SocketConverter {
            fn into_raw_socket(self) -> RawSocket {
                self.0
            }
        }

        let fd = SocketConverter(fd);

        let gio_socket = gio::Socket::from_socket(fd).map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::OpenWrite,
                ["Failed to create wrapped GIO socket: {}", err]
            )
        })?;

        Ok(GioSocketWrapper::new(&gio_socket))
    }
}
