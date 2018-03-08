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

use gst_plugin::properties::*;
use gst_plugin::object::*;
use gst_plugin::element::*;
use gst_plugin::bytes::*;

use std::sync::{Arc, Mutex, Weak};
use std::{cmp, mem, ops, i32, u16};
use std::io::Write;

use futures::{Async, Future, Poll, Stream};
use futures::{future, task};
use futures::sync::oneshot;
use tokio::executor::thread_pool;
use tokio::reactor;
use tokio::net;
use std::thread;
use std::collections::HashMap;

lazy_static!{
    static ref CONTEXTS: Mutex<HashMap<String, Weak<IOContextInner>>> = Mutex::new(HashMap::new());
    static ref CONTEXT_CAT: gst::DebugCategory = gst::DebugCategory::new(
                "ts-context",
                gst::DebugColorFlags::empty(),
                "Thread-sharing Context",
            );
    static ref SOCKET_CAT: gst::DebugCategory = gst::DebugCategory::new(
                "ts-socket",
                gst::DebugColorFlags::empty(),
                "Thread-sharing Socket",
            );
}

#[derive(Clone)]
struct IOContext(Arc<IOContextInner>);

struct IOContextInner {
    name: String,
    reactor: reactor::Background,
    pool: thread_pool::ThreadPool,
}

impl Drop for IOContextInner {
    fn drop(&mut self) {
        let mut contexts = CONTEXTS.lock().unwrap();
        gst_debug!(CONTEXT_CAT, "Finalizing context '{}'", self.name);
        contexts.remove(&self.name);
    }
}

impl IOContext {
    fn new(name: &str, n_threads: usize) -> Self {
        let mut contexts = CONTEXTS.lock().unwrap();
        if let Some(context) = contexts.get(name) {
            if let Some(context) = context.upgrade() {
                gst_debug!(CONTEXT_CAT, "Reusing existing context '{}'", name);
                return IOContext(context);
            }
        }

        let reactor = reactor::Reactor::new().unwrap().background().unwrap();

        let handle = reactor.handle().clone();

        let mut pool_builder = thread_pool::Builder::new();
        pool_builder.around_worker(move |w, enter| {
            ::tokio_reactor::with_default(&handle, enter, |_| {
                w.run();
            });
        });

        if n_threads > 0 {
            pool_builder.pool_size(n_threads);
        }
        let pool = pool_builder.build();

        let context = Arc::new(IOContextInner {
            name: name.into(),
            reactor,
            pool,
        });
        contexts.insert(name.into(), Arc::downgrade(&context));

        gst_debug!(CONTEXT_CAT, "Created new context '{}'", name);
        IOContext(context)
    }

    fn spawn<F>(&self, future: F)
    where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        self.0.pool.spawn(future);
    }
}

#[derive(Clone)]
struct Socket(Arc<Mutex<SocketInner>>);

#[derive(PartialEq, Eq, Debug)]
enum SocketState {
    Unscheduled,
    Scheduled,
    Running,
    Shutdown,
}

struct SocketInner {
    element: Element,
    state: SocketState,
    socket: net::UdpSocket,
    buffer_pool: gst::BufferPool,
    current_task: Option<task::Task>,
    shutdown_receiver: Option<oneshot::Receiver<()>>,
    clock: Option<gst::Clock>,
    base_time: Option<gst::ClockTime>,
}

impl Socket {
    fn new(element: &Element, socket: net::UdpSocket, buffer_pool: gst::BufferPool) -> Self {
        Socket(Arc::new(Mutex::new(SocketInner {
            element: element.clone(),
            state: SocketState::Unscheduled,
            socket: socket,
            buffer_pool: buffer_pool,
            current_task: None,
            shutdown_receiver: None,
            clock: None,
            base_time: None,
        })))
    }

    fn schedule<F: FnMut(gst::Buffer) -> Result<(), gst::FlowError> + Send + 'static>(
        &self,
        io_context: &IOContext,
        func: F,
    ) {
        // Ready->Paused
        //
        // Need to wait for a possible shutdown to finish first
        // spawn() on the reactor, change state to Scheduled
        let stream = SocketStream(self.clone(), None);

        let mut inner = self.0.lock().unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Scheduling socket");
        if inner.state == SocketState::Scheduled {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket already scheduled");
            return;
        }

        assert_eq!(inner.state, SocketState::Unscheduled);
        inner.state = SocketState::Scheduled;
        inner.buffer_pool.set_active(true).unwrap();

        let (sender, receiver) = oneshot::channel::<()>();
        inner.shutdown_receiver = Some(receiver);

        let element_clone = inner.element.clone();
        io_context.spawn(stream.for_each(func).then(move |res| {
            gst_debug!(SOCKET_CAT, obj: &element_clone, "Socket finished {:?}", res);
            // TODO: Do something with errors here?
            let _ = sender.send(());

            Ok(())
        }));
    }

    fn unpause(&self, clock: gst::Clock, base_time: gst::ClockTime) {
        // Paused->Playing
        //
        // Change state to Running and signal task
        let mut inner = self.0.lock().unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Unpausing socket");
        if inner.state == SocketState::Running {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket already unpaused");
            return;
        }

        assert_eq!(inner.state, SocketState::Scheduled);
        inner.state = SocketState::Running;
        inner.clock = Some(clock);
        inner.base_time = Some(base_time);

        if let Some(task) = inner.current_task.take() {
            task.notify();
        }
    }

    fn pause(&self) {
        // Playing->Paused
        //
        // Change state to Scheduled and signal task

        let mut inner = self.0.lock().unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Pausing socket");
        if inner.state == SocketState::Scheduled {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket already paused");
            return;
        }

        assert_eq!(inner.state, SocketState::Running);
        inner.state = SocketState::Scheduled;
        inner.clock = None;
        inner.base_time = None;

        if let Some(task) = inner.current_task.take() {
            task.notify();
        }
    }

    fn shutdown(&self) {
        // Paused->Ready
        //
        // Change state to Shutdown and signal task, wait for our future to be finished
        // Requires scheduled function to be unblocked! Pad must be deactivated before

        let mut inner = self.0.lock().unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Shutting down socket");
        if inner.state == SocketState::Unscheduled {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket already shut down");
            return;
        }

        assert!(inner.state == SocketState::Scheduled || inner.state == SocketState::Running);
        inner.state = SocketState::Shutdown;

        if let Some(task) = inner.current_task.take() {
            task.notify();
        }

        let shutdown_receiver = inner.shutdown_receiver.take().unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Waiting for socket to shut down");
        drop(inner);

        shutdown_receiver.wait().unwrap();

        let mut inner = self.0.lock().unwrap();
        inner.state = SocketState::Unscheduled;
        inner.buffer_pool.set_active(false).unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket shut down");
    }
}

impl Drop for SocketInner {
    fn drop(&mut self) {
        assert_eq!(self.state, SocketState::Unscheduled);
    }
}

struct SocketStream(Socket, Option<gst::MappedBuffer<gst::buffer::Writable>>);

impl Stream for SocketStream {
    type Item = gst::Buffer;
    type Error = gst::FlowError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut inner = (self.0).0.lock().unwrap();
        if inner.state == SocketState::Shutdown {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket shutting down");
            return Ok(Async::Ready(None));
        } else if inner.state == SocketState::Scheduled {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket not running");
            inner.current_task = Some(task::current());
            return Ok(Async::NotReady);
        }

        assert_ne!(inner.state, SocketState::Unscheduled);

        gst_debug!(SOCKET_CAT, obj: &inner.element, "Trying to read data");
        let (len, time) = {
            let mut buffer = match self.1 {
                Some(ref mut buffer) => buffer,
                None => match inner.buffer_pool.acquire_buffer(None) {
                    Ok(buffer) => {
                        self.1 = Some(buffer.into_mapped_buffer_writable().unwrap());
                        self.1.as_mut().unwrap()
                    }
                    Err(err) => {
                        gst_debug!(SOCKET_CAT, obj: &inner.element, "Failed to acquire buffer {:?}", err);
                        return Err(err.into_result().unwrap_err());
                    }
                },
            };

            match inner.socket.poll_recv(buffer.as_mut_slice()) {
                Ok(Async::NotReady) => {
                    gst_debug!(SOCKET_CAT, obj: &inner.element, "No data available");
                    inner.current_task = Some(task::current());
                    return Ok(Async::NotReady);
                }
                Err(err) => {
                    gst_debug!(SOCKET_CAT, obj: &inner.element, "Read error {:?}", err);
                    return Err(gst::FlowError::Error);
                }
                Ok(Async::Ready(len)) => {
                    let time = inner.clock.as_ref().unwrap().get_time();
                    let dts = time - inner.base_time.unwrap();
                    gst_debug!(SOCKET_CAT, obj: &inner.element, "Read {} bytes at {} (clock {})", len, dts, time);
                    (len, dts)
                }
            }
        };

        let mut buffer = self.1.take().unwrap().into_buffer();
        {
            let buffer = buffer.get_mut().unwrap();
            if len < buffer.get_size() {
                buffer.set_size(len);
            }
            buffer.set_dts(time);
        }

        Ok(Async::Ready(Some(buffer)))
    }
}

const DEFAULT_ADDRESS: Option<&'static str> = Some("127.0.0.1");
const DEFAULT_PORT: u32 = 5000;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_MTU: u32 = 1500;
const DEFAULT_CONTEXT: &'static str = "";
const DEFAULT_CONTEXT_THREADS: u32 = 0;

#[derive(Debug, Clone)]
struct Settings {
    address: Option<String>,
    port: u32,
    caps: Option<gst::Caps>,
    mtu: u32,
    context: String,
    context_threads: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            address: DEFAULT_ADDRESS.map(Into::into),
            port: DEFAULT_PORT,
            caps: DEFAULT_CAPS,
            mtu: DEFAULT_MTU,
            context: DEFAULT_CONTEXT.into(),
            context_threads: DEFAULT_CONTEXT_THREADS,
        }
    }
}

static PROPERTIES: [Property; 6] = [
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
        "context-threads",
        "Context Threads",
        "Number of threads for the context thread-pool if we create it",
        (0, u16::MAX as u32),
        DEFAULT_CONTEXT_THREADS,
        PropertyMutability::ReadWrite,
    ),
];

struct State {
    io_context: Option<IOContext>,
    socket: Option<Socket>,
    need_initial_events: bool,
}

impl Default for State {
    fn default() -> State {
        State {
            io_context: None,
            socket: None,
            need_initial_events: true,
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

    fn catch_panic_pad_function<T, F: FnOnce(&Self, &Element) -> T, G: FnOnce() -> T>(
        parent: &Option<gst::Object>,
        fallback: G,
        f: F,
    ) -> T {
        let element = parent
            .as_ref()
            .cloned()
            .unwrap()
            .downcast::<Element>()
            .unwrap();
        let udpsrc = element.get_impl().downcast_ref::<UdpSrc>().unwrap();
        element.catch_panic(fallback, |element| f(udpsrc, element))
    }

    fn src_event(&self, pad: &gst::Pad, element: &Element, mut event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        let mut handled = true;
        match event.view() {
            EventView::FlushStart(..) => {}
            EventView::FlushStop(..) => {}
            _ => (),
        }

        if handled {
            gst_log!(self.cat, obj: pad, "Handled event {:?}", event);
            pad.event_default(Some(element), event)
        } else {
            gst_log!(self.cat, obj: pad, "Didn't handle event {:?}", event);
            false
        }
    }

    fn src_query(&self, pad: &gst::Pad, element: &Element, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        gst_log!(self.cat, obj: pad, "Handling query {:?}", query);
        match query.view_mut() {
            _ => (),
        };

        gst_log!(self.cat, obj: pad, "Forwarding query {:?}", query);
        pad.query_default(Some(element), query)
    }

    fn prepare(&self, element: &Element) -> Result<(), ()> {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

        gst_debug!(self.cat, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        // TODO: Error handling
        let mut state = self.state.lock().unwrap();

        let io_context = IOContext::new(&settings.context, settings.context_threads as usize);

        let addr: IpAddr = match settings.address {
            None => return Err(()),
            Some(addr) => match addr.parse() {
                Err(_) => return Err(()),
                Ok(addr) => addr,
            },
        };
        let port = settings.port;

        // TODO: TTL, multicast loopback, etc
        let socket = if addr.is_multicast() {
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

            let socket = net::UdpSocket::bind(&saddr).unwrap();

            // TODO: Multicast interface configuration, going to be tricky
            match addr {
                IpAddr::V4(addr) => {
                    socket
                        .join_multicast_v4(&addr, &Ipv4Addr::new(0, 0, 0, 0))
                        .unwrap();
                }
                IpAddr::V6(addr) => {
                    socket.join_multicast_v6(&addr, 0).unwrap();
                }
            }

            socket
        } else {
            let saddr = SocketAddr::new(addr, port as u16);
            gst_debug!(self.cat, obj: element, "Binding to {:?}", saddr);
            let socket = net::UdpSocket::bind(&saddr).unwrap();

            socket
        };

        let buffer_pool = gst::BufferPool::new();
        let mut config = buffer_pool.get_config();
        config.set_params(None, settings.mtu, 0, 0);
        buffer_pool.set_config(config).unwrap();

        let socket = Socket::new(element, socket, buffer_pool);

        let element_clone = element.clone();
        socket.schedule(&io_context, move |buffer| {
            let udpsrc = element_clone.get_impl().downcast_ref::<UdpSrc>().unwrap();

            let mut state = udpsrc.state.lock().unwrap();
            if state.need_initial_events {
                gst_debug!(udpsrc.cat, obj: &element_clone, "Pushing initial events");

                // TODO: Invent a stream id
                udpsrc
                    .src_pad
                    .push_event(gst::Event::new_stream_start("meh").build());
                udpsrc.src_pad.push_event(
                    gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new())
                        .build(),
                );
                if let Some(caps) = udpsrc
                    .settings
                    .lock()
                    .unwrap()
                    .caps
                    .as_ref()
                    .map(|c| c.clone())
                {
                    udpsrc
                        .src_pad
                        .push_event(gst::Event::new_caps(&caps).build());
                }
                state.need_initial_events = false;
            }

            // TODO: Error handling
            udpsrc.src_pad.push(buffer).into_result().unwrap();

            Ok(())
        });

        state.socket = Some(socket);
        state.io_context = Some(io_context);

        gst_debug!(self.cat, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Unpreparing");

        let mut state = self.state.lock().unwrap();

        if let Some(ref socket) = state.socket {
            socket.shutdown();
        }

        *state = State::default();

        gst_debug!(self.cat, obj: element, "Unprepared");
        Ok(())
    }

    fn start(&self, element: &Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Starting");
        let mut state = self.state.lock().unwrap();

        if let Some(ref socket) = state.socket {
            socket.unpause(element.get_clock().unwrap(), element.get_base_time());
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
            Property::UInt("context-threads", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_threads = value.get().unwrap();
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
            Property::UInt("context-threads", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context_threads.to_value())
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
                Err(_) => return gst::StateChangeReturn::Failure,
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
