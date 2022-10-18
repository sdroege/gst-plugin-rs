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
//
// SPDX-License-Identifier: LGPL-2.1-or-later

use futures::future::BoxFuture;
use futures::prelude::*;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_net::*;

use once_cell::sync::Lazy;

use std::i32;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Mutex;
use std::time::Duration;
use std::u16;

use crate::runtime::prelude::*;
use crate::runtime::{Async, Context, PadSrc, PadSrcRef, Task};

use crate::socket::{wrap_socket, GioSocketWrapper, Socket, SocketError, SocketRead};

const DEFAULT_ADDRESS: Option<&str> = Some("0.0.0.0");
const DEFAULT_PORT: i32 = 5004;
const DEFAULT_REUSE: bool = true;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_MTU: u32 = 1492;
const DEFAULT_SOCKET: Option<GioSocketWrapper> = None;
const DEFAULT_USED_SOCKET: Option<GioSocketWrapper> = None;
const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: Duration = Duration::ZERO;
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
struct UdpReader(Async<UdpSocket>);

impl UdpReader {
    fn new(socket: Async<UdpSocket>) -> Self {
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

#[derive(Clone, Debug)]
struct UdpSrcPadHandler;

impl PadSrcHandler for UdpSrcPadHandler {
    type ElementImpl = UdpSrc;

    fn src_event(&self, pad: &PadSrcRef, imp: &UdpSrc, event: gst::Event) -> bool {
        gst::log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        use gst::EventView;
        let ret = match event.view() {
            EventView::FlushStart(..) => imp.task.flush_start().await_maybe_on_context().is_ok(),
            EventView::FlushStop(..) => imp.task.flush_stop().await_maybe_on_context().is_ok(),
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst::log!(CAT, obj: pad.gst_pad(), "Handled {:?}", event);
        } else {
            gst::log!(CAT, obj: pad.gst_pad(), "Didn't handle {:?}", event);
        }

        ret
    }

    fn src_query(&self, pad: &PadSrcRef, imp: &UdpSrc, query: &mut gst::QueryRef) -> bool {
        gst::log!(CAT, obj: pad.gst_pad(), "Handling {:?}", query);

        use gst::QueryViewMut;
        let ret = match query.view_mut() {
            QueryViewMut::Latency(q) => {
                q.set(true, gst::ClockTime::ZERO, gst::ClockTime::NONE);
                true
            }
            QueryViewMut::Scheduling(q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryViewMut::Caps(q) => {
                let caps = if let Some(caps) = imp.configured_caps.lock().unwrap().as_ref() {
                    q.filter()
                        .map(|f| f.intersect_with_mode(caps, gst::CapsIntersectMode::First))
                        .unwrap_or_else(|| caps.clone())
                } else {
                    q.filter()
                        .map(|f| f.to_owned())
                        .unwrap_or_else(gst::Caps::new_any)
                };

                q.set_result(Some(&caps));

                true
            }
            _ => false,
        };

        if ret {
            gst::log!(CAT, obj: pad.gst_pad(), "Handled {:?}", query);
        } else {
            gst::log!(CAT, obj: pad.gst_pad(), "Didn't handle {:?}", query);
        }

        ret
    }
}

struct UdpSrcTask {
    element: super::UdpSrc,
    socket: Option<Socket<UdpReader>>,
    retrieve_sender_address: bool,
    need_initial_events: bool,
    need_segment: bool,
}

impl UdpSrcTask {
    fn new(element: super::UdpSrc) -> Self {
        UdpSrcTask {
            element,
            socket: None,
            retrieve_sender_address: DEFAULT_RETRIEVE_SENDER_ADDRESS,
            need_initial_events: true,
            need_segment: true,
        }
    }
}

impl TaskImpl for UdpSrcTask {
    type Item = gst::Buffer;

    fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            let udpsrc = self.element.imp();
            let mut settings = udpsrc.settings.lock().unwrap();

            gst::debug!(CAT, obj: &self.element, "Preparing Task");

            self.retrieve_sender_address = settings.retrieve_sender_address;

            let socket = if let Some(ref wrapped_socket) = settings.socket {
                let socket: UdpSocket;

                #[cfg(unix)]
                {
                    socket = wrapped_socket.get()
                }
                #[cfg(windows)]
                {
                    socket = wrapped_socket.get()
                }

                let socket = Async::<UdpSocket>::try_from(socket).map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to setup Async socket: {}", err]
                    )
                })?;

                settings.used_socket = Some(wrapped_socket.clone());

                socket
            } else {
                let addr: IpAddr = match settings.address {
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
                let port = settings.port;

                // TODO: TTL, multicast loopback, etc
                let saddr = if addr.is_multicast() {
                    let bind_addr = if addr.is_ipv4() {
                        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
                    } else {
                        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
                    };

                    let saddr = SocketAddr::new(bind_addr, port as u16);
                    gst::debug!(
                        CAT,
                        obj: &self.element,
                        "Binding to {:?} for multicast group {:?}",
                        saddr,
                        addr
                    );

                    saddr
                } else {
                    let saddr = SocketAddr::new(addr, port as u16);
                    gst::debug!(CAT, obj: &self.element, "Binding to {:?}", saddr);

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

                socket.set_reuse_address(settings.reuse).map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to set reuse_address: {}", err]
                    )
                })?;

                #[cfg(unix)]
                {
                    socket.set_reuse_port(settings.reuse).map_err(|err| {
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

                let socket = Async::<UdpSocket>::try_from(socket).map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to setup Async socket: {}", err]
                    )
                })?;

                if addr.is_multicast() {
                    // TODO: Multicast interface configuration, going to be tricky
                    match addr {
                        IpAddr::V4(addr) => {
                            socket
                                .as_ref()
                                .join_multicast_v4(&addr, &Ipv4Addr::new(0, 0, 0, 0))
                                .map_err(|err| {
                                    gst::error_msg!(
                                        gst::ResourceError::OpenRead,
                                        ["Failed to join multicast group: {}", err]
                                    )
                                })?;
                        }
                        IpAddr::V6(addr) => {
                            socket.as_ref().join_multicast_v6(&addr, 0).map_err(|err| {
                                gst::error_msg!(
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

            let port: i32 = socket.as_ref().local_addr().unwrap().port().into();
            if settings.port != port {
                settings.port = port;
                drop(settings);
                self.element.notify("port");

                settings = udpsrc.settings.lock().unwrap();
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

            self.socket = Some(
                Socket::try_new(
                    self.element.clone().upcast(),
                    buffer_pool,
                    UdpReader::new(socket),
                )
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to prepare socket {:?}", err]
                    )
                })?,
            );

            self.element.notify("used-socket");

            Ok(())
        }
        .boxed()
    }

    fn unprepare(&mut self) -> BoxFuture<'_, ()> {
        async move {
            gst::debug!(CAT, obj: &self.element, "Unpreparing Task");
            let udpsrc = self.element.imp();
            udpsrc.settings.lock().unwrap().used_socket = None;
            self.element.notify("used-socket");
        }
        .boxed()
    }

    fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst::log!(CAT, obj: &self.element, "Starting task");
            self.socket
                .as_mut()
                .unwrap()
                .set_clock(self.element.clock(), self.element.base_time());
            gst::log!(CAT, obj: &self.element, "Task started");
            Ok(())
        }
        .boxed()
    }

    fn try_next(&mut self) -> BoxFuture<'_, Result<gst::Buffer, gst::FlowError>> {
        async move {
            self.socket
                .as_mut()
                .unwrap()
                .try_next()
                .await
                .map(|(mut buffer, saddr)| {
                    if let Some(saddr) = saddr {
                        if self.retrieve_sender_address {
                            NetAddressMeta::add(
                                buffer.get_mut().unwrap(),
                                &gio::InetSocketAddress::from(saddr),
                            );
                        }
                    }
                    buffer
                })
                .map_err(|err| {
                    gst::error!(CAT, obj: &self.element, "Got error {:?}", err);
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
                    gst::FlowError::Error
                })
        }
        .boxed()
    }

    fn handle_item(&mut self, buffer: gst::Buffer) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async {
            gst::log!(CAT, obj: &self.element, "Handling {:?}", buffer);
            let udpsrc = self.element.imp();

            if self.need_initial_events {
                gst::debug!(CAT, obj: &self.element, "Pushing initial events");

                let stream_id =
                    format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
                let stream_start_evt = gst::event::StreamStart::builder(&stream_id)
                    .group_id(gst::GroupId::next())
                    .build();
                udpsrc.src_pad.push_event(stream_start_evt).await;

                let caps = udpsrc.settings.lock().unwrap().caps.clone();
                if let Some(caps) = caps {
                    udpsrc
                        .src_pad
                        .push_event(gst::event::Caps::new(&caps))
                        .await;
                    *udpsrc.configured_caps.lock().unwrap() = Some(caps);
                }

                self.need_initial_events = false;
            }

            if self.need_segment {
                let segment_evt =
                    gst::event::Segment::new(&gst::FormattedSegment::<gst::format::Time>::new());
                udpsrc.src_pad.push_event(segment_evt).await;

                self.need_segment = false;
            }

            let res = udpsrc.src_pad.push(buffer).await.map(drop);
            match res {
                Ok(_) => gst::log!(CAT, obj: &self.element, "Successfully pushed buffer"),
                Err(gst::FlowError::Flushing) => gst::debug!(CAT, obj: &self.element, "Flushing"),
                Err(gst::FlowError::Eos) => {
                    gst::debug!(CAT, obj: &self.element, "EOS");
                    udpsrc.src_pad.push_event(gst::event::Eos::new()).await;
                }
                Err(err) => {
                    gst::error!(CAT, obj: &self.element, "Got error {}", err);
                    gst::element_error!(
                        self.element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["streaming stopped, reason {}", err]
                    );
                }
            }

            res
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst::log!(CAT, obj: &self.element, "Stopping task");
            self.need_initial_events = true;
            self.need_segment = true;
            gst::log!(CAT, obj: &self.element, "Task stopped");
            Ok(())
        }
        .boxed()
    }

    fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst::log!(CAT, obj: &self.element, "Stopping task flush");
            self.need_segment = true;
            gst::log!(CAT, obj: &self.element, "Stopped task flush");
            Ok(())
        }
        .boxed()
    }
}

pub struct UdpSrc {
    src_pad: PadSrc,
    task: Task,
    configured_caps: Mutex<Option<gst::Caps>>,
    settings: Mutex<Settings>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-udpsrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing UDP source"),
    )
});

impl UdpSrc {
    fn prepare(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Preparing");

        let settings = self.settings.lock().unwrap();
        let context =
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;
        drop(settings);

        *self.configured_caps.lock().unwrap() = None;
        self.task
            .prepare(UdpSrcTask::new(self.instance().clone()), context)
            .block_on()?;

        gst::debug!(CAT, imp: self, "Prepared");

        Ok(())
    }

    fn unprepare(&self) {
        gst::debug!(CAT, imp: self, "Unpreparing");
        self.task.unprepare().block_on().unwrap();
        gst::debug!(CAT, imp: self, "Unprepared");
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Stopping");
        self.task.stop().block_on()?;
        gst::debug!(CAT, imp: self, "Stopped");
        Ok(())
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Starting");
        self.task.start().block_on()?;
        gst::debug!(CAT, imp: self, "Started");
        Ok(())
    }

    fn pause(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp: self, "Pausing");
        self.task.pause().block_on()?;
        gst::debug!(CAT, imp: self, "Paused");
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for UdpSrc {
    const NAME: &'static str = "RsTsUdpSrc";
    type Type = super::UdpSrc;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        Self {
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap(), Some("src")),
                UdpSrcPadHandler,
            ),
            task: Task::default(),
            configured_caps: Default::default(),
            settings: Default::default(),
        }
    }
}

impl ObjectImpl for UdpSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            let mut properties = vec![
                glib::ParamSpecString::builder("context")
                    .nick("Context")
                    .blurb("Context name to share threads with")
                    .default_value(Some(DEFAULT_CONTEXT))
                    .build(),
                glib::ParamSpecUInt::builder("context-wait")
                    .nick("Context Wait")
                    .blurb("Throttle poll loop to run at most once every this many ms")
                    .maximum(1000)
                    .default_value(DEFAULT_CONTEXT_WAIT.as_millis() as u32)
                    .build(),
                glib::ParamSpecString::builder("address")
                    .nick("Address")
                    .blurb("Address/multicast group to listen on")
                    .default_value(DEFAULT_ADDRESS)
                    .build(),
                glib::ParamSpecInt::builder("port")
                    .nick("Port")
                    .blurb("Port to listen on")
                    .minimum(0)
                    .maximum(u16::MAX as i32)
                    .default_value(DEFAULT_PORT)
                    .build(),
                glib::ParamSpecBoolean::builder("reuse")
                    .nick("Reuse")
                    .blurb("Allow reuse of the port")
                    .default_value(DEFAULT_REUSE)
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("caps")
                    .nick("Caps")
                    .blurb("Caps to use")
                    .build(),
                glib::ParamSpecUInt::builder("mtu")
                    .nick("MTU")
                    .blurb("Maximum expected packet size. This directly defines the allocation size of the receive buffer pool")
                    .maximum(i32::MAX as u32)
                    .default_value(DEFAULT_MTU)
                    .build(),
                glib::ParamSpecBoolean::builder("retrieve-sender-address")
                    .nick("Retrieve sender address")
                    .blurb("Whether to retrieve the sender address and add it to buffers as meta. Disabling this might result in minor performance improvements in certain scenarios")
                    .default_value(DEFAULT_RETRIEVE_SENDER_ADDRESS)
                    .build(),
            ];

            #[cfg(not(windows))]
            {
                properties.push(
                    glib::ParamSpecObject::builder::<gio::Socket>("socket")
                        .nick("Socket")
                        .blurb("Socket to use for UDP reception. (None == allocate)")
                        .build(),
                );
                properties.push(
                    glib::ParamSpecObject::builder::<gio::Socket>("used-socket")
                        .nick("Used Socket")
                        .blurb("Socket currently in use for UDP reception. (None = no socket)")
                        .read_only()
                        .build(),
                );
            }

            properties
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
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
                    .unwrap_or_else(|| DEFAULT_CONTEXT.into());
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

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
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

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.instance();
        obj.add_pad(self.src_pad.gst_pad()).unwrap();
        obj.set_element_flags(gst::ElementFlags::SOURCE);
    }
}

impl GstObjectImpl for UdpSrc {}

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
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp: self, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare().map_err(|err| {
                    self.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                self.pause().map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare();
            }
            _ => (),
        }

        let mut success = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => {
                self.start().map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToReady => {
                self.stop().map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        Ok(success)
    }
}
