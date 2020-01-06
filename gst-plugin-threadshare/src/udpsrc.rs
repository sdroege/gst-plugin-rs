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

use either::Either;

use futures::future::BoxFuture;
use futures::lock::Mutex;
use futures::prelude::*;

use gio;
use gio_sys as gio_ffi;

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use glib::{glib_object_impl, glib_object_subclass};

use gobject_sys as gobject_ffi;

use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_element_error, gst_error, gst_error_msg, gst_log, gst_trace};
use gst_net::*;

use lazy_static::lazy_static;

use rand;

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{self, Arc};
use std::u16;

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket, RawSocket};

use crate::runtime::prelude::*;
use crate::runtime::{self, Context, JoinHandle, PadSrc, PadSrcRef};

use super::socket::{Socket, SocketRead, SocketStream};

const DEFAULT_ADDRESS: Option<&str> = Some("127.0.0.1");
const DEFAULT_PORT: u32 = 5000;
const DEFAULT_REUSE: bool = true;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_MTU: u32 = 1500;
const DEFAULT_SOCKET: Option<GioSocketWrapper> = None;
const DEFAULT_USED_SOCKET: Option<GioSocketWrapper> = None;
const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;
const DEFAULT_RETRIEVE_SENDER_ADDRESS: bool = true;

// Send/Sync struct for passing around a gio::Socket
// and getting the raw fd from it
//
// gio::Socket is not Send/Sync as it's generally unsafe
// to access it from multiple threads. Getting the underlying raw
// fd is safe though, as is receiving/sending from two different threads
#[derive(Debug)]
struct GioSocketWrapper {
    socket: *mut gio_ffi::GSocket,
}

unsafe impl Send for GioSocketWrapper {}
unsafe impl Sync for GioSocketWrapper {}

impl GioSocketWrapper {
    fn new(socket: &gio::Socket) -> Self {
        use glib::translate::*;

        Self {
            socket: socket.to_glib_full(),
        }
    }

    fn as_socket(&self) -> gio::Socket {
        unsafe {
            use glib::translate::*;

            from_glib_none(self.socket)
        }
    }

    #[cfg(unix)]
    fn get<T: FromRawFd>(&self) -> T {
        unsafe { FromRawFd::from_raw_fd(libc::dup(gio_ffi::g_socket_get_fd(self.socket))) }
    }

    #[cfg(windows)]
    fn get<T: FromRawSocket>(&self) -> T {
        unsafe {
            FromRawSocket::from_raw_socket(
                dup_socket(gio_ffi::g_socket_get_fd(self.socket) as _) as _
            )
        }
    }
}

impl Clone for GioSocketWrapper {
    fn clone(&self) -> Self {
        Self {
            socket: unsafe { gobject_ffi::g_object_ref(self.socket as *mut _) as *mut _ },
        }
    }
}

impl Drop for GioSocketWrapper {
    fn drop(&mut self) {
        unsafe {
            gobject_ffi::g_object_unref(self.socket as *mut _);
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

#[derive(Debug, Clone)]
struct Settings {
    address: Option<String>,
    port: u32,
    reuse: bool,
    caps: Option<gst::Caps>,
    mtu: u32,
    socket: Option<GioSocketWrapper>,
    used_socket: Option<GioSocketWrapper>,
    context: String,
    context_wait: u32,
    retrieve_sender_address: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            address: DEFAULT_ADDRESS.map(Into::into),
            port: DEFAULT_PORT,
            reuse: DEFAULT_REUSE,
            caps: DEFAULT_CAPS,
            mtu: DEFAULT_MTU,
            socket: DEFAULT_SOCKET,
            used_socket: DEFAULT_USED_SOCKET,
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            retrieve_sender_address: DEFAULT_RETRIEVE_SENDER_ADDRESS,
        }
    }
}

static PROPERTIES: [subclass::Property; 10] = [
    subclass::Property("address", |name| {
        glib::ParamSpec::string(
            name,
            "Address",
            "Address/multicast group to listen on",
            DEFAULT_ADDRESS,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("port", |name| {
        glib::ParamSpec::uint(
            name,
            "Port",
            "Port to listen on",
            0,
            u16::MAX as u32,
            DEFAULT_PORT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("reuse", |name| {
        glib::ParamSpec::boolean(
            name,
            "Reuse",
            "Allow reuse of the port",
            DEFAULT_REUSE,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("caps", |name| {
        glib::ParamSpec::boxed(
            name,
            "Caps",
            "Caps to use",
            gst::Caps::static_type(),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("mtu", |name| {
        glib::ParamSpec::uint(
            name,
            "MTU",
            "MTU",
            0,
            u16::MAX as u32,
            DEFAULT_MTU,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("socket", |name| {
        glib::ParamSpec::object(
            name,
            "Socket",
            "Socket to use for UDP reception. (None == allocate)",
            gio::Socket::static_type(),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("used-socket", |name| {
        glib::ParamSpec::object(
            name,
            "Used Socket",
            "Socket currently in use for UDP reception. (None = no socket)",
            gio::Socket::static_type(),
            glib::ParamFlags::READABLE,
        )
    }),
    subclass::Property("context", |name| {
        glib::ParamSpec::string(
            name,
            "Context",
            "Context name to share threads with",
            Some(DEFAULT_CONTEXT),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("context-wait", |name| {
        glib::ParamSpec::uint(
            name,
            "Context Wait",
            "Throttle poll loop to run at most once every this many ms",
            0,
            1000,
            DEFAULT_CONTEXT_WAIT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("retrieve-sender-address", |name| {
        glib::ParamSpec::boolean(
            name,
            "Retrieve sender address",
            "Whether to retrieve the sender address and add it to buffers as meta. Disabling this might result in minor performance improvements in certain scenarios",
            DEFAULT_REUSE,
            glib::ParamFlags::READWRITE,
        )
    }),
];

#[derive(Debug)]
struct UdpReaderInner {
    socket: tokio::net::UdpSocket,
}

#[derive(Debug)]
pub struct UdpReader(Arc<Mutex<UdpReaderInner>>);

impl UdpReader {
    fn new(socket: tokio::net::UdpSocket) -> Self {
        UdpReader(Arc::new(Mutex::new(UdpReaderInner { socket })))
    }
}

impl SocketRead for UdpReader {
    const DO_TIMESTAMP: bool = true;

    fn read<'buf>(
        &self,
        buffer: &'buf mut [u8],
    ) -> BoxFuture<'buf, io::Result<(usize, Option<std::net::SocketAddr>)>> {
        let this = Arc::clone(&self.0);

        async move {
            this.lock()
                .await
                .socket
                .recv_from(buffer)
                .await
                .map(|(read_size, saddr)| (read_size, Some(saddr)))
        }
        .boxed()
    }
}

#[derive(Debug)]
struct UdpSrcPadHandlerState {
    retrieve_sender_address: bool,
    need_initial_events: bool,
    caps: Option<gst::Caps>,
    configured_caps: Option<gst::Caps>,
}

impl Default for UdpSrcPadHandlerState {
    fn default() -> Self {
        UdpSrcPadHandlerState {
            retrieve_sender_address: true,
            need_initial_events: true,
            caps: None,
            configured_caps: None,
        }
    }
}

#[derive(Debug, Default)]
struct UdpSrcPadHandlerInner {
    state: sync::RwLock<UdpSrcPadHandlerState>,
    socket_stream: Mutex<Option<SocketStream<UdpReader>>>,
    flush_join_handle: sync::Mutex<Option<JoinHandle<Result<(), ()>>>>,
}

#[derive(Clone, Debug, Default)]
struct UdpSrcPadHandler(Arc<UdpSrcPadHandlerInner>);

impl UdpSrcPadHandler {
    async fn start_task(&self, pad: PadSrcRef<'_>, element: &gst::Element) {
        let this = self.clone();
        let pad_weak = pad.downgrade();
        let element = element.clone();
        pad.start_task(move || {
            let this = this.clone();
            let pad_weak = pad_weak.clone();
            let element = element.clone();
            async move {
                let item = this
                    .0
                    .socket_stream
                    .lock()
                    .await
                    .as_mut()
                    .expect("Missing SocketStream")
                    .next()
                    .await;

                let pad = pad_weak.upgrade().expect("PadSrc no longer exists");
                let (mut buffer, saddr) = match item {
                    Some(Ok((buffer, saddr))) => (buffer, saddr),
                    Some(Err(err)) => {
                        gst_error!(CAT, obj: &element, "Got error {}", err);
                        match err {
                            Either::Left(gst::FlowError::CustomError) => (),
                            Either::Left(err) => {
                                gst_element_error!(
                                    element,
                                    gst::StreamError::Failed,
                                    ("Internal data stream error"),
                                    ["streaming stopped, reason {}", err]
                                );
                            }
                            Either::Right(err) => {
                                gst_element_error!(
                                    element,
                                    gst::StreamError::Failed,
                                    ("I/O error"),
                                    ["streaming stopped, I/O error {}", err]
                                );
                            }
                        }
                        return;
                    }
                    None => {
                        gst_log!(CAT, obj: pad.gst_pad(), "SocketStream Stopped");
                        pad.pause_task().await;
                        return;
                    }
                };

                if let Some(saddr) = saddr {
                    if this.0.state.read().unwrap().retrieve_sender_address {
                        let inet_addr = match saddr.ip() {
                            IpAddr::V4(ip) => gio::InetAddress::new_from_bytes(
                                gio::InetAddressBytes::V4(&ip.octets()),
                            ),
                            IpAddr::V6(ip) => gio::InetAddress::new_from_bytes(
                                gio::InetAddressBytes::V6(&ip.octets()),
                            ),
                        };
                        let inet_socket_addr =
                            &gio::InetSocketAddress::new(&inet_addr, saddr.port());
                        NetAddressMeta::add(buffer.get_mut().unwrap(), inet_socket_addr);
                    }
                }

                this.push_buffer(pad, &element, buffer).await;
            }
        })
        .await;
    }

    async fn push_buffer(&self, pad: PadSrcRef<'_>, element: &gst::Element, buffer: gst::Buffer) {
        {
            let mut events = Vec::new();
            {
                // Only `read` the state in the hot path
                if self.0.state.read().unwrap().need_initial_events {
                    // We will need to `write` and we also want to prevent
                    // any changes on the state while we are handling initial events
                    let mut state = self.0.state.write().unwrap();
                    assert!(state.need_initial_events);

                    gst_debug!(CAT, obj: pad.gst_pad(), "Pushing initial events");

                    let stream_id =
                        format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
                    events.push(
                        gst::Event::new_stream_start(&stream_id)
                            .group_id(gst::util_group_id_next())
                            .build(),
                    );

                    if let Some(ref caps) = state.caps {
                        events.push(gst::Event::new_caps(&caps).build());
                        state.configured_caps = Some(caps.clone());
                    }
                    events.push(
                        gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new())
                            .build(),
                    );

                    state.need_initial_events = false;
                }
            }

            for event in events {
                pad.push_event(event).await;
            }
        }

        match pad.push(buffer).await {
            Ok(_) => {
                gst_log!(CAT, obj: pad.gst_pad(), "Successfully pushed buffer");
            }
            Err(gst::FlowError::Flushing) => {
                gst_debug!(CAT, obj: pad.gst_pad(), "Flushing");
                pad.pause_task().await;
            }
            Err(gst::FlowError::Eos) => {
                gst_debug!(CAT, obj: pad.gst_pad(), "EOS");
                pad.pause_task().await;
            }
            Err(err) => {
                gst_error!(CAT, obj: pad.gst_pad(), "Got error {}", err);
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["streaming stopped, reason {}", err]
                );
            }
        }
    }
}

impl PadSrcHandler for UdpSrcPadHandler {
    type ElementImpl = UdpSrc;

    fn src_event(
        &self,
        pad: &PadSrcRef,
        _udpsrc: &UdpSrc,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                let mut flush_join_handle = self.0.flush_join_handle.lock().unwrap();
                if flush_join_handle.is_none() {
                    let element = element.clone();
                    let pad_weak = pad.downgrade();

                    *flush_join_handle = Some(pad.spawn(async move {
                        let res = UdpSrc::from_instance(&element).pause(&element).await;
                        let pad = pad_weak.upgrade().unwrap();
                        if res.is_ok() {
                            gst_debug!(CAT, obj: pad.gst_pad(), "FlushStart complete");
                        } else {
                            gst_debug!(CAT, obj: pad.gst_pad(), "FlushStart failed");
                        }

                        res
                    }));
                } else {
                    gst_debug!(CAT, obj: pad.gst_pad(), "FlushStart ignored: previous Flush in progress");
                }

                true
            }
            EventView::FlushStop(..) => {
                let element = element.clone();
                let inner_weak = Arc::downgrade(&self.0);
                let pad_weak = pad.downgrade();

                let fut = async move {
                    let mut ret = false;

                    let pad = pad_weak.upgrade().unwrap();
                    let inner_weak = inner_weak.upgrade().unwrap();
                    let flush_join_handle = inner_weak.flush_join_handle.lock().unwrap().take();
                    if let Some(flush_join_handle) = flush_join_handle {
                        if let Ok(Ok(())) = flush_join_handle.await {
                            ret = UdpSrc::from_instance(&element)
                                .start(&element)
                                .await
                                .is_ok();
                            gst_debug!(CAT, obj: pad.gst_pad(), "FlushStop complete");
                        } else {
                            gst_debug!(CAT, obj: pad.gst_pad(), "FlushStop aborted: FlushStart failed");
                        }
                    } else {
                        gst_debug!(CAT, obj: pad.gst_pad(), "FlushStop ignored: no Flush in progress");
                    }

                    ret
                }
                .boxed();

                return Either::Right(fut);
            }
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst_log!(CAT, obj: pad.gst_pad(), "Handled {:?}", event);
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Didn't handle {:?}", event);
        }

        Either::Left(ret)
    }

    fn src_query(
        &self,
        pad: &PadSrcRef,
        _udpsrc: &UdpSrc,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", query);

        let ret = match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                q.set(true, 0.into(), 0.into());
                true
            }
            QueryView::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryView::Caps(ref mut q) => {
                let state = self.0.state.read().unwrap();
                let caps = if let Some(ref caps) = state.configured_caps {
                    q.get_filter()
                        .map(|f| f.intersect_with_mode(caps, gst::CapsIntersectMode::First))
                        .unwrap_or_else(|| caps.clone())
                } else {
                    q.get_filter()
                        .map(|f| f.to_owned())
                        .unwrap_or_else(gst::Caps::new_any)
                };

                q.set_result(&caps);

                true
            }
            _ => false,
        };

        if ret {
            gst_log!(CAT, obj: pad.gst_pad(), "Handled {:?}", query);
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Didn't handle {:?}", query);
        }

        ret
    }
}

#[derive(Debug)]
struct State {
    socket: Option<Socket<UdpReader>>,
}

impl Default for State {
    fn default() -> State {
        State { socket: None }
    }
}

#[derive(Debug)]
struct PreparationSet {
    join_handle: JoinHandle<Result<(), gst::ErrorMessage>>,
    context: Context,
}

#[derive(Debug)]
struct UdpSrc {
    src_pad: PadSrc,
    src_pad_handler: UdpSrcPadHandler,
    state: Mutex<State>,
    settings: Mutex<Settings>,
    preparation_set: Mutex<Option<PreparationSet>>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-udpsrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing UDP source"),
    );
}

impl UdpSrc {
    async fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let _state = self.state.lock().await;
        gst_debug!(CAT, obj: element, "Preparing");

        let context = {
            let settings = self.settings.lock().await.clone();

            {
                let mut src_pad_handler_state = self.src_pad_handler.0.state.write().unwrap();
                src_pad_handler_state.retrieve_sender_address = settings.retrieve_sender_address;
                src_pad_handler_state.caps = settings.caps.clone();
            }

            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?
        };

        // UdpSocket needs to be instantiated in the thread of its I/O reactor
        *self.preparation_set.lock().await = Some(PreparationSet {
            join_handle: context.spawn(Self::prepare_socket(element.clone())),
            context,
        });

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    async fn prepare_socket(element: gst::Element) -> Result<(), gst::ErrorMessage> {
        let this = Self::from_instance(&element);

        let mut settings = this.settings.lock().await.clone();
        gst_debug!(CAT, obj: &element, "Preparing Socket");

        let socket = if let Some(ref wrapped_socket) = settings.socket {
            use std::net::UdpSocket;

            let socket: UdpSocket;

            #[cfg(unix)]
            {
                socket = wrapped_socket.get()
            }
            #[cfg(windows)]
            {
                socket = wrapped_socket.get()
            }

            let socket = tokio::net::UdpSocket::from_std(socket).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to setup socket for tokio: {}", err]
                )
            })?;

            settings.used_socket = Some(wrapped_socket.clone());

            socket
        } else {
            let addr: IpAddr = match settings.address {
                None => {
                    return Err(gst_error_msg!(
                        gst::ResourceError::Settings,
                        ["No address set"]
                    ));
                }
                Some(ref addr) => match addr.parse() {
                    Err(err) => {
                        return Err(gst_error_msg!(
                            gst::ResourceError::Settings,
                            ["Invalid address '{}' set: {}", addr, err]
                        ));
                    }
                    Ok(addr) => addr,
                },
            };
            let port = settings.port;

            // TODO: TTL, multicast loopback, etc
            let saddr = if addr.is_multicast() {
                let bind_addr = if addr.is_ipv4() {
                    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
                } else {
                    IpAddr::V6(Ipv6Addr::UNSPECIFIED)
                };

                let saddr = SocketAddr::new(bind_addr, port as u16);
                gst_debug!(
                    CAT,
                    obj: &element,
                    "Binding to {:?} for multicast group {:?}",
                    saddr,
                    addr
                );

                saddr
            } else {
                let saddr = SocketAddr::new(addr, port as u16);
                gst_debug!(CAT, obj: &element, "Binding to {:?}", saddr);

                saddr
            };

            let builder = if addr.is_ipv4() {
                net2::UdpBuilder::new_v4()
            } else {
                net2::UdpBuilder::new_v6()
            }
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to create socket: {}", err]
                )
            })?;

            builder.reuse_address(settings.reuse).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to set reuse_address: {}", err]
                )
            })?;

            #[cfg(unix)]
            {
                use net2::unix::UnixUdpBuilderExt;

                builder.reuse_port(settings.reuse).map_err(|err| {
                    gst_error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to set reuse_port: {}", err]
                    )
                })?;
            }

            let socket = builder.bind(&saddr).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to bind socket: {}", err]
                )
            })?;

            let socket = tokio::net::UdpSocket::from_std(socket).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to setup socket for tokio: {}", err]
                )
            })?;

            if addr.is_multicast() {
                // TODO: Multicast interface configuration, going to be tricky
                match addr {
                    IpAddr::V4(addr) => {
                        socket
                            .join_multicast_v4(addr, Ipv4Addr::new(0, 0, 0, 0))
                            .map_err(|err| {
                                gst_error_msg!(
                                    gst::ResourceError::OpenRead,
                                    ["Failed to join multicast group: {}", err]
                                )
                            })?;
                    }
                    IpAddr::V6(addr) => {
                        socket.join_multicast_v6(&addr, 0).map_err(|err| {
                            gst_error_msg!(
                                gst::ResourceError::OpenRead,
                                ["Failed to join multicast group: {}", err]
                            )
                        })?;
                    }
                }
            }

            // Store the socket as used-socket in the settings
            #[cfg(unix)]
            unsafe {
                let fd = libc::dup(socket.as_raw_fd());

                // This is unsafe because it allows us to share the fd between the socket and the
                // GIO socket below, but safety of this is the job of the application
                struct FdConverter(RawFd);
                impl IntoRawFd for FdConverter {
                    fn into_raw_fd(self) -> RawFd {
                        self.0
                    }
                }

                let fd = FdConverter(fd);

                let gio_socket = gio::Socket::new_from_fd(fd).map_err(|err| {
                    gst_error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to create wrapped GIO socket: {}", err]
                    )
                })?;
                let wrapper = GioSocketWrapper::new(&gio_socket);
                settings.used_socket = Some(wrapper);
            }
            #[cfg(windows)]
            unsafe {
                // FIXME: Needs https://github.com/tokio-rs/tokio/pull/806
                // and https://github.com/carllerche/mio/pull/859
                let fd = unreachable!(); //dup_socket(socket.as_raw_socket() as _) as _;

                // This is unsafe because it allows us to share the fd between the socket and the
                // GIO socket below, but safety of this is the job of the application
                struct SocketConverter(RawSocket);
                impl IntoRawSocket for SocketConverter {
                    fn into_raw_socket(self) -> RawSocket {
                        self.0
                    }
                }

                let fd = SocketConverter(fd);

                let gio_socket = gio::Socket::new_from_socket(fd).map_err(|err| {
                    gst_error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to create wrapped GIO socket: {}", err]
                    )
                })?;
                let wrapper = GioSocketWrapper::new(&gio_socket);
                settings.used_socket = Some(wrapper);
            }

            socket
        };

        let buffer_pool = gst::BufferPool::new();
        let mut config = buffer_pool.get_config();
        config.set_params(None, settings.mtu, 0, 0);
        buffer_pool.set_config(config).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["Failed to configure buffer pool {:?}", err]
            )
        })?;

        let socket = Socket::new(element.upcast_ref(), UdpReader::new(socket), buffer_pool);
        let socket_stream = socket.prepare().await.map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to prepare socket {:?}", err]
            )
        })?;

        *this.src_pad_handler.0.socket_stream.lock().await = Some(socket_stream);

        this.state.lock().await.socket = Some(socket);

        gst_debug!(CAT, obj: &element, "Socket Prepared");
        drop(settings);

        element.notify("used-socket");

        Ok(())
    }

    async fn complete_preparation(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Completing preparation");

        let PreparationSet {
            join_handle,
            context,
        } = self
            .preparation_set
            .lock()
            .await
            .take()
            .expect("preparation_set already taken");

        join_handle
            .await
            .expect("The socket preparation has panicked")?;

        self.src_pad
            .prepare(context, &self.src_pad_handler)
            .await
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing src_pads: {:?}", err]
                )
            })?;

        gst_debug!(CAT, obj: element, "Preparation completed");

        Ok(())
    }

    async fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().await;
        gst_debug!(CAT, obj: element, "Unpreparing");

        self.settings.lock().await.used_socket = None;

        self.src_pad.stop_task().await;

        {
            let socket = state.socket.take().unwrap();
            socket.unprepare().await.unwrap();
        }

        let _ = self.src_pad.unprepare().await;
        self.src_pad_handler
            .0
            .state
            .write()
            .unwrap()
            .configured_caps = None;

        gst_debug!(CAT, obj: element, "Unprepared");
        Ok(())
    }

    async fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let state = self.state.lock().await;
        gst_debug!(CAT, obj: element, "Starting");

        if let Some(ref socket) = state.socket {
            socket
                .start(element.get_clock(), Some(element.get_base_time()))
                .await;
        }

        self.src_pad_handler
            .start_task(self.src_pad.as_ref(), element)
            .await;

        gst_debug!(CAT, obj: element, "Started");

        Ok(())
    }

    async fn pause(&self, element: &gst::Element) -> Result<(), ()> {
        let pause_completion = {
            let state = self.state.lock().await;
            gst_debug!(CAT, obj: element, "Pausing");

            let pause_completion = self.src_pad.pause_task().await;
            state.socket.as_ref().unwrap().pause().await;

            pause_completion
        };

        gst_debug!(CAT, obj: element, "Waiting for Task Pause to complete");
        pause_completion.await;

        gst_debug!(CAT, obj: element, "Paused");

        Ok(())
    }
}

impl ObjectSubclass for UdpSrc {
    const NAME: &'static str = "RsTsUdpSrc";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing UDP source",
            "Source/Network",
            "Receives data over the network via UDP",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_any();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        #[cfg(not(windows))]
        {
            klass.install_properties(&PROPERTIES);
        }
        #[cfg(windows)]
        {
            let properties = PROPERTIES
                .iter()
                .filter(|p| match *p {
                    subclass::Property("socket", ..) | subclass::Property("used-socket", ..) => {
                        false
                    }
                    _ => true,
                })
                .collect::<Vec<_>>();
            klass.install_properties(properties.as_slice());
        }
    }

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("src").unwrap();
        let src_pad = PadSrc::new_from_template(&templ, Some("src"));

        Self {
            src_pad,
            src_pad_handler: UdpSrcPadHandler::default(),
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
            preparation_set: Mutex::new(None),
        }
    }
}

impl ObjectImpl for UdpSrc {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        let mut settings = runtime::executor::block_on(self.settings.lock());
        match *prop {
            subclass::Property("address", ..) => {
                settings.address = value.get().expect("type checked upstream");
            }
            subclass::Property("port", ..) => {
                settings.port = value.get_some().expect("type checked upstream");
            }
            subclass::Property("reuse", ..) => {
                settings.reuse = value.get_some().expect("type checked upstream");
            }
            subclass::Property("caps", ..) => {
                settings.caps = value.get().expect("type checked upstream");
            }
            subclass::Property("mtu", ..) => {
                settings.mtu = value.get_some().expect("type checked upstream");
            }
            subclass::Property("socket", ..) => {
                settings.socket = value
                    .get::<gio::Socket>()
                    .expect("type checked upstream")
                    .map(|socket| GioSocketWrapper::new(&socket));
            }
            subclass::Property("used-socket", ..) => {
                unreachable!();
            }
            subclass::Property("context", ..) => {
                settings.context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                settings.context_wait = value.get_some().expect("type checked upstream");
            }
            subclass::Property("retrieve-sender-address", ..) => {
                settings.retrieve_sender_address = value.get_some().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        let settings = runtime::executor::block_on(self.settings.lock());
        match *prop {
            subclass::Property("address", ..) => Ok(settings.address.to_value()),
            subclass::Property("port", ..) => Ok(settings.port.to_value()),
            subclass::Property("reuse", ..) => Ok(settings.reuse.to_value()),
            subclass::Property("caps", ..) => Ok(settings.caps.to_value()),
            subclass::Property("mtu", ..) => Ok(settings.mtu.to_value()),
            subclass::Property("socket", ..) => Ok(settings
                .socket
                .as_ref()
                .map(GioSocketWrapper::as_socket)
                .to_value()),
            subclass::Property("used-socket", ..) => Ok(settings
                .used_socket
                .as_ref()
                .map(GioSocketWrapper::as_socket)
                .to_value()),
            subclass::Property("context", ..) => Ok(settings.context.to_value()),
            subclass::Property("context-wait", ..) => Ok(settings.context_wait.to_value()),
            subclass::Property("retrieve-sender-address", ..) => {
                Ok(settings.retrieve_sender_address.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(self.src_pad.gst_pad()).unwrap();
        super::set_element_flags(element, gst::ElementFlags::SOURCE);
    }
}

impl ElementImpl for UdpSrc {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                runtime::executor::block_on(self.prepare(element)).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::ReadyToPaused => {
                runtime::executor::block_on(self.complete_preparation(element)).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                runtime::executor::block_on(self.pause(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                runtime::executor::block_on(self.unprepare(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => {
                runtime::executor::block_on(self.start(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PausedToReady => {
                self.src_pad_handler
                    .0
                    .state
                    .write()
                    .unwrap()
                    .need_initial_events = true;
            }
            _ => (),
        }

        Ok(success)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-udpsrc",
        gst::Rank::None,
        UdpSrc::get_type(),
    )
}
