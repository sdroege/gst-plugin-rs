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

use futures::future::BoxFuture;
use futures::lock::Mutex as FutMutex;
use futures::prelude::*;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_log, gst_trace};
use gst_net::*;

use once_cell::sync::Lazy;

use std::i32;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use std::u16;

use crate::runtime::prelude::*;
use crate::runtime::{Context, PadSrc, PadSrcRef, PadSrcWeak, Task};

use crate::socket::{wrap_socket, GioSocketWrapper, Socket, SocketError, SocketRead};

const DEFAULT_ADDRESS: Option<&str> = Some("0.0.0.0");
const DEFAULT_PORT: i32 = 5000;
const DEFAULT_REUSE: bool = true;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_MTU: u32 = 1492;
const DEFAULT_SOCKET: Option<GioSocketWrapper> = None;
const DEFAULT_USED_SOCKET: Option<GioSocketWrapper> = None;
const DEFAULT_CONTEXT: &str = "";
// FIXME use Duration::ZERO when MSVC >= 1.53.2
const DEFAULT_CONTEXT_WAIT: Duration = Duration::from_nanos(0);
const DEFAULT_RETRIEVE_SENDER_ADDRESS: bool = true;

#[derive(Debug, Clone)]
struct Settings {
    address: Option<String>,
    port: i32, // for conformity with C based udpsrc
    reuse: bool,
    caps: Option<gst::Caps>,
    mtu: u32,
    socket: Option<GioSocketWrapper>,
    used_socket: Option<GioSocketWrapper>,
    context: String,
    context_wait: Duration,
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

#[derive(Debug)]
struct UdpReader(tokio::net::UdpSocket);

impl UdpReader {
    fn new(socket: tokio::net::UdpSocket) -> Self {
        UdpReader(socket)
    }
}

impl SocketRead for UdpReader {
    const DO_TIMESTAMP: bool = true;

    fn read<'buf>(
        &'buf mut self,
        buffer: &'buf mut [u8],
    ) -> BoxFuture<'buf, io::Result<(usize, Option<std::net::SocketAddr>)>> {
        async move {
            self.0
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
    need_segment: bool,
    caps: Option<gst::Caps>,
}

impl Default for UdpSrcPadHandlerState {
    fn default() -> Self {
        UdpSrcPadHandlerState {
            retrieve_sender_address: true,
            need_initial_events: true,
            need_segment: true,
            caps: None,
        }
    }
}

#[derive(Debug, Default)]
struct UdpSrcPadHandlerInner {
    state: FutMutex<UdpSrcPadHandlerState>,
    configured_caps: StdMutex<Option<gst::Caps>>,
}

#[derive(Clone, Debug, Default)]
struct UdpSrcPadHandler(Arc<UdpSrcPadHandlerInner>);

impl UdpSrcPadHandler {
    fn prepare(&self, caps: Option<gst::Caps>, retrieve_sender_address: bool) {
        let mut state = self.0.state.try_lock().expect("State locked elsewhere");

        state.caps = caps;
        state.retrieve_sender_address = retrieve_sender_address;
    }

    async fn reset_state(&self) {
        *self.0.state.lock().await = Default::default();
    }

    async fn set_need_segment(&self) {
        self.0.state.lock().await.need_segment = true;
    }

    async fn push_prelude(&self, pad: &PadSrcRef<'_>, _element: &super::UdpSrc) {
        let mut state = self.0.state.lock().await;
        if state.need_initial_events {
            gst_debug!(CAT, obj: pad.gst_pad(), "Pushing initial events");

            let stream_id = format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
            let stream_start_evt = gst::event::StreamStart::builder(&stream_id)
                .group_id(gst::GroupId::next())
                .build();
            pad.push_event(stream_start_evt).await;

            if let Some(ref caps) = state.caps {
                pad.push_event(gst::event::Caps::new(&caps)).await;
                *self.0.configured_caps.lock().unwrap() = Some(caps.clone());
            }

            state.need_initial_events = false;
        }

        if state.need_segment {
            let segment_evt =
                gst::event::Segment::new(&gst::FormattedSegment::<gst::format::Time>::new());
            pad.push_event(segment_evt).await;

            state.need_segment = false;
        }
    }

    async fn push_buffer(
        &self,
        pad: &PadSrcRef<'_>,
        element: &super::UdpSrc,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", buffer);

        self.push_prelude(pad, element).await;

        pad.push(buffer).await
    }
}

impl PadSrcHandler for UdpSrcPadHandler {
    type ElementImpl = UdpSrc;

    fn src_event(
        &self,
        pad: &PadSrcRef,
        udpsrc: &UdpSrc,
        _element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => udpsrc.task.flush_start().is_ok(),
            EventView::FlushStop(..) => udpsrc.task.flush_stop().is_ok(),
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst_log!(CAT, obj: pad.gst_pad(), "Handled {:?}", event);
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Didn't handle {:?}", event);
        }

        ret
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
                q.set(true, gst::ClockTime::ZERO, gst::ClockTime::NONE);
                true
            }
            QueryView::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryView::Caps(ref mut q) => {
                let caps = if let Some(caps) = self.0.configured_caps.lock().unwrap().as_ref() {
                    q.filter()
                        .map(|f| f.intersect_with_mode(caps, gst::CapsIntersectMode::First))
                        .unwrap_or_else(|| caps.clone())
                } else {
                    q.filter()
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

struct UdpSrcTask {
    element: super::UdpSrc,
    src_pad: PadSrcWeak,
    src_pad_handler: UdpSrcPadHandler,
    socket: Socket<UdpReader>,
}

impl UdpSrcTask {
    fn new(
        element: &super::UdpSrc,
        src_pad: &PadSrc,
        src_pad_handler: &UdpSrcPadHandler,
        socket: Socket<UdpReader>,
    ) -> Self {
        UdpSrcTask {
            element: element.clone(),
            src_pad: src_pad.downgrade(),
            src_pad_handler: src_pad_handler.clone(),
            socket,
        }
    }
}

impl TaskImpl for UdpSrcTask {
    fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Starting task");
            self.socket
                .set_clock(self.element.clock(), self.element.base_time());
            gst_log!(CAT, obj: &self.element, "Task started");
            Ok(())
        }
        .boxed()
    }

    fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            let item = self.socket.next().await;

            let (mut buffer, saddr) = match item {
                Some(Ok((buffer, saddr))) => (buffer, saddr),
                Some(Err(err)) => {
                    gst_error!(CAT, obj: &self.element, "Got error {:?}", err);
                    match err {
                        SocketError::Gst(err) => {
                            gst::element_error!(
                                self.element,
                                gst::StreamError::Failed,
                                ("Internal data stream error"),
                                ["streaming stopped, reason {}", err]
                            );
                        }
                        SocketError::Io(err) => {
                            gst::element_error!(
                                self.element,
                                gst::StreamError::Failed,
                                ("I/O error"),
                                ["streaming stopped, I/O error {}", err]
                            );
                        }
                    }
                    return Err(gst::FlowError::Error);
                }
                None => {
                    gst_log!(CAT, obj: &self.element, "SocketStream Stopped");
                    return Err(gst::FlowError::Flushing);
                }
            };

            if let Some(saddr) = saddr {
                if self
                    .src_pad_handler
                    .0
                    .state
                    .lock()
                    .await
                    .retrieve_sender_address
                {
                    NetAddressMeta::add(
                        buffer.get_mut().unwrap(),
                        &gio::InetSocketAddress::from(saddr),
                    );
                }
            }

            let pad = self.src_pad.upgrade().expect("PadSrc no longer exists");
            let res = self
                .src_pad_handler
                .push_buffer(&pad, &self.element, buffer)
                .await;
            match res {
                Ok(_) => gst_log!(CAT, obj: &self.element, "Successfully pushed buffer"),
                Err(gst::FlowError::Flushing) => gst_debug!(CAT, obj: &self.element, "Flushing"),
                Err(gst::FlowError::Eos) => {
                    gst_debug!(CAT, obj: &self.element, "EOS");
                    pad.push_event(gst::event::Eos::new()).await;
                }
                Err(err) => {
                    gst_error!(CAT, obj: &self.element, "Got error {}", err);
                    gst::element_error!(
                        self.element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["streaming stopped, reason {}", err]
                    );
                }
            }

            res.map(drop)
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Stopping task");
            self.src_pad_handler.reset_state().await;
            gst_log!(CAT, obj: &self.element, "Task stopped");
            Ok(())
        }
        .boxed()
    }

    fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Stopping task flush");
            self.src_pad_handler.set_need_segment().await;
            gst_log!(CAT, obj: &self.element, "Stopped task flush");
            Ok(())
        }
        .boxed()
    }
}

pub struct UdpSrc {
    src_pad: PadSrc,
    src_pad_handler: UdpSrcPadHandler,
    task: Task,
    settings: StdMutex<Settings>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-udpsrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing UDP source"),
    )
});

impl UdpSrc {
    fn prepare(&self, element: &super::UdpSrc) -> Result<(), gst::ErrorMessage> {
        let mut settings_guard = self.settings.lock().unwrap();

        gst_debug!(CAT, obj: element, "Preparing");

        let context = Context::acquire(&settings_guard.context, settings_guard.context_wait)
            .map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;

        let socket = if let Some(ref wrapped_socket) = settings_guard.socket {
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

            let socket = context.enter(|| {
                tokio::net::UdpSocket::from_std(socket).map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to setup socket for tokio: {}", err]
                    )
                })
            })?;

            settings_guard.used_socket = Some(wrapped_socket.clone());

            socket
        } else {
            let addr: IpAddr = match settings_guard.address {
                None => {
                    return Err(gst::error_msg!(
                        gst::ResourceError::Settings,
                        ["No address set"]
                    ));
                }
                Some(ref addr) => match addr.parse() {
                    Err(err) => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Settings,
                            ["Invalid address '{}' set: {}", addr, err]
                        ));
                    }
                    Ok(addr) => addr,
                },
            };
            let port = settings_guard.port;

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
                    obj: element,
                    "Binding to {:?} for multicast group {:?}",
                    saddr,
                    addr
                );

                saddr
            } else {
                let saddr = SocketAddr::new(addr, port as u16);
                gst_debug!(CAT, obj: element, "Binding to {:?}", saddr);

                saddr
            };

            let socket = if addr.is_ipv4() {
                socket2::Socket::new(
                    socket2::Domain::IPV4,
                    socket2::Type::DGRAM,
                    Some(socket2::Protocol::UDP),
                )
            } else {
                socket2::Socket::new(
                    socket2::Domain::IPV6,
                    socket2::Type::DGRAM,
                    Some(socket2::Protocol::UDP),
                )
            }
            .map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to create socket: {}", err]
                )
            })?;

            socket
                .set_reuse_address(settings_guard.reuse)
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to set reuse_address: {}", err]
                    )
                })?;

            #[cfg(unix)]
            {
                socket.set_reuse_port(settings_guard.reuse).map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to set reuse_port: {}", err]
                    )
                })?;
            }

            socket.bind(&saddr.into()).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to bind socket: {}", err]
                )
            })?;

            let socket = context.enter(|| {
                tokio::net::UdpSocket::from_std(socket.into()).map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to setup socket for tokio: {}", err]
                    )
                })
            })?;

            if addr.is_multicast() {
                // TODO: Multicast interface configuration, going to be tricky
                match addr {
                    IpAddr::V4(addr) => {
                        socket
                            .join_multicast_v4(addr, Ipv4Addr::new(0, 0, 0, 0))
                            .map_err(|err| {
                                gst::error_msg!(
                                    gst::ResourceError::OpenRead,
                                    ["Failed to join multicast group: {}", err]
                                )
                            })?;
                    }
                    IpAddr::V6(addr) => {
                        socket.join_multicast_v6(&addr, 0).map_err(|err| {
                            gst::error_msg!(
                                gst::ResourceError::OpenRead,
                                ["Failed to join multicast group: {}", err]
                            )
                        })?;
                    }
                }
            }

            settings_guard.used_socket = Some(wrap_socket(&socket)?);

            socket
        };

        let port: i32 = socket.local_addr().unwrap().port().into();
        let settings = if settings_guard.port != port {
            settings_guard.port = port;
            let settings = settings_guard.clone();
            drop(settings_guard);
            element.notify("port");

            settings
        } else {
            let settings = settings_guard.clone();
            drop(settings_guard);

            settings
        };

        let buffer_pool = gst::BufferPool::new();
        let mut config = buffer_pool.config();
        config.set_params(None, settings.mtu, 0, 0);
        buffer_pool.set_config(config).map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::Settings,
                ["Failed to configure buffer pool {:?}", err]
            )
        })?;

        let socket = Socket::try_new(
            element.clone().upcast(),
            buffer_pool,
            UdpReader::new(socket),
        )
        .map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to prepare socket {:?}", err]
            )
        })?;

        element.notify("used-socket");

        self.src_pad_handler
            .prepare(settings.caps, settings.retrieve_sender_address);

        self.task
            .prepare(
                UdpSrcTask::new(element, &self.src_pad, &self.src_pad_handler, socket),
                context,
            )
            .map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing Task: {:?}", err]
                )
            })?;

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &super::UdpSrc) {
        gst_debug!(CAT, obj: element, "Unpreparing");

        self.settings.lock().unwrap().used_socket = None;
        element.notify("used-socket");

        self.task.unprepare().unwrap();

        gst_debug!(CAT, obj: element, "Unprepared");
    }

    fn stop(&self, element: &super::UdpSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Stopping");
        self.task.stop()?;
        gst_debug!(CAT, obj: element, "Stopped");
        Ok(())
    }

    fn start(&self, element: &super::UdpSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Starting");
        self.task.start()?;
        gst_debug!(CAT, obj: element, "Started");
        Ok(())
    }

    fn pause(&self, element: &super::UdpSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Pausing");
        self.task.pause()?;
        gst_debug!(CAT, obj: element, "Paused");
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for UdpSrc {
    const NAME: &'static str = "RsTsUdpSrc";
    type Type = super::UdpSrc;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let src_pad_handler = UdpSrcPadHandler::default();

        Self {
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap(), Some("src")),
                src_pad_handler.clone(),
            ),
            src_pad_handler,
            task: Task::default(),
            settings: StdMutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for UdpSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            let mut properties = vec![
                glib::ParamSpec::new_string(
                    "context",
                    "Context",
                    "Context name to share threads with",
                    Some(DEFAULT_CONTEXT),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "context-wait",
                    "Context Wait",
                    "Throttle poll loop to run at most once every this many ms",
                    0,
                    1000,
                    DEFAULT_CONTEXT_WAIT.as_millis() as u32,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_string(
                    "address",
                    "Address",
                    "Address/multicast group to listen on",
                    DEFAULT_ADDRESS,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_int(
                    "port",
                    "Port",
                    "Port to listen on",
                    0,
                    u16::MAX as i32,
                    DEFAULT_PORT,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_boolean(
                    "reuse",
                    "Reuse",
                    "Allow reuse of the port",
                    DEFAULT_REUSE,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_boxed(
                    "caps",
                    "Caps",
                    "Caps to use",
                    gst::Caps::static_type(),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "mtu",
                    "MTU",
                    "Maximum expected packet size. This directly defines the allocation size of the receive buffer pool",
                    0,
                    i32::MAX as u32,
                    DEFAULT_MTU,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_boolean(
                    "retrieve-sender-address",
                    "Retrieve sender address",
                    "Whether to retrieve the sender address and add it to buffers as meta. Disabling this might result in minor performance improvements in certain scenarios",
                    DEFAULT_RETRIEVE_SENDER_ADDRESS,
                    glib::ParamFlags::READWRITE,
                ),
            ];

            #[cfg(not(windows))]
            {
                properties.push(glib::ParamSpec::new_object(
                    "socket",
                    "Socket",
                    "Socket to use for UDP reception. (None == allocate)",
                    gio::Socket::static_type(),
                    glib::ParamFlags::READWRITE,
                ));
                properties.push(glib::ParamSpec::new_object(
                    "used-socket",
                    "Used Socket",
                    "Socket currently in use for UDP reception. (None = no socket)",
                    gio::Socket::static_type(),
                    glib::ParamFlags::READABLE,
                ));
            }

            properties
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "address" => {
                settings.address = value.get().expect("type checked upstream");
            }
            "port" => {
                settings.port = value.get().expect("type checked upstream");
            }
            "reuse" => {
                settings.reuse = value.get().expect("type checked upstream");
            }
            "caps" => {
                settings.caps = value.get().expect("type checked upstream");
            }
            "mtu" => {
                settings.mtu = value.get().expect("type checked upstream");
            }
            "socket" => {
                settings.socket = value
                    .get::<Option<gio::Socket>>()
                    .expect("type checked upstream")
                    .map(|socket| GioSocketWrapper::new(&socket));
            }
            "used-socket" => {
                unreachable!();
            }
            "context" => {
                settings.context = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            "context-wait" => {
                settings.context_wait = Duration::from_millis(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            "retrieve-sender-address" => {
                settings.retrieve_sender_address = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "address" => settings.address.to_value(),
            "port" => settings.port.to_value(),
            "reuse" => settings.reuse.to_value(),
            "caps" => settings.caps.to_value(),
            "mtu" => settings.mtu.to_value(),
            "socket" => settings
                .socket
                .as_ref()
                .map(GioSocketWrapper::as_socket)
                .to_value(),
            "used-socket" => settings
                .used_socket
                .as_ref()
                .map(GioSocketWrapper::as_socket)
                .to_value(),
            "context" => settings.context.to_value(),
            "context-wait" => (settings.context_wait.as_millis() as u32).to_value(),
            "retrieve-sender-address" => settings.retrieve_sender_address.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(self.src_pad.gst_pad()).unwrap();

        crate::set_element_flags(obj, gst::ElementFlags::SOURCE);
    }
}

impl ElementImpl for UdpSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing UDP source",
                "Source/Network",
                "Receives data over the network via UDP",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare(element).map_err(|err| {
                    element.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                self.pause(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element);
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
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToReady => {
                self.stop(element).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        Ok(success)
    }
}
