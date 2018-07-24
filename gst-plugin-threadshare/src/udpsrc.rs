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
use gst;
use gst::prelude::*;

use gobject_subclass::object::*;
use gst_plugin::element::*;

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

use iocontext::*;
use socket::*;

const DEFAULT_ADDRESS: Option<&'static str> = Some("127.0.0.1");
const DEFAULT_PORT: u32 = 5000;
const DEFAULT_REUSE: bool = true;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_MTU: u32 = 1500;
const DEFAULT_CONTEXT: &'static str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;

#[derive(Debug, Clone)]
struct Settings {
    address: Option<String>,
    port: u32,
    reuse: bool,
    caps: Option<gst::Caps>,
    mtu: u32,
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
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
        }
    }
}

static PROPERTIES: [Property; 7] = [
    Property::String(
        "address",
        "Address",
        "Address/multicast group to listen on",
        DEFAULT_ADDRESS,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "port",
        "Port",
        "Port to listen on",
        (0, u16::MAX as u32),
        DEFAULT_PORT,
        PropertyMutability::ReadWrite,
    ),
    Property::Boolean(
        "reuse",
        "Reuse",
        "Allow reuse of the port",
        DEFAULT_REUSE,
        PropertyMutability::ReadWrite,
    ),
    Property::Boxed(
        "caps",
        "Caps",
        "Caps to use",
        gst::Caps::static_type,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "mtu",
        "MTU",
        "MTU",
        (0, u16::MAX as u32),
        DEFAULT_MTU,
        PropertyMutability::ReadWrite,
    ),
    Property::String(
        "context",
        "Context",
        "Context name to share threads with",
        Some(DEFAULT_CONTEXT),
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "context-wait",
        "Context Wait",
        "Throttle poll loop to run at most once every this many ms",
        (0, 1000),
        DEFAULT_CONTEXT_WAIT,
        PropertyMutability::ReadWrite,
    ),
];

pub struct UdpReader {
    socket: net::UdpSocket,
}

impl UdpReader {
    pub fn new(socket: net::UdpSocket) -> Self {
        Self { socket: socket }
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
    fn class_init(klass: &mut ElementClass) {
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
        );
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }

    fn init(element: &Element) -> Box<ElementImpl<Element>> {
        let templ = element.get_pad_template("src").unwrap();
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
        element.add_pad(&src_pad).unwrap();

        ::set_element_flags(element, gst::ElementFlags::SOURCE);

        Box::new(Self {
            cat: gst::DebugCategory::new(
                "ts-udpsrc",
                gst::DebugColorFlags::empty(),
                "Thread-sharing UDP source",
            ),
            src_pad: src_pad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        })
    }

    fn src_event(&self, pad: &gst::Pad, element: &Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                let _ = self.stop(element);
                true
            }
            EventView::FlushStop(..) => {
                let (ret, state, pending) = element.get_state(0.into());
                if ret == gst::StateChangeReturn::Success && state == gst::State::Playing
                    || ret == gst::StateChangeReturn::Async && pending == gst::State::Playing
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

    fn src_query(&self, pad: &gst::Pad, _element: &Element, query: &mut gst::QueryRef) -> bool {
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
                    ("io-context", &glib::AnySendValue::new(io_context.clone())),
                    (
                        "pending-future-id",
                        &glib::AnySendValue::new(*pending_future_id),
                    ),
                ],
            );
            Some(gst::Event::new_custom_downstream_sticky(s).build())
        } else {
            None
        }
    }

    fn push_buffer(
        &self,
        element: &Element,
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

        let res = match self.src_pad.push(buffer).into_result() {
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

    fn prepare(&self, element: &Element) -> Result<(), gst::ErrorMessage> {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

        gst_debug!(self.cat, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let mut state = self.state.lock().unwrap();

        let io_context = IOContext::new(&settings.context, settings.context_wait).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to create IO context: {}", err]
            )
        })?;

        let addr: IpAddr = match settings.address {
            None => {
                return Err(gst_error_msg!(
                    gst::ResourceError::Settings,
                    ["No address set"]
                ))
            }
            Some(ref addr) => match addr.parse() {
                Err(err) => {
                    return Err(gst_error_msg!(
                        gst::ResourceError::Settings,
                        ["Invalid address '{}' set: {}", addr, err]
                    ))
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
        }.map_err(|err| {
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
                    let udpsrc = element_clone.get_impl().downcast_ref::<UdpSrc>().unwrap();
                    udpsrc.push_buffer(&element_clone, buffer)
                },
                move |err| {
                    let udpsrc = element_clone2.get_impl().downcast_ref::<UdpSrc>().unwrap();
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

        Ok(())
    }

    fn unprepare(&self, element: &Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Unpreparing");

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

    fn start(&self, element: &Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Starting");
        let state = self.state.lock().unwrap();

        if let Some(ref socket) = state.socket {
            socket.unpause(element.get_clock(), Some(element.get_base_time()));
        }

        gst_debug!(self.cat, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &Element) -> Result<(), ()> {
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

impl ObjectImpl<Element> for UdpSrc {
    fn set_property(&self, _obj: &glib::Object, id: u32, value: &glib::Value) {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::String("address", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.address = value.get();
            }
            Property::UInt("port", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.port = value.get().unwrap();
            }
            Property::Boolean("reuse", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.reuse = value.get().unwrap();
            }
            Property::Boxed("caps", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.caps = value.get();
            }
            Property::UInt("mtu", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.mtu = value.get().unwrap();
            }
            Property::String("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context = value.get().unwrap_or_else(|| "".into());
            }
            Property::UInt("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_wait = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::String("address", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.address.to_value())
            }
            Property::UInt("port", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.port.to_value())
            }
            Property::Boolean("reuse", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.reuse.to_value())
            }
            Property::Boxed("caps", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.caps.to_value())
            }
            Property::UInt("mtu", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.mtu.to_value())
            }
            Property::String("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context.to_value())
            }
            Property::UInt("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context_wait.to_value())
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl<Element> for UdpSrc {
    fn change_state(
        &self,
        element: &Element,
        transition: gst::StateChange,
    ) -> gst::StateChangeReturn {
        gst_trace!(self.cat, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => match self.prepare(element) {
                Err(err) => {
                    element.post_error_message(&err);
                    return gst::StateChangeReturn::Failure;
                }
                Ok(_) => (),
            },
            gst::StateChange::PlayingToPaused => match self.stop(element) {
                Err(_) => return gst::StateChangeReturn::Failure,
                Ok(_) => (),
            },
            gst::StateChange::ReadyToNull => match self.unprepare(element) {
                Err(_) => return gst::StateChangeReturn::Failure,
                Ok(_) => (),
            },
            _ => (),
        }

        let mut ret = element.parent_change_state(transition);
        if ret == gst::StateChangeReturn::Failure {
            return ret;
        }

        match transition {
            gst::StateChange::ReadyToPaused => {
                ret = gst::StateChangeReturn::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => match self.start(element) {
                Err(_) => return gst::StateChangeReturn::Failure,
                Ok(_) => (),
            },
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                state.need_initial_events = true;
            }
            _ => (),
        }

        ret
    }
}

struct UdpSrcStatic;

impl ImplTypeStatic<Element> for UdpSrcStatic {
    fn get_name(&self) -> &str {
        "UdpSrc"
    }

    fn new(&self, element: &Element) -> Box<ElementImpl<Element>> {
        UdpSrc::init(element)
    }

    fn class_init(&self, klass: &mut ElementClass) {
        UdpSrc::class_init(klass);
    }
}

pub fn register(plugin: &gst::Plugin) {
    let udpsrc_static = UdpSrcStatic;
    let type_ = register_type(udpsrc_static);
    gst::Element::register(plugin, "ts-udpsrc", 0, type_);
}
