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

use either::Either;

use futures::executor::block_on;
use futures::future::BoxFuture;
use futures::future::{abortable, AbortHandle, Aborted};
use futures::lock::{Mutex, MutexGuard};
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
use crate::runtime::{self, Context, JoinHandle, PadSink, PadSinkRef};
use crate::socket::{wrap_socket, GioSocketWrapper};

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
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

#[derive(Debug)]
struct State {
    segment: Option<gst::Segment>,
    latency: gst::ClockTime,
    context: Option<Context>,
    socket: Arc<Mutex<Option<tokio::net::UdpSocket>>>,
    socket_v6: Arc<Mutex<Option<tokio::net::UdpSocket>>>,
    clients: Vec<SocketAddr>,
    abort_handle: Option<AbortHandle>,
}

impl Default for State {
    fn default() -> State {
        State {
            segment: None,
            latency: gst::CLOCK_TIME_NONE,
            context: None,
            socket: Arc::new(Mutex::new(None)),
            socket_v6: Arc::new(Mutex::new(None)),
            clients: vec![SocketAddr::new(
                DEFAULT_HOST.unwrap().parse().unwrap(),
                DEFAULT_PORT as u16,
            )],
            abort_handle: None,
        }
    }
}

#[derive(Debug)]
struct PreparationSet {
    join_handle: JoinHandle<Result<(), gst::ErrorMessage>>,
}

#[derive(Debug)]
struct UdpSink {
    sink_pad: PadSink,
    state: Mutex<State>,
    settings: Mutex<Settings>,
    preparation_set: Mutex<Option<PreparationSet>>,
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
            Some(DEFAULT_BIND_ADDRESS),
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
            DEFAULT_BIND_PORT,
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

#[derive(Clone, Debug)]
struct UdpSinkPadHandler;

impl PadSinkHandler for UdpSinkPadHandler {
    type ElementImpl = UdpSink;

    fn sink_chain(
        &self,
        _pad: &PadSinkRef,
        _udpsink: &UdpSink,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let element = element.clone();

        async move {
            let udpsink = UdpSink::from_instance(&element);

            udpsink.render(&element, &buffer).await
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
        let element = element.clone();

        async move {
            let udpsink = UdpSink::from_instance(&element);

            for buffer in list.iter() {
                udpsink.render(&element, buffer).await?;
            }

            Ok(gst::FlowSuccess::Ok)
        }
        .boxed()
    }

    fn sink_event(
        &self,
        pad: &PadSinkRef,
        udpsink: &UdpSink,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        if event.is_serialized() {
            let pad_weak = pad.downgrade();
            let element = element.clone();
            Either::Right(
                async move {
                    let udpsink = UdpSink::from_instance(&element);
                    let pad = pad_weak.upgrade().expect("PadSink no longer exists");
                    gst_log!(CAT, obj: pad.gst_pad(), "Handling event {:?}", event);

                    match event.view() {
                        EventView::Eos(_) => {
                            let _ = element
                                .post_message(&gst::Message::new_eos().src(Some(&element)).build());
                        }
                        EventView::Segment(e) => {
                            let mut state = udpsink.state.lock().await;
                            state.segment = Some(e.get_segment().clone());
                        }
                        _ => (),
                    }

                    gst_log!(CAT, obj: pad.gst_pad(), "Queuing event {:?}", event);
                    true
                }
                .boxed(),
            )
        } else {
            match event.view() {
                EventView::FlushStart(..) => {
                    let _ = block_on(udpsink.stop(element));
                }
                _ => (),
            }

            gst_debug!(CAT, obj: pad.gst_pad(), "Fowarding non-serialized event {:?}", event);

            Either::Left(true)
        }
    }
}

impl UdpSink {
    /* Sends buffer to all clients, abortable */
    async fn render(
        &self,
        element: &gst::Element,
        buffer: &gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().await;
        let (render_fut, abort_handle) = abortable(self.do_render(element, buffer));
        state.abort_handle = Some(abort_handle);

        /* Drop our state so we can be interrupted while awaiting */
        drop(state);

        match render_fut.await {
            Ok(res) => res,
            Err(Aborted) => {
                gst_log!(CAT, obj: element, "Aborted render");
                Ok(gst::FlowSuccess::Ok)
            }
        }
    }

    async fn do_render(
        &self,
        element: &gst::Element,
        buffer: &gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let do_sync = self.settings.lock().await.sync;

        if do_sync {
            let state = self.state.lock().await;
            if let Some(segment) = &state.segment {
                if let Some(segment) = segment.downcast_ref::<gst::format::Time>() {
                    let rtime = segment.to_running_time(buffer.get_pts());
                    let rtime = if state.latency.is_some() {
                        rtime + state.latency
                    } else {
                        rtime
                    };
                    drop(state);
                    self.sync(&element, rtime).await;
                }
            }
        }

        gst_log!(CAT, obj: element, "Handling buffer {:?}", &buffer);

        let clients = self.state.lock().await.clients.clone();
        let data = buffer.map_readable().map_err(|_| {
            gst_element_error!(
                element,
                gst::StreamError::Format,
                ["Failed to map buffer readable"]
            );

            gst::FlowError::Error
        })?;

        let (socket, socket_v6) = {
            let state = self.state.lock().await;
            (Arc::clone(&state.socket), Arc::clone(&state.socket_v6))
        };

        for client in &clients {
            let socket = match client.ip() {
                IpAddr::V4(_) => &socket,
                IpAddr::V6(_) => &socket_v6,
            };

            if let Some(socket) = socket.lock().await.as_mut() {
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

    async fn prepare_socket_family(
        &self,
        element: &gst::Element,
        ipv6: bool,
    ) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().await;
        let mut settings = self.settings.lock().await;

        let socket = if let Some(ref wrapped_socket) = if ipv6 {
            &settings.socket_v6
        } else {
            &settings.socket
        } {
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
                    gst::ResourceError::OpenWrite,
                    ["Failed to setup socket for tokio: {}", err]
                )
            })?;

            if ipv6 {
                settings.used_socket_v6 = Some(wrapped_socket.clone());
            } else {
                settings.used_socket = Some(wrapped_socket.clone());
            }

            socket
        } else {
            let bind_addr = if ipv6 {
                &settings.bind_address_v6
            } else {
                &settings.bind_address
            };

            let bind_addr: IpAddr = bind_addr.parse().map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::Settings,
                    ["Invalid address '{}' set: {}", bind_addr, err]
                )
            })?;

            let bind_port = if ipv6 {
                settings.bind_port_v6
            } else {
                settings.bind_port
            };

            let saddr = SocketAddr::new(bind_addr, bind_port as u16);
            gst_debug!(CAT, obj: element, "Binding to {:?}", saddr);

            let builder = if ipv6 {
                net2::UdpBuilder::new_v6()
            } else {
                net2::UdpBuilder::new_v4()
            };

            let builder = match builder {
                Ok(builder) => builder,
                Err(err) => {
                    gst_warning!(
                        CAT,
                        obj: element,
                        "Failed to create {} socket builder: {}",
                        if ipv6 { "IPv6" } else { "IPv4" },
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

            let socket = tokio::net::UdpSocket::from_std(socket).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to setup socket for tokio: {}", err]
                )
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

            if ipv6 {
                settings.used_socket_v6 = Some(wrapper);
            } else {
                settings.used_socket = Some(wrapper);
            }

            socket
        };

        if ipv6 {
            state.socket_v6 = Arc::new(Mutex::new(Some(socket)));
        } else {
            state.socket = Arc::new(Mutex::new(Some(socket)));
        }

        Ok(())
    }

    async fn configure_client(
        &self,
        client: &SocketAddr,
        state: &MutexGuard<'_, State>,
    ) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().await;

        if client.ip().is_multicast() {
            match client.ip() {
                IpAddr::V4(addr) => {
                    if let Some(socket) = state.socket.lock().await.as_mut() {
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
                    if let Some(socket) = state.socket_v6.lock().await.as_mut() {
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
                    if let Some(socket) = state.socket.lock().await.as_mut() {
                        socket.set_ttl(settings.ttl).map_err(|err| {
                            gst_error_msg!(
                                gst::ResourceError::OpenWrite,
                                ["Failed to set unicast ttl: {}", err]
                            )
                        })?;
                    }
                }
                IpAddr::V6(_) => {
                    if let Some(socket) = state.socket_v6.lock().await.as_mut() {
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

    async fn prepare_sockets(element: gst::Element) -> Result<(), gst::ErrorMessage> {
        let this = Self::from_instance(&element);

        this.prepare_socket_family(&element, false).await?;
        this.prepare_socket_family(&element, true).await?;

        let state = this.state.lock().await;

        for client in &state.clients {
            this.configure_client(&client, &state).await?;
        }

        Ok(())
    }

    async fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Preparing");

        let mut state = self.state.lock().await;

        let context = {
            let settings = self.settings.lock().await.clone();

            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to acquire Context: {}", err]
                )
            })?
        };

        *self.preparation_set.lock().await = Some(PreparationSet {
            join_handle: context.spawn(Self::prepare_sockets(element.clone())),
        });

        state.context = Some(context);

        gst_debug!(CAT, obj: element, "Started preparing");

        Ok(())
    }

    async fn complete_preparation(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Completing preparation");

        let PreparationSet { join_handle } = self
            .preparation_set
            .lock()
            .await
            .take()
            .expect("preparation_set already taken");

        join_handle
            .await
            .expect("The socket preparation has panicked")?;

        self.sink_pad.prepare(&UdpSinkPadHandler).await;

        gst_debug!(CAT, obj: element, "Preparation completed");

        Ok(())
    }

    async fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().await;
        gst_debug!(CAT, obj: element, "Unpreparing");

        *state = State::default();

        self.sink_pad.unprepare().await;

        gst_debug!(CAT, obj: element, "Unprepared");
        Ok(())
    }

    async fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Stopping");

        let mut state = self.state.lock().await;

        if let Some(abort_handle) = state.abort_handle.take() {
            abort_handle.abort();
        }

        gst_debug!(CAT, obj: element, "Stopped");

        Ok(())
    }

    fn clear_clients(
        &self,
        element: &gst::Element,
        state: &mut MutexGuard<'_, State>,
        settings: &MutexGuard<'_, Settings>,
    ) {
        state.clients.clear();

        if let Some(host) = &settings.host {
            self.add_client(&element, state, &host, settings.port as u16);
        }
    }

    fn remove_client(
        &self,
        element: &gst::Element,
        state: &mut MutexGuard<'_, State>,
        host: &str,
        port: u16,
    ) {
        let addr: IpAddr = match host.parse() {
            Err(err) => {
                gst_error!(CAT, obj: element, "Failed to parse host {}: {}", host, err);
                return;
            }
            Ok(addr) => addr,
        };
        let addr = SocketAddr::new(addr, port);

        gst_info!(CAT, obj: element, "Removing client {:?}", addr);

        state.clients.retain(|addr2| addr != *addr2);
    }

    fn add_client(
        &self,
        element: &gst::Element,
        state: &mut MutexGuard<'_, State>,
        host: &str,
        port: u16,
    ) {
        let addr: IpAddr = match host.parse() {
            Err(err) => {
                gst_error!(CAT, obj: element, "Failed to parse host {}: {}", host, err);
                return;
            }
            Ok(addr) => addr,
        };
        let addr = SocketAddr::new(addr, port);

        let _ = self.configure_client(&addr, &state);

        if !state.clients.contains(&addr) {
            gst_info!(CAT, obj: element, "Adding client {:?}", addr);
            state.clients.push(addr);
        } else {
            gst_warning!(CAT, obj: element, "Not adding client {:?} again", &addr);
        }
    }
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
                    .expect("missing signal arg");

                let udpsink = Self::from_instance(&element);
                let mut state = block_on(udpsink.state.lock());

                udpsink.add_client(&element, &mut state, &host, port as u16);

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
                    .expect("missing signal arg");

                let udpsink = Self::from_instance(&element);

                let settings = block_on(udpsink.settings.lock());
                let mut state = block_on(udpsink.state.lock());

                if Some(&host) != settings.host.as_ref() || port != settings.port as i32 {
                    udpsink.remove_client(&element, &mut state, &host, port as u16);
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
                let mut state = block_on(udpsink.state.lock());
                let settings = block_on(udpsink.settings.lock());

                udpsink.clear_clients(&element, &mut state, &settings);

                None
            },
        );

        klass.install_properties(&PROPERTIES);
    }

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sink_pad = PadSink::new_from_template(&templ, Some("sink"));

        Self {
            sink_pad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
            preparation_set: Mutex::new(None),
        }
    }
}

impl ObjectImpl for UdpSink {
    glib_object_impl!();

    fn set_property(&self, obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        let element = obj.downcast_ref::<gst::Element>().unwrap();

        match *prop {
            subclass::Property("host", ..) => {
                let mut settings = block_on(self.settings.lock());
                let mut state = block_on(self.state.lock());
                if let Some(host) = &settings.host {
                    self.remove_client(&element, &mut state, &host, settings.port as u16);
                }

                settings.host = value.get().expect("type checked upstream");

                if let Some(host) = &settings.host {
                    self.add_client(&element, &mut state, &host, settings.port as u16);
                }
            }
            subclass::Property("port", ..) => {
                let mut settings = block_on(self.settings.lock());
                let mut state = block_on(self.state.lock());
                if let Some(host) = &settings.host {
                    self.remove_client(&element, &mut state, &host, settings.port as u16);
                }

                settings.port = value.get_some().expect("type checked upstream");

                if let Some(host) = &settings.host {
                    self.add_client(&element, &mut state, &host, settings.port as u16);
                }
            }
            subclass::Property("sync", ..) => {
                let mut settings = block_on(self.settings.lock());

                settings.sync = value.get_some().expect("type checked upstream");
            }
            subclass::Property("bind-address", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.bind_address = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("bind-port", ..) => {
                let mut settings = block_on(self.settings.lock());

                settings.bind_port = value.get_some().expect("type checked upstream");
            }
            subclass::Property("bind-address-v6", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.bind_address_v6 = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("bind-port-v6", ..) => {
                let mut settings = block_on(self.settings.lock());

                settings.bind_port_v6 = value.get_some().expect("type checked upstream");
            }
            subclass::Property("socket", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.socket = value
                    .get::<gio::Socket>()
                    .expect("type checked upstream")
                    .map(|socket| GioSocketWrapper::new(&socket));
            }
            subclass::Property("used-socket", ..) => {
                unreachable!();
            }
            subclass::Property("socket-v6", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.socket_v6 = value
                    .get::<gio::Socket>()
                    .expect("type checked upstream")
                    .map(|socket| GioSocketWrapper::new(&socket));
            }
            subclass::Property("used-socket-v6", ..) => {
                unreachable!();
            }
            subclass::Property("auto-multicast", ..) => {
                let mut settings = block_on(self.settings.lock());

                settings.auto_multicast = value.get_some().expect("type checked upstream");
            }
            subclass::Property("loop", ..) => {
                let mut settings = block_on(self.settings.lock());

                settings.multicast_loop = value.get_some().expect("type checked upstream");
            }
            subclass::Property("ttl", ..) => {
                let mut settings = block_on(self.settings.lock());

                settings.ttl = value.get_some().expect("type checked upstream");
            }
            subclass::Property("ttl-mc", ..) => {
                let mut settings = block_on(self.settings.lock());

                settings.ttl_mc = value.get_some().expect("type checked upstream");
            }
            subclass::Property("qos-dscp", ..) => {
                let mut settings = block_on(self.settings.lock());

                settings.qos_dscp = value.get_some().expect("type checked upstream");
            }
            subclass::Property("clients", ..) => {
                let clients: String = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
                let mut state = block_on(self.state.lock());
                let clients = clients.split(',');
                let settings = block_on(self.settings.lock());
                self.clear_clients(element, &mut state, &settings);

                for client in clients {
                    let split: Vec<&str> = client.rsplitn(2, ':').collect();

                    if split.len() == 2 {
                        match split[0].parse::<u16>() {
                            Ok(port) => self.add_client(element, &mut state, split[1], port),
                            Err(_) => (),
                        }
                    }
                }
            }
            subclass::Property("context", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.context_wait = value.get_some().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("host", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.host.to_value())
            }
            subclass::Property("port", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.port.to_value())
            }
            subclass::Property("sync", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.sync.to_value())
            }
            subclass::Property("bind-address", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.bind_address.to_value())
            }
            subclass::Property("bind-port", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.bind_port.to_value())
            }
            subclass::Property("bind-address-v6", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.bind_address_v6.to_value())
            }
            subclass::Property("bind-port-v6", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.bind_port_v6.to_value())
            }
            subclass::Property("socket", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings
                    .socket
                    .as_ref()
                    .map(GioSocketWrapper::as_socket)
                    .to_value())
            }
            subclass::Property("used-socket", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings
                    .used_socket
                    .as_ref()
                    .map(GioSocketWrapper::as_socket)
                    .to_value())
            }
            subclass::Property("socket-v6", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings
                    .socket_v6
                    .as_ref()
                    .map(GioSocketWrapper::as_socket)
                    .to_value())
            }
            subclass::Property("used-socket-v6", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings
                    .used_socket_v6
                    .as_ref()
                    .map(GioSocketWrapper::as_socket)
                    .to_value())
            }
            subclass::Property("auto-multicast", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.sync.to_value())
            }
            subclass::Property("loop", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.multicast_loop.to_value())
            }
            subclass::Property("ttl", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.ttl.to_value())
            }
            subclass::Property("ttl-mc", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.ttl_mc.to_value())
            }
            subclass::Property("qos-dscp", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.qos_dscp.to_value())
            }
            subclass::Property("clients", ..) => {
                let state = block_on(self.state.lock());

                let clients: Vec<String> =
                    state.clients.iter().map(|addr| addr.to_string()).collect();
                Ok(clients.join(",").to_value())
            }
            subclass::Property("context", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.context.to_value())
            }
            subclass::Property("context-wait", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.context_wait.to_value())
            }
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
                block_on(self.prepare(element)).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::ReadyToPaused => {
                block_on(self.complete_preparation(element)).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PausedToReady => {
                block_on(self.stop(element)).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                block_on(self.unprepare(element)).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }

    fn send_event(&self, _element: &gst::Element, event: gst::Event) -> bool {
        match event.view() {
            EventView::Latency(ev) => {
                let _ = block_on(async {
                    let mut state = self.state.lock().await;
                    state.latency = ev.get_latency();
                });

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
