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

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;

use gio;

use gio_ffi;
use gobject_ffi;

use std::io;
use std::sync::Mutex;
use std::u16;

use futures;
use futures::future;
use futures::{Future, Poll};
use tokio::net;

use either::Either;

use rand;

use net2;

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket, RawSocket};

use iocontext::*;
use socket::*;

const DEFAULT_ADDRESS: Option<&'static str> = Some("127.0.0.1");
const DEFAULT_PORT: u32 = 5000;
const DEFAULT_REUSE: bool = true;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_MTU: u32 = 1500;
const DEFAULT_SOCKET: Option<GioSocketWrapper> = None;
const DEFAULT_USED_SOCKET: Option<GioSocketWrapper> = None;
const DEFAULT_CONTEXT: &'static str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;

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
        use libc;

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

    let mut proto_info = mem::zeroed();
    let ret = winsock2::WSADuplicateSocketA(
        socket,
        processthreadsapi::GetCurrentProcessId(),
        &mut proto_info,
    );
    assert_eq!(ret, 0);

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
        }
    }
}

static PROPERTIES: [subclass::Property; 9] = [
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
];

pub struct UdpReader {
    socket: net::UdpSocket,
}

impl UdpReader {
    fn new(socket: net::UdpSocket) -> Self {
        Self { socket }
    }
}

impl SocketRead for UdpReader {
    const DO_TIMESTAMP: bool = true;

    fn poll_read(&mut self, buf: &mut [u8]) -> Poll<usize, io::Error> {
        self.socket.poll_recv(buf)
    }
}

struct State {
    io_context: Option<IOContext>,
    pending_future_id: Option<PendingFutureId>,
    socket: Option<Socket<UdpReader>>,
    need_initial_events: bool,
    configured_caps: Option<gst::Caps>,
    pending_future_cancel: Option<futures::sync::oneshot::Sender<()>>,
}

impl Default for State {
    fn default() -> State {
        State {
            io_context: None,
            pending_future_id: None,
            socket: None,
            need_initial_events: true,
            configured_caps: None,
            pending_future_cancel: None,
        }
    }
}

struct UdpSrc {
    cat: gst::DebugCategory,
    src_pad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl UdpSrc {
    fn src_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                let _ = self.stop(element);
                true
            }
            EventView::FlushStop(..) => {
                let (res, state, pending) = element.get_state(0.into());
                if res == Ok(gst::StateChangeSuccess::Success) && state == gst::State::Playing
                    || res == Ok(gst::StateChangeSuccess::Async) && pending == gst::State::Playing
                {
                    let _ = self.start(element);
                }
                true
            }
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst_log!(self.cat, obj: pad, "Handled event {:?}", event);
        } else {
            gst_log!(self.cat, obj: pad, "Didn't handle event {:?}", event);
        }
        ret
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(self.cat, obj: pad, "Handling query {:?}", query);
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
                let state = self.state.lock().unwrap();
                let caps = if let Some(ref caps) = state.configured_caps {
                    q.get_filter()
                        .map(|f| f.intersect_with_mode(caps, gst::CapsIntersectMode::First))
                        .unwrap_or(caps.clone())
                } else {
                    q.get_filter()
                        .map(|f| f.to_owned())
                        .unwrap_or(gst::Caps::new_any())
                };

                q.set_result(&caps);

                true
            }
            _ => false,
        };

        if ret {
            gst_log!(self.cat, obj: pad, "Handled query {:?}", query);
        } else {
            gst_log!(self.cat, obj: pad, "Didn't handle query {:?}", query);
        }
        ret
    }

    fn create_io_context_event(state: &State) -> Option<gst::Event> {
        if let (&Some(ref pending_future_id), &Some(ref io_context)) =
            (&state.pending_future_id, &state.io_context)
        {
            let s = gst::Structure::new(
                "ts-io-context",
                &[
                    ("io-context", &io_context),
                    ("pending-future-id", &*pending_future_id),
                ],
            );
            Some(gst::Event::new_custom_downstream_sticky(s).build())
        } else {
            None
        }
    }

    fn push_buffer(
        &self,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> future::Either<
        Box<Future<Item = (), Error = gst::FlowError> + Send + 'static>,
        future::FutureResult<(), gst::FlowError>,
    > {
        let mut events = Vec::new();
        let mut state = self.state.lock().unwrap();
        if state.need_initial_events {
            gst_debug!(self.cat, obj: element, "Pushing initial events");

            let stream_id = format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
            events.push(gst::Event::new_stream_start(&stream_id).build());
            if let Some(ref caps) = self.settings.lock().unwrap().caps {
                events.push(gst::Event::new_caps(&caps).build());
                state.configured_caps = Some(caps.clone());
            }
            events.push(
                gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
            );

            if let Some(event) = Self::create_io_context_event(&state) {
                events.push(event);

                // Get rid of reconfigure flag
                self.src_pad.check_reconfigure();
            }
            state.need_initial_events = false;
        } else if self.src_pad.check_reconfigure() {
            if let Some(event) = Self::create_io_context_event(&state) {
                events.push(event);
            }
        }
        drop(state);

        for event in events {
            self.src_pad.push_event(event);
        }

        let res = match self.src_pad.push(buffer) {
            Ok(_) => {
                gst_log!(self.cat, obj: element, "Successfully pushed buffer");
                Ok(())
            }
            Err(gst::FlowError::Flushing) => {
                gst_debug!(self.cat, obj: element, "Flushing");
                let state = self.state.lock().unwrap();
                if let Some(ref socket) = state.socket {
                    socket.pause();
                }
                Ok(())
            }
            Err(gst::FlowError::Eos) => {
                gst_debug!(self.cat, obj: element, "EOS");
                let state = self.state.lock().unwrap();
                if let Some(ref socket) = state.socket {
                    socket.pause();
                }
                Ok(())
            }
            Err(err) => {
                gst_error!(self.cat, obj: element, "Got error {}", err);
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["streaming stopped, reason {}", err]
                );
                Err(gst::FlowError::CustomError)
            }
        };

        match res {
            Ok(()) => {
                let mut state = self.state.lock().unwrap();

                if let State {
                    io_context: Some(ref io_context),
                    pending_future_id: Some(ref pending_future_id),
                    ref mut pending_future_cancel,
                    ..
                } = *state
                {
                    let (cancel, future) = io_context.drain_pending_futures(*pending_future_id);
                    *pending_future_cancel = cancel;

                    future
                } else {
                    future::Either::B(future::ok(()))
                }
            }
            Err(err) => future::Either::B(future::err(err)),
        }
    }

    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

        gst_debug!(self.cat, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let mut state = self.state.lock().unwrap();

        let io_context =
            IOContext::new(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to create IO context: {}", err]
                )
            })?;

        let socket = if let Some(ref wrapped_socket) = settings.socket {
            use std::net::UdpSocket;

            let mut socket: UdpSocket;

            #[cfg(unix)]
            {
                socket = wrapped_socket.get()
            }
            #[cfg(windows)]
            {
                socket = wrapped_socket.get()
            }

            let socket =
                net::UdpSocket::from_std(socket, io_context.reactor_handle()).map_err(|err| {
                    gst_error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to setup socket for tokio: {}", err]
                    )
                })?;

            self.settings.lock().unwrap().used_socket = Some(wrapped_socket.clone());

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
                // TODO: Use ::unspecified() constructor once stable
                let bind_addr = if addr.is_ipv4() {
                    IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))
                } else {
                    IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0))
                };

                let saddr = SocketAddr::new(bind_addr, port as u16);
                gst_debug!(
                    self.cat,
                    obj: element,
                    "Binding to {:?} for multicast group {:?}",
                    saddr,
                    addr
                );

                saddr
            } else {
                let saddr = SocketAddr::new(addr, port as u16);
                gst_debug!(self.cat, obj: element, "Binding to {:?}", saddr);

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

            let socket =
                net::UdpSocket::from_std(socket, io_context.reactor_handle()).map_err(|err| {
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
                            .join_multicast_v4(&addr, &Ipv4Addr::new(0, 0, 0, 0))
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
                use libc;

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
                })?;;
                let wrapper = GioSocketWrapper::new(&gio_socket);
                self.settings.lock().unwrap().used_socket = Some(wrapper);
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
                })?;;
                let wrapper = GioSocketWrapper::new(&gio_socket);
                self.settings.lock().unwrap().used_socket = Some(wrapper);
            }

            socket
        };

        let buffer_pool = gst::BufferPool::new();
        let mut config = buffer_pool.get_config();
        config.set_params(None, settings.mtu, 0, 0);
        buffer_pool.set_config(config).map_err(|_| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["Failed to configure buffer pool"]
            )
        })?;

        let socket = Socket::new(element.upcast_ref(), UdpReader::new(socket), buffer_pool);

        let element_clone = element.clone();
        let element_clone2 = element.clone();
        socket
            .schedule(
                &io_context,
                move |buffer| {
                    let udpsrc = Self::from_instance(&element_clone);
                    udpsrc.push_buffer(&element_clone, buffer)
                },
                move |err| {
                    let udpsrc = Self::from_instance(&element_clone2);
                    gst_error!(udpsrc.cat, obj: &element_clone2, "Got error {}", err);
                    match err {
                        Either::Left(gst::FlowError::CustomError) => (),
                        Either::Left(err) => {
                            gst_element_error!(
                                element_clone2,
                                gst::StreamError::Failed,
                                ("Internal data stream error"),
                                ["streaming stopped, reason {}", err]
                            );
                        }
                        Either::Right(err) => {
                            gst_element_error!(
                                element_clone2,
                                gst::StreamError::Failed,
                                ("I/O error"),
                                ["streaming stopped, I/O error {}", err]
                            );
                        }
                    }
                },
            )
            .map_err(|_| {
                gst_error_msg!(gst::ResourceError::OpenRead, ["Failed to schedule socket"])
            })?;

        let pending_future_id = io_context.acquire_pending_future_id();
        gst_debug!(
            self.cat,
            obj: element,
            "Got pending future id {:?}",
            pending_future_id
        );

        state.socket = Some(socket);
        state.io_context = Some(io_context);
        state.pending_future_id = Some(pending_future_id);

        gst_debug!(self.cat, obj: element, "Prepared");
        drop(state);

        element.notify("used-socket");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Unpreparing");

        self.settings.lock().unwrap().used_socket = None;

        // FIXME: The IO Context has to be alive longer than the queue,
        // otherwise the queue can't finish any remaining work
        let (mut socket, io_context) = {
            let mut state = self.state.lock().unwrap();

            if let (&Some(ref pending_future_id), &Some(ref io_context)) =
                (&state.pending_future_id, &state.io_context)
            {
                io_context.release_pending_future_id(*pending_future_id);
            }

            let socket = state.socket.take();
            let io_context = state.io_context.take();
            *state = State::default();
            (socket, io_context)
        };

        if let Some(ref socket) = socket.take() {
            socket.shutdown();
        }
        drop(io_context);

        gst_debug!(self.cat, obj: element, "Unprepared");
        Ok(())
    }

    fn start(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Starting");
        let state = self.state.lock().unwrap();

        if let Some(ref socket) = state.socket {
            socket.unpause(element.get_clock(), Some(element.get_base_time()));
        }

        gst_debug!(self.cat, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Stopping");
        let mut state = self.state.lock().unwrap();

        if let Some(ref socket) = state.socket {
            socket.pause();
        }
        let _ = state.pending_future_cancel.take();

        gst_debug!(self.cat, obj: element, "Stopped");

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
        let src_pad = gst::Pad::new_from_template(&templ, "src");

        src_pad.set_event_function(|pad, parent, event| {
            UdpSrc::catch_panic_pad_function(
                parent,
                || false,
                |udpsrc, element| udpsrc.src_event(pad, element, event),
            )
        });
        src_pad.set_query_function(|pad, parent, query| {
            UdpSrc::catch_panic_pad_function(
                parent,
                || false,
                |udpsrc, element| udpsrc.src_query(pad, element, query),
            )
        });

        Self {
            cat: gst::DebugCategory::new(
                "ts-udpsrc",
                gst::DebugColorFlags::empty(),
                "Thread-sharing UDP source",
            ),
            src_pad: src_pad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for UdpSrc {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("address", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.address = value.get();
            }
            subclass::Property("port", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.port = value.get().unwrap();
            }
            subclass::Property("reuse", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.reuse = value.get().unwrap();
            }
            subclass::Property("caps", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.caps = value.get();
            }
            subclass::Property("mtu", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.mtu = value.get().unwrap();
            }
            subclass::Property("socket", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.socket = value
                    .get::<gio::Socket>()
                    .map(|socket| GioSocketWrapper::new(&socket));
            }
            subclass::Property("used-socket", ..) => {
                unreachable!();
            }
            subclass::Property("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context = value.get().unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_wait = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("address", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.address.to_value())
            }
            subclass::Property("port", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.port.to_value())
            }
            subclass::Property("reuse", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.reuse.to_value())
            }
            subclass::Property("caps", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.caps.to_value())
            }
            subclass::Property("mtu", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.mtu.to_value())
            }
            subclass::Property("socket", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings
                    .socket
                    .as_ref()
                    .map(GioSocketWrapper::as_socket)
                    .to_value())
            }
            subclass::Property("used-socket", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings
                    .used_socket
                    .as_ref()
                    .map(GioSocketWrapper::as_socket)
                    .to_value())
            }
            subclass::Property("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context.to_value())
            }
            subclass::Property("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context_wait.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.src_pad).unwrap();
        ::set_element_flags(element, gst::ElementFlags::SOURCE);
    }
}

impl ElementImpl for UdpSrc {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(self.cat, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare(element).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                self.stop(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => {
                self.start(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                state.need_initial_events = true;
            }
            _ => (),
        }

        Ok(success)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(plugin, "ts-udpsrc", 0, UdpSrc::get_type())
}
