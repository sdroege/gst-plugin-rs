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
//
// SPDX-License-Identifier: LGPL-2.1-or-later

use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::Peekable;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::EventView;
use gst::{element_error, error_msg};

use once_cell::sync::Lazy;

use crate::runtime::prelude::*;
use crate::runtime::{self, Async, Context, PadSink, PadSinkRef, Task};
use crate::socket::{wrap_socket, GioSocketWrapper};

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::Poll;
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
const DEFAULT_CONTEXT_WAIT: Duration = Duration::ZERO;

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
    clients: BTreeSet<SocketAddr>,
    latency: Option<gst::ClockTime>,
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
            clients: BTreeSet::from([SocketAddr::new(
                DEFAULT_HOST.unwrap().parse().unwrap(),
                DEFAULT_PORT as u16,
            )]),
            latency: None,
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

#[derive(Clone, Debug)]
struct UdpSinkPadHandler;

impl PadSinkHandler for UdpSinkPadHandler {
    type ElementImpl = UdpSink;

    fn sink_chain(
        &self,
        _pad: &PadSinkRef,
        udpsink: &UdpSink,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let sender = udpsink.clone_item_sender();
        let element = element.clone().downcast::<super::UdpSink>().unwrap();

        async move {
            if sender.send_async(TaskItem::Buffer(buffer)).await.is_err() {
                gst::debug!(CAT, obj: &element, "Flushing");
                return Err(gst::FlowError::Flushing);
            }

            Ok(gst::FlowSuccess::Ok)
        }
        .boxed()
    }

    fn sink_chain_list(
        &self,
        _pad: &PadSinkRef,
        udpsink: &UdpSink,
        element: &gst::Element,
        list: gst::BufferList,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let sender = udpsink.clone_item_sender();
        let element = element.clone().downcast::<super::UdpSink>().unwrap();

        async move {
            for buffer in list.iter_owned() {
                if sender.send_async(TaskItem::Buffer(buffer)).await.is_err() {
                    gst::debug!(CAT, obj: &element, "Flushing");
                    return Err(gst::FlowError::Flushing);
                }
            }

            Ok(gst::FlowSuccess::Ok)
        }
        .boxed()
    }

    fn sink_event_serialized(
        &self,
        _pad: &PadSinkRef,
        udpsink: &UdpSink,
        element: &gst::Element,
        event: gst::Event,
    ) -> BoxFuture<'static, bool> {
        let sender = udpsink.clone_item_sender();
        let element = element.clone().downcast::<super::UdpSink>().unwrap();

        async move {
            if let EventView::FlushStop(_) = event.view() {
                let udpsink = element.imp();
                return udpsink.task.flush_stop().await_maybe_on_context().is_ok();
            } else if sender.send_async(TaskItem::Event(event)).await.is_err() {
                gst::debug!(CAT, obj: &element, "Flushing");
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
            return udpsink.task.flush_start().await_maybe_on_context().is_ok();
        }

        true
    }
}

#[derive(Debug)]
enum Command {
    AddClient(SocketAddr),
    RemoveClient(SocketAddr),
    ReplaceWithClients(BTreeSet<SocketAddr>),
    SetLatency(Option<gst::ClockTime>),
    SetSync(bool),
}

struct UdpSinkTask {
    element: super::UdpSink,
    item_receiver: Peekable<flume::r#async::RecvStream<'static, TaskItem>>,
    cmd_receiver: flume::Receiver<Command>,
    clients: BTreeSet<SocketAddr>,
    socket: Option<Async<UdpSocket>>,
    socket_v6: Option<Async<UdpSocket>>,
    sync: bool,
    latency: Option<gst::ClockTime>,
    segment: Option<gst::Segment>,
}

impl UdpSinkTask {
    fn new(
        element: &super::UdpSink,
        item_receiver: flume::Receiver<TaskItem>,
        cmd_receiver: flume::Receiver<Command>,
    ) -> Self {
        UdpSinkTask {
            element: element.clone(),
            item_receiver: item_receiver.into_stream().peekable(),
            cmd_receiver,
            clients: Default::default(),
            socket: None,
            socket_v6: None,
            sync: false,
            latency: None,
            segment: None,
        }
    }

    async fn flush(&mut self) {
        // Purge the channel
        while let Poll::Ready(Some(_item)) = futures::poll!(self.item_receiver.next()) {}
    }

    fn process_command(&mut self, cmd: Command) {
        use Command::*;
        match cmd {
            AddClient(client) => self.add_client(client),
            RemoveClient(client) => self.remove_client(&client),
            ReplaceWithClients(clients) => self.replace_with_clients(clients),
            SetSync(sync) => self.sync = sync,
            SetLatency(latency) => self.latency = latency,
        }
    }
}

/// Socket configuration.
impl UdpSinkTask {
    fn prepare_socket(
        &self,
        settings: &mut Settings,
        family: SocketFamily,
    ) -> Result<Option<Async<UdpSocket>>, gst::ErrorMessage> {
        let wrapped_socket = match family {
            SocketFamily::Ipv4 => &settings.socket,
            SocketFamily::Ipv6 => &settings.socket_v6,
        };

        if let Some(ref wrapped_socket) = wrapped_socket {
            let socket: UdpSocket = wrapped_socket.get();
            let socket = Async::<UdpSocket>::try_from(socket).map_err(|err| {
                error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to setup Async socket: {}", err]
                )
            })?;

            match family {
                SocketFamily::Ipv4 => {
                    settings.used_socket = Some(wrapped_socket.clone());
                }
                SocketFamily::Ipv6 => {
                    settings.used_socket_v6 = Some(wrapped_socket.clone());
                }
            }

            Ok(Some(socket))
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
            gst::debug!(CAT, obj: &self.element, "Binding to {:?}", saddr);

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
                    gst::warning!(
                        CAT,
                        obj: &self.element,
                        "Failed to create {} socket: {}",
                        match family {
                            SocketFamily::Ipv4 => "IPv4",
                            SocketFamily::Ipv6 => "IPv6",
                        },
                        err
                    );
                    return Ok(None);
                }
            };

            socket.bind(&saddr.into()).map_err(|err| {
                error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to bind socket: {}", err]
                )
            })?;

            let socket = Async::<UdpSocket>::try_from(socket).map_err(|err| {
                error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to setup Async socket: {}", err]
                )
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
                }
                SocketFamily::Ipv6 => {
                    settings.used_socket_v6 = Some(wrapper);
                }
            }

            Ok(Some(socket))
        }
    }

    fn add_client(&mut self, addr: SocketAddr) {
        if self.clients.contains(&addr) {
            gst::warning!(CAT, obj: &self.element, "Not adding client {:?} again", &addr);
            return;
        }

        let udpsink = self.element.imp();
        let mut settings = udpsink.settings.lock().unwrap();
        match self.configure_client(&settings, &addr) {
            Ok(()) => {
                gst::info!(CAT, obj: &self.element, "Added client {:?}", addr);
                self.clients.insert(addr);
            }
            Err(err) => {
                gst::error!(CAT, obj: &self.element, "Failed to add client {:?}: {}", addr, err);
                settings.clients = self.clients.clone();
                self.element.post_error_message(err);
            }
        }
    }

    fn remove_client(&mut self, addr: &SocketAddr) {
        if self.clients.take(addr).is_none() {
            gst::warning!(CAT, obj: &self.element, "Not removing unknown client {:?}", &addr);
            return;
        }

        let udpsink = self.element.imp();
        let mut settings = udpsink.settings.lock().unwrap();
        match self.unconfigure_client(&settings, addr) {
            Ok(()) => {
                gst::info!(CAT, obj: &self.element, "Removed client {:?}", addr);
            }
            Err(err) => {
                gst::error!(CAT, obj: &self.element, "Failed to remove client {:?}: {}", addr, err);
                settings.clients = self.clients.clone();
                self.element.post_error_message(err);
            }
        }
    }

    fn replace_with_clients(&mut self, mut clients_to_add: BTreeSet<SocketAddr>) {
        if clients_to_add.is_empty() {
            gst::info!(CAT, obj: &self.element, "Clearing clients");
        } else {
            gst::info!(CAT, obj: &self.element, "Replacing clients");
        }

        let old_clients = std::mem::take(&mut self.clients);

        let mut res = Ok(());
        let udpsink = self.element.imp();
        let mut settings = udpsink.settings.lock().unwrap();

        for addr in old_clients.iter() {
            if clients_to_add.take(addr).is_some() {
                // client is already configured
                self.clients.insert(*addr);
            } else if let Err(err) = self.unconfigure_client(&settings, addr) {
                gst::error!(CAT, obj: &self.element, "Failed to remove client {:?}: {}", addr, err);
                res = Err(err);
            } else {
                gst::info!(CAT, obj: &self.element, "Removed client {:?}", addr);
            }
        }

        for addr in clients_to_add.into_iter() {
            if let Err(err) = self.configure_client(&settings, &addr) {
                gst::error!(CAT, obj: &self.element, "Failed to add client {:?}: {}", addr, err);
                res = Err(err);
            } else {
                gst::info!(CAT, obj: &self.element, "Added client {:?}", addr);
                self.clients.insert(addr);
            }
        }

        // FIXME: which error handling:
        // - If at least one client could be configured, should we keep going? (current)
        // - or, should we consider the preparation failed when the first client
        //   configuration fails? (previously)
        if let Err(err) = res {
            settings.clients = self.clients.clone();
            self.element.post_error_message(err);
        }
    }

    fn configure_client(
        &self,
        settings: &Settings,
        client: &SocketAddr,
    ) -> Result<(), gst::ErrorMessage> {
        if client.ip().is_multicast() {
            match client.ip() {
                IpAddr::V4(addr) => {
                    if let Some(socket) = self.socket.as_ref() {
                        if settings.auto_multicast {
                            socket
                                .as_ref()
                                .join_multicast_v4(&addr, &Ipv4Addr::new(0, 0, 0, 0))
                                .map_err(|err| {
                                    error_msg!(
                                        gst::ResourceError::OpenWrite,
                                        [
                                            "Failed to join multicast group for {:?}: {}",
                                            client,
                                            err
                                        ]
                                    )
                                })?;
                        }
                        if settings.multicast_loop {
                            socket.as_ref().set_multicast_loop_v4(true).map_err(|err| {
                                error_msg!(
                                    gst::ResourceError::OpenWrite,
                                    ["Failed to set multicast loop for {:?}: {}", client, err]
                                )
                            })?;
                        }

                        socket
                            .as_ref()
                            .set_multicast_ttl_v4(settings.ttl_mc)
                            .map_err(|err| {
                                error_msg!(
                                    gst::ResourceError::OpenWrite,
                                    ["Failed to set multicast ttl for {:?}: {}", client, err]
                                )
                            })?;
                    }
                }
                IpAddr::V6(addr) => {
                    if let Some(socket) = self.socket_v6.as_ref() {
                        if settings.auto_multicast {
                            socket.as_ref().join_multicast_v6(&addr, 0).map_err(|err| {
                                error_msg!(
                                    gst::ResourceError::OpenWrite,
                                    ["Failed to join multicast group for {:?}: {}", client, err]
                                )
                            })?;
                        }
                        if settings.multicast_loop {
                            socket.as_ref().set_multicast_loop_v6(true).map_err(|err| {
                                error_msg!(
                                    gst::ResourceError::OpenWrite,
                                    ["Failed to set multicast loop for {:?}: {}", client, err]
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
                    if let Some(socket) = self.socket.as_ref() {
                        socket.as_ref().set_ttl(settings.ttl).map_err(|err| {
                            error_msg!(
                                gst::ResourceError::OpenWrite,
                                ["Failed to set unicast ttl for {:?}: {}", client, err]
                            )
                        })?;
                    }
                }
                IpAddr::V6(_) => {
                    if let Some(socket) = self.socket_v6.as_ref() {
                        socket.as_ref().set_ttl(settings.ttl).map_err(|err| {
                            error_msg!(
                                gst::ResourceError::OpenWrite,
                                ["Failed to set unicast ttl for {:?}: {}", client, err]
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
        client: &SocketAddr,
    ) -> Result<(), gst::ErrorMessage> {
        if client.ip().is_multicast() {
            match client.ip() {
                IpAddr::V4(addr) => {
                    if let Some(socket) = self.socket.as_ref() {
                        if settings.auto_multicast {
                            socket
                                .as_ref()
                                .leave_multicast_v4(&addr, &Ipv4Addr::new(0, 0, 0, 0))
                                .map_err(|err| {
                                    error_msg!(
                                        gst::ResourceError::OpenWrite,
                                        [
                                            "Failed to leave multicast group for {:?}: {}",
                                            client,
                                            err
                                        ]
                                    )
                                })?;
                        }
                    }
                }
                IpAddr::V6(addr) => {
                    if let Some(socket) = self.socket_v6.as_ref() {
                        if settings.auto_multicast {
                            socket
                                .as_ref()
                                .leave_multicast_v6(&addr, 0)
                                .map_err(|err| {
                                    error_msg!(
                                        gst::ResourceError::OpenWrite,
                                        [
                                            "Failed to leave multicast group for {:?}: {}",
                                            client,
                                            err
                                        ]
                                    )
                                })?;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

/// Buffer handling.
impl UdpSinkTask {
    async fn render(&mut self, buffer: gst::Buffer) -> Result<(), gst::FlowError> {
        let data = buffer.map_readable().map_err(|_| {
            element_error!(
                self.element,
                gst::StreamError::Format,
                ["Failed to map buffer readable"]
            );
            gst::FlowError::Error
        })?;

        for client in self.clients.iter() {
            let socket = match client.ip() {
                IpAddr::V4(_) => &mut self.socket,
                IpAddr::V6(_) => &mut self.socket_v6,
            };

            if let Some(socket) = socket.as_mut() {
                gst::log!(CAT, obj: &self.element, "Sending to {:?}", &client);
                socket.send_to(&data, *client).await.map_err(|err| {
                    element_error!(
                        self.element,
                        gst::StreamError::Failed,
                        ("I/O error"),
                        ["streaming stopped, I/O error {}", err]
                    );
                    gst::FlowError::Error
                })?;
            } else {
                element_error!(
                    self.element,
                    gst::StreamError::Failed,
                    ("I/O error"),
                    ["No socket available for sending to {}", client]
                );
                return Err(gst::FlowError::Error);
            }
        }

        gst::log!(
            CAT,
            obj: &self.element,
            "Sent buffer {:?} to all clients",
            &buffer
        );

        Ok(())
    }

    /// Waits until specified time.
    async fn sync(&self, running_time: gst::ClockTime) {
        let now = self.element.current_running_time();

        if let Ok(Some(delay)) = running_time.opt_checked_sub(now) {
            gst::trace!(CAT, obj: &self.element, "sync: waiting {}", delay);
            runtime::timer::delay_for(delay.into()).await;
        }
    }
}

impl TaskImpl for UdpSinkTask {
    type Item = TaskItem;

    fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst::info!(CAT, obj: &self.element, "Preparing Task");
            assert!(self.clients.is_empty());
            let clients = {
                let udpsink = self.element.imp();
                let mut settings = udpsink.settings.lock().unwrap();
                self.sync = settings.sync;
                self.socket = self.prepare_socket(&mut settings, SocketFamily::Ipv4)?;
                self.socket_v6 = self.prepare_socket(&mut settings, SocketFamily::Ipv6)?;
                self.latency = settings.latency;
                settings.clients.clone()
            };

            self.replace_with_clients(clients);

            Ok(())
        }
        .boxed()
    }

    fn unprepare(&mut self) -> BoxFuture<'_, ()> {
        async move {
            gst::info!(CAT, obj: &self.element, "Unpreparing Task");

            let udpsink = self.element.imp();
            let settings = udpsink.settings.lock().unwrap();
            for addr in self.clients.iter() {
                let _ = self.unconfigure_client(&settings, addr);
            }
        }
        .boxed()
    }

    fn try_next(&mut self) -> BoxFuture<'_, Result<TaskItem, gst::FlowError>> {
        async move {
            loop {
                gst::info!(CAT, obj: &self.element, "Awaiting next item or command");
                futures::select_biased! {
                    cmd = self.cmd_receiver.recv_async() => {
                        self.process_command(cmd.unwrap());
                    }
                    item_opt = Pin::new(&mut self.item_receiver).peek() => {
                        // Check the peeked item in case we need to sync.
                        // The item will still be available in the channel
                        // in case this is cancelled by a state transition.
                        match item_opt {
                            Some(TaskItem::Buffer(buffer)) => {
                                if self.sync {
                                    let rtime = self.segment.as_ref().and_then(|segment| {
                                        segment
                                            .downcast_ref::<gst::format::Time>()
                                            .and_then(|segment| {
                                                segment.to_running_time(buffer.pts()).opt_add(self.latency)
                                            })
                                    });
                                    if let Some(rtime) = rtime {
                                        // This can be cancelled by a state transition.
                                        self.sync(rtime).await;
                                    }
                                }
                            }
                            Some(_) => (),
                            None => {
                                panic!("Internal channel sender dropped while Task is Started");
                            }
                        }

                        // An item was peeked above, we can now pop it without losing it.
                        return Ok(self.item_receiver.next().await.unwrap());
                    }
                }
            }
        }
        .boxed()
    }

    fn handle_item(&mut self, item: TaskItem) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            gst::info!(CAT, obj: &self.element, "Handling {:?}", item);

            match item {
                TaskItem::Buffer(buffer) => self.render(buffer).await.map_err(|err| {
                    element_error!(
                        &self.element,
                        gst::StreamError::Failed,
                        ["Failed to render item, stopping task: {}", err]
                    );
                    gst::FlowError::Error
                })?,
                TaskItem::Event(event) => match event.view() {
                    EventView::Eos(_) => {
                        let _ = self
                            .element
                            .post_message(gst::message::Eos::builder().src(&self.element).build());
                    }
                    EventView::Segment(e) => {
                        self.segment = Some(e.segment().clone());
                    }
                    EventView::SinkMessage(e) => {
                        let _ = self.element.post_message(e.message());
                    }
                    _ => (),
                },
            }

            Ok(())
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async {
            gst::info!(CAT, obj: &self.element, "Stopping Task");
            self.flush().await;
            Ok(())
        }
        .boxed()
    }

    fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async {
            gst::info!(CAT, obj: &self.element, "Starting Task Flush");
            self.flush().await;
            Ok(())
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
    task: Task,
    item_sender: Mutex<Option<flume::Sender<TaskItem>>>,
    cmd_sender: Mutex<Option<flume::Sender<Command>>>,
    settings: Mutex<Settings>,
}

impl UdpSink {
    #[track_caller]
    fn clone_item_sender(&self) -> flume::Sender<TaskItem> {
        self.item_sender.lock().unwrap().as_ref().unwrap().clone()
    }

    fn prepare(&self, element: &super::UdpSink) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Preparing");

        let context = {
            let settings = self.settings.lock().unwrap();

            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to acquire Context: {}", err]
                )
            })?
        };

        // Enable backpressure for items
        let (item_sender, item_receiver) = flume::bounded(0);
        let (cmd_sender, cmd_receiver) = flume::unbounded();
        let task_impl = UdpSinkTask::new(element, item_receiver, cmd_receiver);
        self.task.prepare(task_impl, context).block_on()?;

        *self.item_sender.lock().unwrap() = Some(item_sender);
        *self.cmd_sender.lock().unwrap() = Some(cmd_sender);

        gst::debug!(CAT, obj: element, "Started preparation");

        Ok(())
    }

    fn unprepare(&self, element: &super::UdpSink) {
        gst::debug!(CAT, obj: element, "Unpreparing");
        self.task.unprepare().block_on().unwrap();
        gst::debug!(CAT, obj: element, "Unprepared");
    }

    fn stop(&self, element: &super::UdpSink) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Stopping");
        self.task.stop().block_on()?;
        gst::debug!(CAT, obj: element, "Stopped");
        Ok(())
    }

    fn start(&self, element: &super::UdpSink) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Starting");
        self.task.start().block_on()?;
        gst::debug!(CAT, obj: element, "Started");
        Ok(())
    }
}

impl UdpSink {
    fn add_client(&self, settings: &mut Settings, client: SocketAddr) {
        settings.clients.insert(client);
        if let Some(cmd_sender) = self.cmd_sender.lock().unwrap().as_mut() {
            cmd_sender.send(Command::AddClient(client)).unwrap();
        }
    }

    fn remove_client(&self, settings: &mut Settings, client: SocketAddr) {
        settings.clients.remove(&client);
        if let Some(cmd_sender) = self.cmd_sender.lock().unwrap().as_mut() {
            cmd_sender.send(Command::RemoveClient(client)).unwrap();
        }
    }

    fn replace_with_clients(
        &self,
        settings: &mut Settings,
        clients: impl IntoIterator<Item = SocketAddr>,
    ) {
        let clients = BTreeSet::<SocketAddr>::from_iter(clients);
        if let Some(cmd_sender) = self.cmd_sender.lock().unwrap().as_mut() {
            settings.clients = clients.clone();
            cmd_sender
                .send(Command::ReplaceWithClients(clients))
                .unwrap();
        } else {
            settings.clients = clients;
        }
    }
}

fn try_into_socket_addr(element: &super::UdpSink, host: &str, port: i32) -> Result<SocketAddr, ()> {
    let addr: IpAddr = match host.parse() {
        Err(err) => {
            gst::error!(CAT, obj: element, "Failed to parse host {}: {}", host, err);
            return Err(());
        }
        Ok(addr) => addr,
    };

    let port: u16 = match port.try_into() {
        Err(err) => {
            gst::error!(CAT, obj: element, "Invalid port {}: {}", port, err);
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
        Self {
            sink_pad: PadSink::new(
                gst::Pad::from_template(&klass.pad_template("sink").unwrap(), Some("sink")),
                UdpSinkPadHandler,
            ),
            task: Task::default(),
            item_sender: Default::default(),
            cmd_sender: Default::default(),
            settings: Default::default(),
        }
    }
}

impl ObjectImpl for UdpSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
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
                glib::ParamSpecBoolean::builder("sync")
                    .nick("Sync")
                    .blurb("Sync on the clock")
                    .default_value(DEFAULT_SYNC)
                    .build(),
                glib::ParamSpecString::builder("bind-address")
                    .nick("Bind Address")
                    .blurb("Address to bind the socket to")
                    .default_value(Some(DEFAULT_BIND_ADDRESS))
                    .build(),
                glib::ParamSpecInt::builder("bind-port")
                    .nick("Bind Port")
                    .blurb("Port to bind the socket to")
                    .minimum(0)
                    .maximum(u16::MAX as i32)
                    .default_value(DEFAULT_BIND_PORT)
                    .build(),
                glib::ParamSpecString::builder("bind-address-v6")
                    .nick("Bind Address V6")
                    .blurb("Address to bind the V6 socket to")
                    .default_value(Some(DEFAULT_BIND_ADDRESS_V6))
                    .build(),
                glib::ParamSpecInt::builder("bind-port-v6")
                    .nick("Bind Port")
                    .blurb("Port to bind the V6 socket to")
                    .minimum(0)
                    .maximum(u16::MAX as i32)
                    .default_value(DEFAULT_BIND_PORT_V6)
                    .build(),
                glib::ParamSpecObject::builder::<gio::Socket>("socket")
                    .nick("Socket")
                    .blurb("Socket to use for UDP transmission. (None == allocate)")
                    .build(),
                glib::ParamSpecObject::builder::<gio::Socket>("used-socket")
                    .nick("Used Socket")
                    .blurb("Socket currently in use for UDP transmission. (None = no socket)")
                    .read_only()
                    .build(),
                glib::ParamSpecObject::builder::<gio::Socket>("socket-v6")
                    .nick("Socket V6")
                    .blurb("IPV6 Socket to use for UDP transmission. (None == allocate)")
                    .build(),
                glib::ParamSpecObject::builder::<gio::Socket>("used-socket-v6")
                    .nick("Used Socket V6")
                    .blurb("V6 Socket currently in use for UDP transmission. (None = no socket)")
                    .read_only()
                    .build(),
                glib::ParamSpecBoolean::builder("auto-multicast")
                    .nick("Auto multicast")
                    .blurb("Automatically join/leave the multicast groups, FALSE means user has to do it himself")
                    .default_value(DEFAULT_AUTO_MULTICAST)
                    .build(),
                glib::ParamSpecBoolean::builder("loop")
                    .nick("Loop")
                    .blurb("Set the multicast loop parameter.")
                    .default_value(DEFAULT_LOOP)
                    .build(),
                glib::ParamSpecUInt::builder("ttl")
                    .nick("Time To Live")
                    .blurb("Used for setting the unicast TTL parameter")
                    .maximum(u8::MAX as u32)
                    .default_value(DEFAULT_TTL)
                    .build(),
                glib::ParamSpecUInt::builder("ttl-mc")
                    .nick("Time To Live Multicast")
                    .blurb("Used for setting the multicast TTL parameter")
                    .maximum(u8::MAX as u32)
                    .default_value(DEFAULT_TTL_MC)
                    .build(),
                glib::ParamSpecInt::builder("qos-dscp")
                    .nick("QoS DSCP")
                    .blurb("Quality of Service, differentiated services code point (-1 default)")
                    .minimum(-1)
                    .maximum(63)
                    .default_value(DEFAULT_QOS_DSCP)
                    .build(),
                glib::ParamSpecString::builder("clients")
                    .nick("Clients")
                    .blurb("A comma separated list of host:port pairs with destinations")
                    .default_value(Some(DEFAULT_CLIENTS))
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                glib::subclass::Signal::builder("add")
                    .param_types([String::static_type(), i32::static_type()])
                    .action()
                    .class_handler(|_, args| {
                        let element = args[0].get::<super::UdpSink>().expect("signal arg");
                        let host = args[1].get::<String>().expect("signal arg");
                        let port = args[2].get::<i32>().expect("signal arg");

                        if let Ok(addr) = try_into_socket_addr(&element, &host, port) {
                            let udpsink = element.imp();
                            let mut settings = udpsink.settings.lock().unwrap();
                            udpsink.add_client(&mut settings, addr);
                        }

                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("remove")
                    .param_types([String::static_type(), i32::static_type()])
                    .action()
                    .class_handler(|_, args| {
                        let element = args[0].get::<super::UdpSink>().expect("signal arg");
                        let host = args[1].get::<String>().expect("signal arg");
                        let port = args[2].get::<i32>().expect("signal arg");

                        if let Ok(addr) = try_into_socket_addr(&element, &host, port) {
                            let udpsink = element.imp();
                            let mut settings = udpsink.settings.lock().unwrap();
                            udpsink.remove_client(&mut settings, addr);
                        }

                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("clear")
                    .action()
                    .class_handler(|_, args| {
                        let element = args[0].get::<super::UdpSink>().expect("signal arg");

                        let udpsink = element.imp();
                        let mut settings = udpsink.settings.lock().unwrap();
                        udpsink.replace_with_clients(&mut settings, BTreeSet::new());

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
                let sync = value.get().expect("type checked upstream");
                settings.sync = sync;
                if let Some(cmd_sender) = self.cmd_sender.lock().unwrap().as_mut() {
                    cmd_sender.send(Command::SetSync(sync)).unwrap();
                }
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

                let clients = clients.split(',').filter_map(|client| {
                    let rsplit: Vec<&str> = client.rsplitn(2, ':').collect();

                    if rsplit.len() == 2 {
                        rsplit[0]
                            .parse::<i32>()
                            .map_err(|err| {
                                gst::error!(CAT, obj: obj, "Invalid port {}: {}", rsplit[0], err);
                            })
                            .and_then(|port| try_into_socket_addr(obj, rsplit[1], port))
                            .ok()
                    } else {
                        None
                    }
                });

                self.replace_with_clients(&mut settings, clients);
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
                let clients = settings.clients.clone();
                drop(settings);
                let clients: Vec<String> = clients.iter().map(ToString::to_string).collect();

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

impl GstObjectImpl for UdpSink {}

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
        gst::trace!(CAT, obj: element, "Changing state {:?}", transition);

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
                let latency = Some(ev.latency());
                if let Some(cmd_sender) = self.cmd_sender.lock().unwrap().as_mut() {
                    cmd_sender.send(Command::SetLatency(latency)).unwrap();
                }
                self.settings.lock().unwrap().latency = latency;
                self.sink_pad.gst_pad().push_event(event)
            }
            EventView::Step(..) => false,
            _ => self.sink_pad.gst_pad().push_event(event),
        }
    }
}
