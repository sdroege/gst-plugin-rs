// Copyright (C) 2019 Mathieu Duponchelle <mathieu@centricular.com>
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

use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::lock::Mutex;
use futures::prelude::*;

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use glib::{glib_object_impl, glib_object_subclass};

use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::EventView;
use gst::{
    gst_debug, gst_element_error, gst_error, gst_error_msg, gst_info, gst_log, gst_trace,
    gst_warning,
};

use lazy_static::lazy_static;

use crate::runtime::prelude::*;
use crate::runtime::{self, Context, PadSink, PadSinkRef, Task};
use crate::socket::{wrap_socket, GioSocketWrapper};

use std::convert::TryInto;
use std::mem;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::string::ToString;
use std::sync::Mutex as StdMutex;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use std::u16;
use std::u8;

const DEFAULT_HOST: Option<&str> = Some("127.0.0.1");
const DEFAULT_PORT: u32 = 5000;
const DEFAULT_SYNC: bool = true;
const DEFAULT_BIND_ADDRESS: &str = "0.0.0.0";
const DEFAULT_BIND_PORT: u32 = 0;
const DEFAULT_BIND_ADDRESS_V6: &str = "::";
const DEFAULT_BIND_PORT_V6: u32 = 0;
const DEFAULT_SOCKET: Option<GioSocketWrapper> = None;
const DEFAULT_USED_SOCKET: Option<GioSocketWrapper> = None;
const DEFAULT_SOCKET_V6: Option<GioSocketWrapper> = None;
const DEFAULT_USED_SOCKET_V6: Option<GioSocketWrapper> = None;
const DEFAULT_AUTO_MULTICAST: bool = true;
const DEFAULT_LOOP: bool = true;
const DEFAULT_TTL: u32 = 64;
const DEFAULT_TTL_MC: u32 = 1;
const DEFAULT_QOS_DSCP: i32 = -1;
const DEFAULT_CLIENTS: &str = "";
const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;

#[derive(Debug, Clone)]
struct Settings {
    host: Option<String>,
    port: u32,
    sync: bool,
    bind_address: String,
    bind_port: u32,
    bind_address_v6: String,
    bind_port_v6: u32,
    socket: Option<GioSocketWrapper>,
    used_socket: Option<GioSocketWrapper>,
    socket_v6: Option<GioSocketWrapper>,
    used_socket_v6: Option<GioSocketWrapper>,
    auto_multicast: bool,
    multicast_loop: bool,
    ttl: u32,
    ttl_mc: u32,
    qos_dscp: i32,
    context: String,
    context_wait: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            host: DEFAULT_HOST.map(Into::into),
            port: DEFAULT_PORT,
            sync: DEFAULT_SYNC,
            bind_address: DEFAULT_BIND_ADDRESS.into(),
            bind_port: DEFAULT_BIND_PORT,
            bind_address_v6: DEFAULT_BIND_ADDRESS_V6.into(),
            bind_port_v6: DEFAULT_BIND_PORT_V6,
            socket: DEFAULT_SOCKET,
            used_socket: DEFAULT_USED_SOCKET,
            socket_v6: DEFAULT_SOCKET_V6,
            used_socket_v6: DEFAULT_USED_SOCKET_V6,
            auto_multicast: DEFAULT_AUTO_MULTICAST,
            multicast_loop: DEFAULT_LOOP,
            ttl: DEFAULT_TTL,
            ttl_mc: DEFAULT_TTL_MC,
            qos_dscp: DEFAULT_QOS_DSCP,
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
        }
    }
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-udpsink",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing UDP sink"),
    );
}

static PROPERTIES: [subclass::Property; 19] = [
    subclass::Property("host", |name| {
        glib::ParamSpec::string(
            name,
            "Host",
            "The host/IP/Multicast group to send the packets to",
            DEFAULT_HOST,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("port", |name| {
        glib::ParamSpec::uint(
            name,
            "Port",
            "The port to send the packets to",
            0,
            u16::MAX as u32,
            DEFAULT_PORT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("sync", |name| {
        glib::ParamSpec::boolean(
            name,
            "Sync",
            "Sync on the clock",
            DEFAULT_SYNC,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("bind-address", |name| {
        glib::ParamSpec::string(
            name,
            "Bind Address",
            "Address to bind the socket to",
            Some(DEFAULT_BIND_ADDRESS),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("bind-port", |name| {
        glib::ParamSpec::uint(
            name,
            "Bind Port",
            "Port to bind the socket to",
            0,
            u16::MAX as u32,
            DEFAULT_BIND_PORT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("bind-address-v6", |name| {
        glib::ParamSpec::string(
            name,
            "Bind Address V6",
            "Address to bind the V6 socket to",
            Some(DEFAULT_BIND_ADDRESS_V6),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("bind-port-v6", |name| {
        glib::ParamSpec::uint(
            name,
            "Bind Port",
            "Port to bind the V6 socket to",
            0,
            u16::MAX as u32,
            DEFAULT_BIND_PORT_V6,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("socket", |name| {
        glib::ParamSpec::object(
            name,
            "Socket",
            "Socket to use for UDP transmission. (None == allocate)",
            gio::Socket::static_type(),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("used-socket", |name| {
        glib::ParamSpec::object(
            name,
            "Used Socket",
            "Socket currently in use for UDP transmission. (None = no socket)",
            gio::Socket::static_type(),
            glib::ParamFlags::READABLE,
        )
    }),
    subclass::Property("socket-v6", |name| {
        glib::ParamSpec::object(
            name,
            "Socket V6",
            "IPV6 Socket to use for UDP transmission. (None == allocate)",
            gio::Socket::static_type(),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("used-socket-v6", |name| {
        glib::ParamSpec::object(
            name,
            "Used Socket V6",
            "V6 Socket currently in use for UDP transmission. (None = no socket)",
            gio::Socket::static_type(),
            glib::ParamFlags::READABLE,
        )
    }),
    subclass::Property("auto-multicast", |name| {
        glib::ParamSpec::boolean(
            name,
            "Auto multicast",
            "Automatically join/leave the multicast groups, FALSE means user has to do it himself",
            DEFAULT_AUTO_MULTICAST,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("loop", |name| {
        glib::ParamSpec::boolean(
            name,
            "Loop",
            "Set the multicast loop parameter.",
            DEFAULT_LOOP,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("ttl", |name| {
        glib::ParamSpec::uint(
            name,
            "Time To Live",
            "Used for setting the unicast TTL parameter",
            0,
            u8::MAX as u32,
            DEFAULT_TTL,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("ttl-mc", |name| {
        glib::ParamSpec::uint(
            name,
            "Time To Live Multicast",
            "Used for setting the multicast TTL parameter",
            0,
            u8::MAX as u32,
            DEFAULT_TTL_MC,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("qos-dscp", |name| {
        glib::ParamSpec::int(
            name,
            "QoS DSCP",
            "Quality of Service, differentiated services code point (-1 default)",
            -1,
            63,
            DEFAULT_QOS_DSCP,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("clients", |name| {
        glib::ParamSpec::string(
            name,
            "Clients",
            "A comma separated list of host:port pairs with destinations",
            Some(DEFAULT_CLIENTS),
            glib::ParamFlags::READWRITE,
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

#[derive(Debug)]
enum TaskItem {
    Buffer(gst::Buffer),
    Event(gst::Event),
}

#[derive(Debug)]
struct UdpSinkPadHandlerInner {
    sync: bool,
    segment: Option<gst::Segment>,
    latency: gst::ClockTime,
    socket: Arc<Mutex<Option<tokio::net::UdpSocket>>>,
    socket_v6: Arc<Mutex<Option<tokio::net::UdpSocket>>>,
    clients: Arc<Vec<SocketAddr>>,
    clients_to_configure: Vec<SocketAddr>,
    clients_to_unconfigure: Vec<SocketAddr>,
    sender: Arc<Mutex<Option<mpsc::Sender<TaskItem>>>>,
    settings: Arc<StdMutex<Settings>>,
}

impl UdpSinkPadHandlerInner {
    fn new(settings: Arc<StdMutex<Settings>>) -> Self {
        UdpSinkPadHandlerInner {
            sync: DEFAULT_SYNC,
            segment: None,
            latency: gst::CLOCK_TIME_NONE,
            socket: Arc::new(Mutex::new(None)),
            socket_v6: Arc::new(Mutex::new(None)),
            clients: Arc::new(vec![SocketAddr::new(
                DEFAULT_HOST.unwrap().parse().unwrap(),
                DEFAULT_PORT as u16,
            )]),
            clients_to_configure: vec![],
            clients_to_unconfigure: vec![],
            sender: Arc::new(Mutex::new(None)),
            settings,
        }
    }

    fn clear_clients(
        &mut self,
        gst_pad: &gst::Pad,
        clients_to_add: impl Iterator<Item = SocketAddr>,
    ) {
        let old_clients = mem::replace(&mut *Arc::make_mut(&mut self.clients), vec![]);

        self.clients_to_configure = vec![];
        self.clients_to_unconfigure = vec![];

        for addr in clients_to_add {
            if !old_clients.contains(&addr) {
                self.clients_to_unconfigure.push(addr);
            }
            self.add_client(gst_pad, addr);
        }
    }

    fn remove_client(&mut self, gst_pad: &gst::Pad, addr: SocketAddr) {
        if !self.clients.contains(&addr) {
            gst_warning!(CAT, obj: gst_pad, "Not removing unknown client {:?}", &addr);
            return;
        }

        gst_info!(CAT, obj: gst_pad, "Removing client {:?}", addr);

        Arc::make_mut(&mut self.clients).retain(|addr2| addr != *addr2);

        self.clients_to_unconfigure.push(addr);
        self.clients_to_configure.retain(|addr2| addr != *addr2);
    }

    fn replace_client(
        &mut self,
        gst_pad: &gst::Pad,
        addr: Option<SocketAddr>,
        new_addr: Option<SocketAddr>,
    ) {
        if let Some(addr) = addr {
            self.remove_client(gst_pad, addr);
        }

        if let Some(new_addr) = new_addr {
            self.add_client(gst_pad, new_addr);
        }
    }

    fn add_client(&mut self, gst_pad: &gst::Pad, addr: SocketAddr) {
        if self.clients.contains(&addr) {
            gst_warning!(CAT, obj: gst_pad, "Not adding client {:?} again", &addr);
            return;
        }

        gst_info!(CAT, obj: gst_pad, "Adding client {:?}", addr);

        Arc::make_mut(&mut self.clients).push(addr);

        self.clients_to_configure.push(addr);
        self.clients_to_unconfigure.retain(|addr2| addr != *addr2);
    }
}

#[derive(Debug)]
enum SocketQualified {
    Ipv4(tokio::net::UdpSocket),
    Ipv6(tokio::net::UdpSocket),
}

#[derive(Clone, Debug)]
struct UdpSinkPadHandler(Arc<RwLock<UdpSinkPadHandlerInner>>);

impl UdpSinkPadHandler {
    fn new(settings: Arc<StdMutex<Settings>>) -> UdpSinkPadHandler {
        Self(Arc::new(RwLock::new(UdpSinkPadHandlerInner::new(settings))))
    }

    fn set_latency(&self, latency: gst::ClockTime) {
        self.0.write().unwrap().latency = latency;
    }

    fn prepare(&self) {
        let mut inner = self.0.write().unwrap();
        inner.clients_to_configure = inner.clients.to_vec();
    }

    fn prepare_socket(&self, socket: SocketQualified) {
        let mut inner = self.0.write().unwrap();

        match socket {
            SocketQualified::Ipv4(socket) => inner.socket = Arc::new(Mutex::new(Some(socket))),
            SocketQualified::Ipv6(socket) => inner.socket_v6 = Arc::new(Mutex::new(Some(socket))),
        }
    }

    fn unprepare(&self) {
        let mut inner = self.0.write().unwrap();
        *inner = UdpSinkPadHandlerInner::new(Arc::clone(&inner.settings))
    }

    fn clear_clients(&self, gst_pad: &gst::Pad, clients_to_add: impl Iterator<Item = SocketAddr>) {
        self.0
            .write()
            .unwrap()
            .clear_clients(gst_pad, clients_to_add);
    }

    fn remove_client(&self, gst_pad: &gst::Pad, addr: SocketAddr) {
        self.0.write().unwrap().remove_client(gst_pad, addr);
    }

    fn replace_client(
        &self,
        gst_pad: &gst::Pad,
        addr: Option<SocketAddr>,
        new_addr: Option<SocketAddr>,
    ) {
        self.0
            .write()
            .unwrap()
            .replace_client(gst_pad, addr, new_addr);
    }

    fn add_client(&self, gst_pad: &gst::Pad, addr: SocketAddr) {
        self.0.write().unwrap().add_client(gst_pad, addr);
    }

    fn get_clients(&self) -> Vec<SocketAddr> {
        (*self.0.read().unwrap().clients).clone()
    }

    fn configure_client(
        &self,
        settings: &Settings,
        socket: &mut Option<tokio::net::UdpSocket>,
        socket_v6: &mut Option<tokio::net::UdpSocket>,
        client: &SocketAddr,
    ) -> Result<(), gst::ErrorMessage> {
        if client.ip().is_multicast() {
            match client.ip() {
                IpAddr::V4(addr) => {
                    if let Some(socket) = socket.as_mut() {
                        if settings.auto_multicast {
                            socket
                                .join_multicast_v4(addr, Ipv4Addr::new(0, 0, 0, 0))
                                .map_err(|err| {
                                    gst_error_msg!(
                                        gst::ResourceError::OpenWrite,
                                        ["Failed to join multicast group: {}", err]
                                    )
                                })?;
                        }
                        if settings.multicast_loop {
                            socket.set_multicast_loop_v4(true).map_err(|err| {
                                gst_error_msg!(
                                    gst::ResourceError::OpenWrite,
                                    ["Failed to set multicast loop: {}", err]
                                )
                            })?;
                        }
                        socket
                            .set_multicast_ttl_v4(settings.ttl_mc)
                            .map_err(|err| {
                                gst_error_msg!(
                                    gst::ResourceError::OpenWrite,
                                    ["Failed to set multicast ttl: {}", err]
                                )
                            })?;
                    }
                }
                IpAddr::V6(addr) => {
                    if let Some(socket) = socket_v6.as_mut() {
                        if settings.auto_multicast {
                            socket.join_multicast_v6(&addr, 0).map_err(|err| {
                                gst_error_msg!(
                                    gst::ResourceError::OpenWrite,
                                    ["Failed to join multicast group: {}", err]
                                )
                            })?;
                        }
                        if settings.multicast_loop {
                            socket.set_multicast_loop_v6(true).map_err(|err| {
                                gst_error_msg!(
                                    gst::ResourceError::OpenWrite,
                                    ["Failed to set multicast loop: {}", err]
                                )
                            })?;
                        }
                        /* FIXME no API for set_multicast_ttl_v6 ? */
                    }
                }
            }
        } else {
            match client.ip() {
                IpAddr::V4(_) => {
                    if let Some(socket) = socket.as_mut() {
                        socket.set_ttl(settings.ttl).map_err(|err| {
                            gst_error_msg!(
                                gst::ResourceError::OpenWrite,
                                ["Failed to set unicast ttl: {}", err]
                            )
                        })?;
                    }
                }
                IpAddr::V6(_) => {
                    if let Some(socket) = socket_v6.as_mut() {
                        socket.set_ttl(settings.ttl).map_err(|err| {
                            gst_error_msg!(
                                gst::ResourceError::OpenWrite,
                                ["Failed to set unicast ttl: {}", err]
                            )
                        })?;
                    }
                }
            }
        }

        Ok(())
    }

    fn unconfigure_client(
        &self,
        settings: &Settings,
        socket: &mut Option<tokio::net::UdpSocket>,
        socket_v6: &mut Option<tokio::net::UdpSocket>,
        client: &SocketAddr,
    ) -> Result<(), gst::ErrorMessage> {
        if client.ip().is_multicast() {
            match client.ip() {
                IpAddr::V4(addr) => {
                    if let Some(socket) = socket.as_mut() {
                        if settings.auto_multicast {
                            socket
                                .leave_multicast_v4(addr, Ipv4Addr::new(0, 0, 0, 0))
                                .map_err(|err| {
                                    gst_error_msg!(
                                        gst::ResourceError::OpenWrite,
                                        ["Failed to join multicast group: {}", err]
                                    )
                                })?;
                        }
                    }
                }
                IpAddr::V6(addr) => {
                    if let Some(socket) = socket_v6.as_mut() {
                        if settings.auto_multicast {
                            socket.leave_multicast_v6(&addr, 0).map_err(|err| {
                                gst_error_msg!(
                                    gst::ResourceError::OpenWrite,
                                    ["Failed to join multicast group: {}", err]
                                )
                            })?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn render(
        &self,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (
            do_sync,
            rtime,
            clients,
            clients_to_configure,
            clients_to_unconfigure,
            socket,
            socket_v6,
            settings,
        ) = {
            let mut inner = self.0.write().unwrap();
            let do_sync = inner.sync;
            let mut rtime: gst::ClockTime = 0.into();

            if let Some(segment) = &inner.segment {
                if let Some(segment) = segment.downcast_ref::<gst::format::Time>() {
                    rtime = segment.to_running_time(buffer.get_pts());
                    if inner.latency.is_some() {
                        rtime += inner.latency;
                    }
                }
            }

            let clients_to_configure = mem::replace(&mut inner.clients_to_configure, vec![]);
            let clients_to_unconfigure = mem::replace(&mut inner.clients_to_unconfigure, vec![]);

            let settings = inner.settings.lock().unwrap().clone();

            (
                do_sync,
                rtime,
                Arc::clone(&inner.clients),
                clients_to_configure,
                clients_to_unconfigure,
                Arc::clone(&inner.socket),
                Arc::clone(&inner.socket_v6),
                settings,
            )
        };

        let mut socket = socket.lock().await;
        let mut socket_v6 = socket_v6.lock().await;

        if !clients_to_configure.is_empty() {
            for client in &clients_to_configure {
                self.configure_client(&settings, &mut socket, &mut socket_v6, &client)
                    .map_err(|err| {
                        gst_element_error!(
                            element,
                            gst::StreamError::Failed,
                            ["Failed to configure client {:?}: {}", client, err]
                        );

                        gst::FlowError::Error
                    })?;
            }
        }

        if !clients_to_unconfigure.is_empty() {
            for client in &clients_to_unconfigure {
                self.unconfigure_client(&settings, &mut socket, &mut socket_v6, &client)
                    .map_err(|err| {
                        gst_element_error!(
                            element,
                            gst::StreamError::Failed,
                            ["Failed to unconfigure client {:?}: {}", client, err]
                        );

                        gst::FlowError::Error
                    })?;
            }
        }

        if do_sync {
            self.sync(&element, rtime).await;
        }

        let data = buffer.map_readable().map_err(|_| {
            gst_element_error!(
                element,
                gst::StreamError::Format,
                ["Failed to map buffer readable"]
            );

            gst::FlowError::Error
        })?;

        for client in clients.iter() {
            let socket = match client.ip() {
                IpAddr::V4(_) => &mut socket,
                IpAddr::V6(_) => &mut socket_v6,
            };

            if let Some(socket) = socket.as_mut() {
                gst_log!(CAT, obj: element, "Sending to {:?}", &client);
                socket.send_to(&data, client).await.map_err(|err| {
                    gst_element_error!(
                        element,
                        gst::StreamError::Failed,
                        ("I/O error"),
                        ["streaming stopped, I/O error {}", err]
                    );
                    gst::FlowError::Error
                })?;
            } else {
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ("I/O error"),
                    ["No socket available for sending to {}", client]
                );
                return Err(gst::FlowError::Error);
            }
        }

        gst_log!(
            CAT,
            obj: element,
            "Sent buffer {:?} to all clients",
            &buffer
        );

        Ok(gst::FlowSuccess::Ok)
    }

    /* Wait until specified time */
    async fn sync(&self, element: &gst::Element, running_time: gst::ClockTime) {
        let now = super::get_current_running_time(&element);

        if now < running_time {
            let delay = running_time - now;
            runtime::time::delay_for(Duration::from_nanos(delay.nseconds().unwrap())).await;
        }
    }

    async fn handle_event(&self, element: &gst::Element, event: gst::Event) {
        match event.view() {
            EventView::Eos(_) => {
                let _ = element.post_message(&gst::Message::new_eos().src(Some(element)).build());
            }
            EventView::Segment(e) => {
                self.0.write().unwrap().segment = Some(e.get_segment().clone());
            }
            _ => (),
        }
    }
}

impl PadSinkHandler for UdpSinkPadHandler {
    type ElementImpl = UdpSink;

    fn sink_chain(
        &self,
        _pad: &PadSinkRef,
        _udpsink: &UdpSink,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let sender = Arc::clone(&self.0.read().unwrap().sender);
        let element = element.clone();

        async move {
            if let Some(sender) = sender.lock().await.as_mut() {
                if let Err(_) = sender.send(TaskItem::Buffer(buffer)).await {
                    gst_debug!(CAT, obj: &element, "Flushing");
                    return Err(gst::FlowError::Flushing);
                }
            }
            Ok(gst::FlowSuccess::Ok)
        }
        .boxed()
    }

    fn sink_chain_list(
        &self,
        _pad: &PadSinkRef,
        _udpsink: &UdpSink,
        element: &gst::Element,
        list: gst::BufferList,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let sender = Arc::clone(&self.0.read().unwrap().sender);
        let element = element.clone();

        async move {
            if let Some(sender) = sender.lock().await.as_mut() {
                for buffer in list.iter_owned() {
                    if let Err(_) = sender.send(TaskItem::Buffer(buffer)).await {
                        gst_debug!(CAT, obj: &element, "Flushing");
                        return Err(gst::FlowError::Flushing);
                    }
                }
            }

            Ok(gst::FlowSuccess::Ok)
        }
        .boxed()
    }

    fn sink_event_serialized(
        &self,
        _pad: &PadSinkRef,
        _udpsink: &UdpSink,
        element: &gst::Element,
        event: gst::Event,
    ) -> BoxFuture<'static, bool> {
        let sender = Arc::clone(&self.0.read().unwrap().sender);
        let element = element.clone();

        async move {
            if let EventView::FlushStop(_) = event.view() {
                let udpsink = UdpSink::from_instance(&element);
                let _ = udpsink.start(&element);
            } else if let Some(sender) = sender.lock().await.as_mut() {
                if let Err(_) = sender.send(TaskItem::Event(event)).await {
                    gst_debug!(CAT, obj: &element, "Flushing");
                }
            }

            true
        }
        .boxed()
    }

    fn sink_event(
        &self,
        _pad: &PadSinkRef,
        udpsink: &UdpSink,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        if let EventView::FlushStart(..) = event.view() {
            let _ = udpsink.stop(&element);
        }

        true
    }
}

#[derive(Debug)]
enum SocketFamily {
    Ipv4,
    Ipv6,
}

#[derive(Debug)]
struct UdpSink {
    sink_pad: PadSink,
    sink_pad_handler: UdpSinkPadHandler,
    task: Task,
    settings: Arc<StdMutex<Settings>>,
}

impl UdpSink {
    fn prepare_socket(
        &self,
        family: SocketFamily,
        context: &Context,
        element: &gst::Element,
    ) -> Result<(), gst::ErrorMessage> {
        let mut settings = self.settings.lock().unwrap();

        let wrapped_socket = match family {
            SocketFamily::Ipv4 => &settings.socket,
            SocketFamily::Ipv6 => &settings.socket_v6,
        };

        let socket_qualified: SocketQualified;

        if let Some(ref wrapped_socket) = wrapped_socket {
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
                        gst::ResourceError::OpenWrite,
                        ["Failed to setup socket for tokio: {}", err]
                    )
                })
            })?;

            match family {
                SocketFamily::Ipv4 => {
                    settings.used_socket = Some(wrapped_socket.clone());
                    socket_qualified = SocketQualified::Ipv4(socket);
                }
                SocketFamily::Ipv6 => {
                    settings.used_socket_v6 = Some(wrapped_socket.clone());
                    socket_qualified = SocketQualified::Ipv6(socket);
                }
            }
        } else {
            let bind_addr = match family {
                SocketFamily::Ipv4 => &settings.bind_address,
                SocketFamily::Ipv6 => &settings.bind_address_v6,
            };

            let bind_addr: IpAddr = bind_addr.parse().map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::Settings,
                    ["Invalid address '{}' set: {}", bind_addr, err]
                )
            })?;

            let bind_port = match family {
                SocketFamily::Ipv4 => settings.bind_port,
                SocketFamily::Ipv6 => settings.bind_port_v6,
            };

            let saddr = SocketAddr::new(bind_addr, bind_port as u16);
            gst_debug!(CAT, obj: element, "Binding to {:?}", saddr);

            let builder = match family {
                SocketFamily::Ipv4 => net2::UdpBuilder::new_v4(),
                SocketFamily::Ipv6 => net2::UdpBuilder::new_v6(),
            };

            let builder = match builder {
                Ok(builder) => builder,
                Err(err) => {
                    gst_warning!(
                        CAT,
                        obj: element,
                        "Failed to create {} socket builder: {}",
                        match family {
                            SocketFamily::Ipv4 => "IPv4",
                            SocketFamily::Ipv6 => "IPv6",
                        },
                        err
                    );
                    return Ok(());
                }
            };

            let socket = builder.bind(&saddr).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to bind socket: {}", err]
                )
            })?;

            let socket = context.enter(|| {
                tokio::net::UdpSocket::from_std(socket).map_err(|err| {
                    gst_error_msg!(
                        gst::ResourceError::OpenWrite,
                        ["Failed to setup socket for tokio: {}", err]
                    )
                })
            })?;

            let wrapper = wrap_socket(&socket)?;

            if settings.qos_dscp != -1 {
                wrapper.set_tos(settings.qos_dscp).map_err(|err| {
                    gst_error_msg!(
                        gst::ResourceError::OpenWrite,
                        ["Failed to set QoS DSCP: {}", err]
                    )
                })?;
            }

            match family {
                SocketFamily::Ipv4 => {
                    settings.used_socket = Some(wrapper);
                    socket_qualified = SocketQualified::Ipv4(socket)
                }
                SocketFamily::Ipv6 => {
                    settings.used_socket_v6 = Some(wrapper);
                    socket_qualified = SocketQualified::Ipv6(socket)
                }
            }
        }

        self.sink_pad_handler.prepare_socket(socket_qualified);

        Ok(())
    }

    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Preparing");

        let context = {
            let settings = self.settings.lock().unwrap();

            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to acquire Context: {}", err]
                )
            })?
        };

        self.sink_pad_handler.prepare();
        self.prepare_socket(SocketFamily::Ipv4, &context, element)?;
        self.prepare_socket(SocketFamily::Ipv6, &context, element)?;

        self.task.prepare(context).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Error preparing Task: {:?}", err]
            )
        })?;

        self.sink_pad.prepare(&self.sink_pad_handler);

        gst_debug!(CAT, obj: element, "Started preparing");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Unpreparing");

        self.task.unprepare().unwrap();
        self.sink_pad_handler.unprepare();
        self.sink_pad.unprepare();

        gst_debug!(CAT, obj: element, "Unprepared");

        Ok(())
    }

    fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Stopping");
        self.task.stop();
        gst_debug!(CAT, obj: element, "Stopped");

        Ok(())
    }

    fn start(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Starting");

        let sink_pad_handler = self.sink_pad_handler.clone();
        let element_clone = element.clone();

        let (sender, receiver) = mpsc::channel(0);
        let receiver = Arc::new(Mutex::new(receiver));

        sink_pad_handler.0.write().unwrap().sender = Arc::new(Mutex::new(Some(sender)));

        self.task.start(move || {
            let receiver = Arc::clone(&receiver);
            let element = element_clone.clone();
            let sink_pad_handler = sink_pad_handler.clone();

            async move {
                match receiver.lock().await.next().await {
                    Some(TaskItem::Buffer(buffer)) => {
                        match sink_pad_handler.render(&element, buffer).await {
                            Err(err) => {
                                gst_element_error!(
                                    element,
                                    gst::StreamError::Failed,
                                    ["Failed to render item, stopping task: {}", err]
                                );

                                glib::Continue(false)
                            }
                            _ => glib::Continue(true),
                        }
                    }
                    Some(TaskItem::Event(event)) => {
                        sink_pad_handler.handle_event(&element, event).await;
                        glib::Continue(true)
                    }
                    None => glib::Continue(false),
                }
            }
        });

        Ok(())
    }
}

impl UdpSink {
    fn clear_clients(&self, clients_to_add: impl Iterator<Item = SocketAddr>) {
        self.sink_pad_handler
            .clear_clients(&self.sink_pad.gst_pad(), clients_to_add);
    }

    fn remove_client(&self, addr: SocketAddr) {
        self.sink_pad_handler
            .remove_client(&self.sink_pad.gst_pad(), addr);
    }

    fn replace_client(&self, addr: Option<SocketAddr>, new_addr: Option<SocketAddr>) {
        self.sink_pad_handler
            .replace_client(&self.sink_pad.gst_pad(), addr, new_addr);
    }

    fn add_client(&self, addr: SocketAddr) {
        self.sink_pad_handler
            .add_client(&self.sink_pad.gst_pad(), addr);
    }
}

fn try_into_socket_addr(element: &gst::Element, host: &str, port: u32) -> Result<SocketAddr, ()> {
    let addr: IpAddr = match host.parse() {
        Err(err) => {
            gst_error!(CAT, obj: element, "Failed to parse host {}: {}", host, err);
            return Err(());
        }
        Ok(addr) => addr,
    };

    let port: u16 = match port.try_into() {
        Err(err) => {
            gst_error!(CAT, obj: element, "Invalid port {}: {}", port, err);
            return Err(());
        }
        Ok(port) => port,
    };

    Ok(SocketAddr::new(addr, port))
}

impl ObjectSubclass for UdpSink {
    const NAME: &'static str = "RsTsUdpSink";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing UDP sink",
            "Sink/Network",
            "Thread-sharing UDP sink",
            "Mathieu <mathieu@centricular.com>",
        );

        let caps = gst::Caps::new_any();

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);
        klass.add_signal_with_class_handler(
            "add",
            glib::SignalFlags::RUN_LAST | glib::SignalFlags::ACTION,
            &[String::static_type(), i32::static_type()],
            glib::types::Type::Unit,
            |_, args| {
                let element = args[0]
                    .get::<gst::Element>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let host = args[1]
                    .get::<String>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let port = args[2]
                    .get::<i32>()
                    .expect("signal arg")
                    .expect("missing signal arg") as u32;

                if let Ok(addr) = try_into_socket_addr(&element, &host, port) {
                    let udpsink = Self::from_instance(&element);
                    udpsink.add_client(addr);
                }

                None
            },
        );

        klass.add_signal_with_class_handler(
            "remove",
            glib::SignalFlags::RUN_LAST | glib::SignalFlags::ACTION,
            &[String::static_type(), i32::static_type()],
            glib::types::Type::Unit,
            |_, args| {
                let element = args[0]
                    .get::<gst::Element>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let host = args[1]
                    .get::<String>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let port = args[2]
                    .get::<i32>()
                    .expect("signal arg")
                    .expect("missing signal arg") as u32;

                let udpsink = Self::from_instance(&element);
                let settings = udpsink.settings.lock().unwrap();

                if Some(&host) != settings.host.as_ref() || port != settings.port {
                    if let Ok(addr) = try_into_socket_addr(&element, &host, port) {
                        udpsink.remove_client(addr);
                    }
                }

                None
            },
        );

        klass.add_signal_with_class_handler(
            "clear",
            glib::SignalFlags::RUN_LAST | glib::SignalFlags::ACTION,
            &[],
            glib::types::Type::Unit,
            |_, args| {
                let element = args[0]
                    .get::<gst::Element>()
                    .expect("signal arg")
                    .expect("missing signal arg");

                let udpsink = Self::from_instance(&element);
                let settings = udpsink.settings.lock().unwrap();
                let current_client = settings
                    .host
                    .iter()
                    .filter_map(|host| try_into_socket_addr(&element, host, settings.port).ok());

                udpsink.clear_clients(current_client);

                None
            },
        );

        klass.install_properties(&PROPERTIES);
    }

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let settings = Arc::new(StdMutex::new(Settings::default()));

        Self {
            sink_pad: PadSink::new(gst::Pad::new_from_template(
                &klass.get_pad_template("sink").unwrap(),
                Some("sink"),
            )),
            sink_pad_handler: UdpSinkPadHandler::new(Arc::clone(&settings)),
            task: Task::default(),
            settings,
        }
    }
}

impl ObjectImpl for UdpSink {
    glib_object_impl!();

    fn set_property(&self, obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        let element = obj.downcast_ref::<gst::Element>().unwrap();

        let mut settings = self.settings.lock().unwrap();
        match *prop {
            subclass::Property("host", ..) => {
                let current_client = settings
                    .host
                    .as_ref()
                    .and_then(|host| try_into_socket_addr(&element, host, settings.port).ok());

                let new_host = value.get().expect("type checked upstream");

                let new_client = new_host
                    .and_then(|host| try_into_socket_addr(&element, host, settings.port).ok());

                self.replace_client(current_client, new_client);

                settings.host = new_host.map(ToString::to_string);
            }
            subclass::Property("port", ..) => {
                let current_client = settings
                    .host
                    .as_ref()
                    .and_then(|host| try_into_socket_addr(&element, host, settings.port).ok());

                let new_port = value.get_some().expect("type checked upstream");

                let new_client = settings
                    .host
                    .as_ref()
                    .and_then(|host| try_into_socket_addr(&element, host, new_port).ok());

                self.replace_client(current_client, new_client);

                settings.port = new_port;
            }
            subclass::Property("sync", ..) => {
                settings.sync = value.get_some().expect("type checked upstream");
            }
            subclass::Property("bind-address", ..) => {
                settings.bind_address = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("bind-port", ..) => {
                settings.bind_port = value.get_some().expect("type checked upstream");
            }
            subclass::Property("bind-address-v6", ..) => {
                settings.bind_address_v6 = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("bind-port-v6", ..) => {
                settings.bind_port_v6 = value.get_some().expect("type checked upstream");
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
            subclass::Property("socket-v6", ..) => {
                settings.socket_v6 = value
                    .get::<gio::Socket>()
                    .expect("type checked upstream")
                    .map(|socket| GioSocketWrapper::new(&socket));
            }
            subclass::Property("used-socket-v6", ..) => {
                unreachable!();
            }
            subclass::Property("auto-multicast", ..) => {
                settings.auto_multicast = value.get_some().expect("type checked upstream");
            }
            subclass::Property("loop", ..) => {
                settings.multicast_loop = value.get_some().expect("type checked upstream");
            }
            subclass::Property("ttl", ..) => {
                settings.ttl = value.get_some().expect("type checked upstream");
            }
            subclass::Property("ttl-mc", ..) => {
                settings.ttl_mc = value.get_some().expect("type checked upstream");
            }
            subclass::Property("qos-dscp", ..) => {
                settings.qos_dscp = value.get_some().expect("type checked upstream");
            }
            subclass::Property("clients", ..) => {
                let clients: String = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());

                let current_client = settings
                    .host
                    .iter()
                    .filter_map(|host| try_into_socket_addr(&element, host, settings.port).ok());

                let clients_iter = current_client.chain(clients.split(',').filter_map(|client| {
                    let rsplit: Vec<&str> = client.rsplitn(2, ':').collect();

                    if rsplit.len() == 2 {
                        rsplit[0]
                            .parse::<u32>()
                            .map_err(|err| {
                                gst_error!(
                                    CAT,
                                    obj: element,
                                    "Invalid port {}: {}",
                                    rsplit[0],
                                    err
                                );
                            })
                            .and_then(|port| try_into_socket_addr(&element, rsplit[1], port))
                            .ok()
                    } else {
                        None
                    }
                }));

                self.clear_clients(clients_iter);
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
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        let settings = self.settings.lock().unwrap();
        match *prop {
            subclass::Property("host", ..) => Ok(settings.host.to_value()),
            subclass::Property("port", ..) => Ok(settings.port.to_value()),
            subclass::Property("sync", ..) => Ok(settings.sync.to_value()),
            subclass::Property("bind-address", ..) => Ok(settings.bind_address.to_value()),
            subclass::Property("bind-port", ..) => Ok(settings.bind_port.to_value()),
            subclass::Property("bind-address-v6", ..) => Ok(settings.bind_address_v6.to_value()),
            subclass::Property("bind-port-v6", ..) => Ok(settings.bind_port_v6.to_value()),
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
            subclass::Property("socket-v6", ..) => Ok(settings
                .socket_v6
                .as_ref()
                .map(GioSocketWrapper::as_socket)
                .to_value()),
            subclass::Property("used-socket-v6", ..) => Ok(settings
                .used_socket_v6
                .as_ref()
                .map(GioSocketWrapper::as_socket)
                .to_value()),
            subclass::Property("auto-multicast", ..) => Ok(settings.sync.to_value()),
            subclass::Property("loop", ..) => Ok(settings.multicast_loop.to_value()),
            subclass::Property("ttl", ..) => Ok(settings.ttl.to_value()),
            subclass::Property("ttl-mc", ..) => Ok(settings.ttl_mc.to_value()),
            subclass::Property("qos-dscp", ..) => Ok(settings.qos_dscp.to_value()),
            subclass::Property("clients", ..) => {
                let clients: Vec<String> = self
                    .sink_pad_handler
                    .get_clients()
                    .iter()
                    .map(ToString::to_string)
                    .collect();

                Ok(clients.join(",").to_value())
            }
            subclass::Property("context", ..) => Ok(settings.context.to_value()),
            subclass::Property("context-wait", ..) => Ok(settings.context_wait.to_value()),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(self.sink_pad.gst_pad()).unwrap();

        super::set_element_flags(element, gst::ElementFlags::SINK);
    }
}

impl ElementImpl for UdpSink {
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
            gst::StateChange::ReadyToPaused => {
                self.start(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PausedToReady => {
                self.stop(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }

    fn send_event(&self, _element: &gst::Element, event: gst::Event) -> bool {
        match event.view() {
            EventView::Latency(ev) => {
                self.sink_pad_handler.set_latency(ev.get_latency());
                self.sink_pad.gst_pad().push_event(event)
            }
            EventView::Step(..) => false,
            _ => self.sink_pad.gst_pad().push_event(event),
        }
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-udpsink",
        gst::Rank::None,
        UdpSink::get_type(),
    )
}
