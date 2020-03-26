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

use gio;

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use glib::{glib_object_impl, glib_object_subclass};

use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_element_error, gst_error, gst_error_msg, gst_log, gst_trace};
use gst_net::*;

use lazy_static::lazy_static;

use rand;

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::u16;

use crate::runtime::prelude::*;
use crate::runtime::{Context, PadSrc, PadSrcRef, Task};

use super::socket::{wrap_socket, GioSocketWrapper, Socket, SocketError, SocketRead, SocketState};

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
pub struct UdpReader(Arc<FutMutex<UdpReaderInner>>);

impl UdpReader {
    fn new(socket: tokio::net::UdpSocket) -> Self {
        UdpReader(Arc::new(FutMutex::new(UdpReaderInner { socket })))
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

    fn reset(&self) {
        *self.0.state.try_lock().expect("State locked elsewhere") = Default::default();
        *self.0.configured_caps.lock().unwrap() = None;
    }

    fn set_need_segment(&self) {
        self.0
            .state
            .try_lock()
            .expect("State locked elsewhere")
            .need_segment = true;
    }

    async fn push_prelude(&self, pad: &PadSrcRef<'_>, _element: &gst::Element) {
        let mut state = self.0.state.lock().await;
        if state.need_initial_events {
            gst_debug!(CAT, obj: pad.gst_pad(), "Pushing initial events");

            let stream_id = format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
            let stream_start_evt = gst::Event::new_stream_start(&stream_id)
                .group_id(gst::GroupId::next())
                .build();
            pad.push_event(stream_start_evt).await;

            if let Some(ref caps) = state.caps {
                let caps_evt = gst::Event::new_caps(&caps).build();
                pad.push_event(caps_evt).await;
                *self.0.configured_caps.lock().unwrap() = Some(caps.clone());
            }

            state.need_initial_events = false;
        }

        if state.need_segment {
            let segment_evt =
                gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build();
            pad.push_event(segment_evt).await;

            state.need_segment = false;
        }
    }

    async fn push_buffer(
        &self,
        pad: &PadSrcRef<'_>,
        element: &gst::Element,
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
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                udpsrc.flush_start(element);

                true
            }
            EventView::FlushStop(..) => {
                udpsrc.flush_stop(element);

                true
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
                q.set(true, 0.into(), gst::CLOCK_TIME_NONE);
                true
            }
            QueryView::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryView::Caps(ref mut q) => {
                let caps = if let Some(caps) = self.0.configured_caps.lock().unwrap().as_ref() {
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

struct UdpSrc {
    src_pad: PadSrc,
    src_pad_handler: UdpSrcPadHandler,
    task: Task,
    socket: StdMutex<Option<Socket<UdpReader>>>,
    settings: StdMutex<Settings>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-udpsrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing UDP source"),
    );
}

impl UdpSrc {
    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let mut settings = self.settings.lock().unwrap().clone();

        gst_debug!(CAT, obj: element, "Preparing");

        let context =
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;

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

            let socket = context.enter(|| {
                tokio::net::UdpSocket::from_std(socket).map_err(|err| {
                    gst_error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to setup socket for tokio: {}", err]
                    )
                })
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

            let socket = context.enter(|| {
                tokio::net::UdpSocket::from_std(socket).map_err(|err| {
                    gst_error_msg!(
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

            settings.used_socket = Some(wrap_socket(&socket)?);

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

        let socket = Socket::new(element.upcast_ref(), buffer_pool, async move {
            Ok(UdpReader::new(socket))
        })
        .map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to prepare socket {:?}", err]
            )
        })?;

        *self.socket.lock().unwrap() = Some(socket);
        element.notify("used-socket");

        self.task.prepare(context).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Error preparing Task: {:?}", err]
            )
        })?;
        self.src_pad_handler
            .prepare(settings.caps, settings.retrieve_sender_address);
        self.src_pad.prepare(&self.src_pad_handler);

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Unpreparing");

        *self.socket.lock().unwrap() = None;
        self.settings.lock().unwrap().used_socket = None;
        element.notify("used-socket");

        self.task.unprepare().unwrap();
        self.src_pad.unprepare();

        gst_debug!(CAT, obj: element, "Unprepared");

        Ok(())
    }

    fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Stopping");

        self.task.stop();
        self.src_pad_handler.reset();

        gst_debug!(CAT, obj: element, "Stopped");

        Ok(())
    }

    fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let socket = self.socket.lock().unwrap();
        let socket = socket.as_ref().unwrap();
        if socket.state() == SocketState::Started {
            gst_debug!(CAT, obj: element, "Already started");
            return Ok(());
        }

        gst_debug!(CAT, obj: element, "Starting");
        self.start_task(element, socket);
        gst_debug!(CAT, obj: element, "Started");

        Ok(())
    }

    fn start_task(&self, element: &gst::Element, socket: &Socket<UdpReader>) {
        let socket_stream = socket
            .start(element.get_clock(), Some(element.get_base_time()))
            .unwrap();
        let socket_stream = Arc::new(FutMutex::new(socket_stream));

        let src_pad_handler = self.src_pad_handler.clone();
        let pad_weak = self.src_pad.downgrade();
        let element = element.clone();

        self.task.start(move || {
            let src_pad_handler = src_pad_handler.clone();
            let pad_weak = pad_weak.clone();
            let element = element.clone();
            let socket_stream = socket_stream.clone();

            async move {
                let item = socket_stream.lock().await.next().await;

                let pad = pad_weak.upgrade().expect("PadSrc no longer exists");
                let (mut buffer, saddr) = match item {
                    Some(Ok((buffer, saddr))) => (buffer, saddr),
                    Some(Err(err)) => {
                        gst_error!(CAT, obj: &element, "Got error {:?}", err);
                        match err {
                            SocketError::Gst(err) => {
                                gst_element_error!(
                                    element,
                                    gst::StreamError::Failed,
                                    ("Internal data stream error"),
                                    ["streaming stopped, reason {}", err]
                                );
                            }
                            SocketError::Io(err) => {
                                gst_element_error!(
                                    element,
                                    gst::StreamError::Failed,
                                    ("I/O error"),
                                    ["streaming stopped, I/O error {}", err]
                                );
                            }
                        }
                        return glib::Continue(false);
                    }
                    None => {
                        gst_log!(CAT, obj: pad.gst_pad(), "SocketStream Stopped");
                        return glib::Continue(false);
                    }
                };

                if let Some(saddr) = saddr {
                    if src_pad_handler.0.state.lock().await.retrieve_sender_address {
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

                match src_pad_handler.push_buffer(&pad, &element, buffer).await {
                    Ok(_) => {
                        gst_log!(CAT, obj: pad.gst_pad(), "Successfully pushed buffer");
                        glib::Continue(true)
                    }
                    Err(gst::FlowError::Flushing) => {
                        gst_debug!(CAT, obj: pad.gst_pad(), "Flushing");
                        glib::Continue(false)
                    }
                    Err(gst::FlowError::Eos) => {
                        gst_debug!(CAT, obj: pad.gst_pad(), "EOS");
                        let eos = gst::Event::new_eos().build();
                        pad.push_event(eos).await;
                        glib::Continue(false)
                    }
                    Err(err) => {
                        gst_error!(CAT, obj: pad.gst_pad(), "Got error {}", err);
                        gst_element_error!(
                            element,
                            gst::StreamError::Failed,
                            ("Internal data stream error"),
                            ["streaming stopped, reason {}", err]
                        );
                        glib::Continue(false)
                    }
                }
            }
        });
    }

    fn flush_stop(&self, element: &gst::Element) {
        let socket = self.socket.lock().unwrap();
        if let Some(socket) = socket.as_ref() {
            if socket.state() == SocketState::Started {
                gst_debug!(CAT, obj: element, "Already started");
                return;
            }

            gst_debug!(CAT, obj: element, "Stopping Flush");

            self.src_pad_handler.set_need_segment();
            self.start_task(element, socket);

            gst_debug!(CAT, obj: element, "Stopped Flush");
        } else {
            gst_debug!(CAT, obj: element, "Socket not available");
        }
    }

    fn flush_start(&self, element: &gst::Element) {
        let socket = self.socket.lock().unwrap();
        gst_debug!(CAT, obj: element, "Starting Flush");

        if let Some(socket) = socket.as_ref() {
            socket.pause();
        }

        self.task.cancel();

        gst_debug!(CAT, obj: element, "Flush Started");
    }

    fn pause(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Pausing");

        self.socket.lock().unwrap().as_ref().unwrap().pause();
        self.task.pause();

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
        Self {
            src_pad: PadSrc::new(gst::Pad::new_from_template(
                &klass.get_pad_template("src").unwrap(),
                Some("src"),
            )),
            src_pad_handler: UdpSrcPadHandler::default(),
            task: Task::default(),
            socket: StdMutex::new(None),
            settings: StdMutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for UdpSrc {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        let mut settings = self.settings.lock().unwrap();
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

        let settings = self.settings.lock().unwrap();
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
                self.prepare(element).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                self.pause(element).map_err(|_| gst::StateChangeError)?;
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

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-udpsrc",
        gst::Rank::None,
        UdpSrc::get_type(),
    )
}
