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

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::EventView;
use gst::{
    element_error, error_msg, gst_debug, gst_error, gst_info, gst_log, gst_trace, gst_warning,
};

use once_cell::sync::Lazy;

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
const DEFAULT_PORT: i32 = 5004;
const DEFAULT_SYNC: bool = true;
const DEFAULT_BIND_ADDRESS: &str = "0.0.0.0";
const DEFAULT_BIND_PORT: i32 = 0;
const DEFAULT_BIND_ADDRESS_V6: &str = "::";
const DEFAULT_BIND_PORT_V6: i32 = 0;
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
// FIXME use Duration::ZERO when MSVC >= 1.53.2
const DEFAULT_CONTEXT_WAIT: Duration = Duration::from_nanos(0);

#[derive(Debug, Clone)]
struct Settings {
    sync: bool,
    bind_address: String,
    bind_port: i32,
    bind_address_v6: String,
    bind_port_v6: i32,
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
    context_wait: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
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

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-udpsink",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing UDP sink"),
    )
});

#[derive(Debug)]
enum TaskItem {
    Buffer(gst::Buffer),
    Event(gst::Event),
}

#[derive(Debug)]
struct UdpSinkPadHandlerInner {
    sync: bool,
    segment: Option<gst::Segment>,
    latency: Option<gst::ClockTime>,
    socket: Arc<Mutex<Option<tokio::net::UdpSocket>>>,
    socket_v6: Arc<Mutex<Option<tokio::net::UdpSocket>>>,
    #[allow(clippy::rc_buffer)]
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
            latency: None,
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
        let old_clients = mem::take(&mut *Arc::make_mut(&mut self.clients));

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
        self.0.write().unwrap().latency = Some(latency);
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

    fn add_client(&self, gst_pad: &gst::Pad, addr: SocketAddr) {
        self.0.write().unwrap().add_client(gst_pad, addr);
    }

    fn clients(&self) -> Vec<SocketAddr> {
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
                                    error_msg!(
                                        gst::ResourceError::OpenWrite,
                                        ["Failed to join multicast group: {}", err]
                                    )
                                })?;
                        }
                        if settings.multicast_loop {
                            socket.set_multicast_loop_v4(true).map_err(|err| {
                                error_msg!(
                                    gst::ResourceError::OpenWrite,
                                    ["Failed to set multicast loop: {}", err]
                                )
                            })?;
                        }
                        socket
                            .set_multicast_ttl_v4(settings.ttl_mc)
                            .map_err(|err| {
                                error_msg!(
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
                                error_msg!(
                                    gst::ResourceError::OpenWrite,
                                    ["Failed to join multicast group: {}", err]
                                )
                            })?;
                        }
                        if settings.multicast_loop {
                            socket.set_multicast_loop_v6(true).map_err(|err| {
                                error_msg!(
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
                            error_msg!(
                                gst::ResourceError::OpenWrite,
                                ["Failed to set unicast ttl: {}", err]
                            )
                        })?;
                    }
                }
                IpAddr::V6(_) => {
                    if let Some(socket) = socket_v6.as_mut() {
                        socket.set_ttl(settings.ttl).map_err(|err| {
                            error_msg!(
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
                                    error_msg!(
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
                                error_msg!(
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
        element: &super::UdpSink,
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
            let mut rtime = gst::ClockTime::NONE;

            if let Some(segment) = &inner.segment {
                rtime = segment
                    .downcast_ref::<gst::format::Time>()
                    .and_then(|segment| {
                        segment
                            .to_running_time(buffer.pts())
                            .zip(inner.latency)
                            .map(|(rtime, latency)| rtime + latency)
                    });
            }

            let clients_to_configure = mem::take(&mut inner.clients_to_configure);
            let clients_to_unconfigure = mem::take(&mut inner.clients_to_unconfigure);

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
                        element_error!(
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
                        element_error!(
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
            element_error!(
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
                    element_error!(
                        element,
                        gst::StreamError::Failed,
                        ("I/O error"),
                        ["streaming stopped, I/O error {}", err]
                    );
                    gst::FlowError::Error
                })?;
            } else {
                element_error!(
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
    async fn sync(
        &self,
        element: &super::UdpSink,
        running_time: impl Into<Option<gst::ClockTime>>,
    ) {
        let now = element.current_running_time();

        match running_time
            .into()
            .zip(now)
            .and_then(|(running_time, now)| running_time.checked_sub(now))
        {
            Some(delay) => runtime::time::delay_for(delay.into()).await,
            None => runtime::executor::yield_now().await,
        }
    }

    async fn handle_event(&self, element: &super::UdpSink, event: gst::Event) {
        match event.view() {
            EventView::Eos(_) => {
                let _ = element.post_message(gst::message::Eos::builder().src(element).build());
            }
            EventView::Segment(e) => {
                self.0.write().unwrap().segment = Some(e.segment().clone());
            }
            EventView::SinkMessage(e) => {
                let _ = element.post_message(e.message());
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
        let element = element.clone().downcast::<super::UdpSink>().unwrap();

        async move {
            if let Some(sender) = sender.lock().await.as_mut() {
                if sender.send(TaskItem::Buffer(buffer)).await.is_err() {
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
        let element = element.clone().downcast::<super::UdpSink>().unwrap();

        async move {
            if let Some(sender) = sender.lock().await.as_mut() {
                for buffer in list.iter_owned() {
                    if sender.send(TaskItem::Buffer(buffer)).await.is_err() {
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
        let element = element.clone().downcast::<super::UdpSink>().unwrap();

        async move {
            if let EventView::FlushStop(_) = event.view() {
                let udpsink = UdpSink::from_instance(&element);
                return udpsink.task.flush_stop().is_ok();
            } else if let Some(sender) = sender.lock().await.as_mut() {
                if sender.send(TaskItem::Event(event)).await.is_err() {
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
        _element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        if let EventView::FlushStart(..) = event.view() {
            return udpsink.task.flush_start().is_ok();
        }

        true
    }
}

#[derive(Debug)]
struct UdpSinkTask {
    element: super::UdpSink,
    sink_pad_handler: UdpSinkPadHandler,
    receiver: Option<mpsc::Receiver<TaskItem>>,
}

impl UdpSinkTask {
    fn new(element: &super::UdpSink, sink_pad_handler: &UdpSinkPadHandler) -> Self {
        UdpSinkTask {
            element: element.clone(),
            sink_pad_handler: sink_pad_handler.clone(),
            receiver: None,
        }
    }
}

impl TaskImpl for UdpSinkTask {
    fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Starting task");

            let (sender, receiver) = mpsc::channel(0);

            let mut sink_pad_handler = self.sink_pad_handler.0.write().unwrap();
            sink_pad_handler.sender = Arc::new(Mutex::new(Some(sender)));

            self.receiver = Some(receiver);

            gst_log!(CAT, obj: &self.element, "Task started");
            Ok(())
        }
        .boxed()
    }

    fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            match self.receiver.as_mut().unwrap().next().await {
                Some(TaskItem::Buffer(buffer)) => {
                    match self.sink_pad_handler.render(&self.element, buffer).await {
                        Err(err) => {
                            element_error!(
                                &self.element,
                                gst::StreamError::Failed,
                                ["Failed to render item, stopping task: {}", err]
                            );

                            Err(gst::FlowError::Error)
                        }
                        _ => Ok(()),
                    }
                }
                Some(TaskItem::Event(event)) => {
                    self.sink_pad_handler
                        .handle_event(&self.element, event)
                        .await;
                    Ok(())
                }
                None => Err(gst::FlowError::Flushing),
            }
        }
        .boxed()
    }
}

#[derive(Debug)]
enum SocketFamily {
    Ipv4,
    Ipv6,
}

#[derive(Debug)]
pub struct UdpSink {
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
        element: &super::UdpSink,
    ) -> Result<(), gst::ErrorMessage> {
        let mut settings = self.settings.lock().unwrap();

        let wrapped_socket = match family {
            SocketFamily::Ipv4 => &settings.socket,
            SocketFamily::Ipv6 => &settings.socket_v6,
        };

        let socket_qualified: SocketQualified;

        if let Some(ref wrapped_socket) = wrapped_socket {
            let socket = wrapped_socket.get();

            let socket = context.enter(|| {
                tokio::net::UdpSocket::from_std(socket).map_err(|err| {
                    error_msg!(
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
                error_msg!(
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

            let socket = match family {
                SocketFamily::Ipv4 => socket2::Socket::new(
                    socket2::Domain::IPV4,
                    socket2::Type::DGRAM,
                    Some(socket2::Protocol::UDP),
                ),
                SocketFamily::Ipv6 => socket2::Socket::new(
                    socket2::Domain::IPV6,
                    socket2::Type::DGRAM,
                    Some(socket2::Protocol::UDP),
                ),
            };

            let socket = match socket {
                Ok(socket) => socket,
                Err(err) => {
                    gst_warning!(
                        CAT,
                        obj: element,
                        "Failed to create {} socket: {}",
                        match family {
                            SocketFamily::Ipv4 => "IPv4",
                            SocketFamily::Ipv6 => "IPv6",
                        },
                        err
                    );
                    return Ok(());
                }
            };

            socket.bind(&saddr.into()).map_err(|err| {
                error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to bind socket: {}", err]
                )
            })?;

            let socket = context.enter(|| {
                tokio::net::UdpSocket::from_std(socket.into()).map_err(|err| {
                    error_msg!(
                        gst::ResourceError::OpenWrite,
                        ["Failed to setup socket for tokio: {}", err]
                    )
                })
            })?;

            let wrapper = wrap_socket(&socket)?;

            if settings.qos_dscp != -1 {
                wrapper.set_tos(settings.qos_dscp).map_err(|err| {
                    error_msg!(
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

    fn prepare(&self, element: &super::UdpSink) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Preparing");

        let context = {
            let settings = self.settings.lock().unwrap();

            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to acquire Context: {}", err]
                )
            })?
        };

        self.sink_pad_handler.prepare();
        self.prepare_socket(SocketFamily::Ipv4, &context, element)?;
        self.prepare_socket(SocketFamily::Ipv6, &context, element)?;

        self.task
            .prepare(UdpSinkTask::new(&element, &self.sink_pad_handler), context)
            .map_err(|err| {
                error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing Task: {:?}", err]
                )
            })?;

        gst_debug!(CAT, obj: element, "Started preparing");

        Ok(())
    }

    fn unprepare(&self, element: &super::UdpSink) {
        gst_debug!(CAT, obj: element, "Unpreparing");

        self.task.unprepare().unwrap();
        self.sink_pad_handler.unprepare();

        gst_debug!(CAT, obj: element, "Unprepared");
    }

    fn stop(&self, element: &super::UdpSink) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Stopping");
        self.task.stop()?;
        gst_debug!(CAT, obj: element, "Stopped");
        Ok(())
    }

    fn start(&self, element: &super::UdpSink) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Starting");
        self.task.start()?;
        gst_debug!(CAT, obj: element, "Started");
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

    fn add_client(&self, addr: SocketAddr) {
        self.sink_pad_handler
            .add_client(&self.sink_pad.gst_pad(), addr);
    }
}

fn try_into_socket_addr(element: &super::UdpSink, host: &str, port: i32) -> Result<SocketAddr, ()> {
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

#[glib::object_subclass]
impl ObjectSubclass for UdpSink {
    const NAME: &'static str = "RsTsUdpSink";
    type Type = super::UdpSink;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let settings = Arc::new(StdMutex::new(Settings::default()));
        let sink_pad_handler = UdpSinkPadHandler::new(Arc::clone(&settings));

        Self {
            sink_pad: PadSink::new(
                gst::Pad::from_template(&klass.pad_template("sink").unwrap(), Some("sink")),
                sink_pad_handler.clone(),
            ),
            sink_pad_handler,
            task: Task::default(),
            settings,
        }
    }
}

impl ObjectImpl for UdpSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
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
                glib::ParamSpec::new_boolean(
                    "sync",
                    "Sync",
                    "Sync on the clock",
                    DEFAULT_SYNC,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_string(
                    "bind-address",
                    "Bind Address",
                    "Address to bind the socket to",
                    Some(DEFAULT_BIND_ADDRESS),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_int(
                    "bind-port",
                    "Bind Port",
                    "Port to bind the socket to",
                    0,
                    u16::MAX as i32,
                    DEFAULT_BIND_PORT,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_string(
                    "bind-address-v6",
                    "Bind Address V6",
                    "Address to bind the V6 socket to",
                    Some(DEFAULT_BIND_ADDRESS_V6),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_int(
                    "bind-port-v6",
                    "Bind Port",
                    "Port to bind the V6 socket to",
                    0,
                    u16::MAX as i32,
                    DEFAULT_BIND_PORT_V6,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_object(
                    "socket",
                    "Socket",
                    "Socket to use for UDP transmission. (None == allocate)",
                    gio::Socket::static_type(),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_object(
                    "used-socket",
                    "Used Socket",
                    "Socket currently in use for UDP transmission. (None = no socket)",
                    gio::Socket::static_type(),
                    glib::ParamFlags::READABLE,
                ),
                glib::ParamSpec::new_object(
                    "socket-v6",
                    "Socket V6",
                    "IPV6 Socket to use for UDP transmission. (None == allocate)",
                    gio::Socket::static_type(),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_object(
                    "used-socket-v6",
                    "Used Socket V6",
                    "V6 Socket currently in use for UDP transmission. (None = no socket)",
                    gio::Socket::static_type(),
                    glib::ParamFlags::READABLE,
                ),
                glib::ParamSpec::new_boolean(
                    "auto-multicast",
                    "Auto multicast",
                    "Automatically join/leave the multicast groups, FALSE means user has to do it himself",
                    DEFAULT_AUTO_MULTICAST,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_boolean(
                    "loop",
                    "Loop",
                    "Set the multicast loop parameter.",
                    DEFAULT_LOOP,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "ttl",
                    "Time To Live",
                    "Used for setting the unicast TTL parameter",
                    0,
                    u8::MAX as u32,
                    DEFAULT_TTL,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "ttl-mc",
                    "Time To Live Multicast",
                    "Used for setting the multicast TTL parameter",
                    0,
                    u8::MAX as u32,
                    DEFAULT_TTL_MC,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_int(
                    "qos-dscp",
                    "QoS DSCP",
                    "Quality of Service, differentiated services code point (-1 default)",
                    -1,
                    63,
                    DEFAULT_QOS_DSCP,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_string(
                    "clients",
                    "Clients",
                    "A comma separated list of host:port pairs with destinations",
                    Some(DEFAULT_CLIENTS),
                    glib::ParamFlags::READWRITE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                glib::subclass::Signal::builder(
                    "add",
                    &[String::static_type().into(), i32::static_type().into()],
                    glib::types::Type::UNIT.into(),
                )
                .action()
                .class_handler(|_, args| {
                    let element = args[0].get::<super::UdpSink>().expect("signal arg");
                    let host = args[1].get::<String>().expect("signal arg");
                    let port = args[2].get::<i32>().expect("signal arg");

                    if let Ok(addr) = try_into_socket_addr(&element, &host, port) {
                        let udpsink = UdpSink::from_instance(&element);
                        udpsink.add_client(addr);
                    }

                    None
                })
                .build(),
                glib::subclass::Signal::builder(
                    "remove",
                    &[String::static_type().into(), i32::static_type().into()],
                    glib::types::Type::UNIT.into(),
                )
                .action()
                .class_handler(|_, args| {
                    let element = args[0].get::<super::UdpSink>().expect("signal arg");
                    let host = args[1].get::<String>().expect("signal arg");
                    let port = args[2].get::<i32>().expect("signal arg");

                    if let Ok(addr) = try_into_socket_addr(&element, &host, port) {
                        let udpsink = UdpSink::from_instance(&element);
                        udpsink.remove_client(addr);
                    }

                    None
                })
                .build(),
                glib::subclass::Signal::builder("clear", &[], glib::types::Type::UNIT.into())
                    .action()
                    .class_handler(|_, args| {
                        let element = args[0].get::<super::UdpSink>().expect("signal arg");

                        let udpsink = UdpSink::from_instance(&element);
                        udpsink.clear_clients(std::iter::empty());

                        None
                    })
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "sync" => {
                settings.sync = value.get().expect("type checked upstream");
            }
            "bind-address" => {
                settings.bind_address = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            "bind-port" => {
                settings.bind_port = value.get().expect("type checked upstream");
            }
            "bind-address-v6" => {
                settings.bind_address_v6 = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            "bind-port-v6" => {
                settings.bind_port_v6 = value.get().expect("type checked upstream");
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
            "socket-v6" => {
                settings.socket_v6 = value
                    .get::<Option<gio::Socket>>()
                    .expect("type checked upstream")
                    .map(|socket| GioSocketWrapper::new(&socket));
            }
            "used-socket-v6" => {
                unreachable!();
            }
            "auto-multicast" => {
                settings.auto_multicast = value.get().expect("type checked upstream");
            }
            "loop" => {
                settings.multicast_loop = value.get().expect("type checked upstream");
            }
            "ttl" => {
                settings.ttl = value.get().expect("type checked upstream");
            }
            "ttl-mc" => {
                settings.ttl_mc = value.get().expect("type checked upstream");
            }
            "qos-dscp" => {
                settings.qos_dscp = value.get().expect("type checked upstream");
            }
            "clients" => {
                let clients = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());

                let clients_iter = clients.split(',').filter_map(|client| {
                    let rsplit: Vec<&str> = client.rsplitn(2, ':').collect();

                    if rsplit.len() == 2 {
                        rsplit[0]
                            .parse::<i32>()
                            .map_err(|err| {
                                gst_error!(CAT, obj: obj, "Invalid port {}: {}", rsplit[0], err);
                            })
                            .and_then(|port| try_into_socket_addr(&obj, rsplit[1], port))
                            .ok()
                    } else {
                        None
                    }
                });
                drop(settings);

                self.clear_clients(clients_iter);
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
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "sync" => settings.sync.to_value(),
            "bind-address" => settings.bind_address.to_value(),
            "bind-port" => settings.bind_port.to_value(),
            "bind-address-v6" => settings.bind_address_v6.to_value(),
            "bind-port-v6" => settings.bind_port_v6.to_value(),
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
            "socket-v6" => settings
                .socket_v6
                .as_ref()
                .map(GioSocketWrapper::as_socket)
                .to_value(),
            "used-socket-v6" => settings
                .used_socket_v6
                .as_ref()
                .map(GioSocketWrapper::as_socket)
                .to_value(),
            "auto-multicast" => settings.sync.to_value(),
            "loop" => settings.multicast_loop.to_value(),
            "ttl" => settings.ttl.to_value(),
            "ttl-mc" => settings.ttl_mc.to_value(),
            "qos-dscp" => settings.qos_dscp.to_value(),
            "clients" => {
                drop(settings);

                let clients: Vec<String> = self
                    .sink_pad_handler
                    .clients()
                    .iter()
                    .map(ToString::to_string)
                    .collect();

                clients.join(",").to_value()
            }
            "context" => settings.context.to_value(),
            "context-wait" => (settings.context_wait.as_millis() as u32).to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(self.sink_pad.gst_pad()).unwrap();

        crate::set_element_flags(obj, gst::ElementFlags::SINK);
    }
}

impl ElementImpl for UdpSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing UDP sink",
                "Sink/Network",
                "Thread-sharing UDP sink",
                "Mathieu <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
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
            gst::StateChange::ReadyToPaused => {
                self.start(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PausedToReady => {
                self.stop(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element);
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }

    fn send_event(&self, _element: &Self::Type, event: gst::Event) -> bool {
        match event.view() {
            EventView::Latency(ev) => {
                self.sink_pad_handler.set_latency(ev.latency());
                self.sink_pad.gst_pad().push_event(event)
            }
            EventView::Step(..) => false,
            _ => self.sink_pad.gst_pad().push_event(event),
        }
    }
}
