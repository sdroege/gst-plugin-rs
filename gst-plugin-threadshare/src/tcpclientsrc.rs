// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2018 LEE Dongjun <redongjun@gmail.com>
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

use std::io;
use std::sync::Mutex;
use std::u16;

use futures;
use futures::future;
use futures::{Async, Future, Poll};
use tokio::io::AsyncRead;
use tokio::net;

use either::Either;

use rand;

use iocontext::*;
use socket::*;

const DEFAULT_ADDRESS: Option<&str> = Some("127.0.0.1");
const DEFAULT_PORT: u32 = 5000;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_CHUNK_SIZE: u32 = 4096;
const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;

#[derive(Debug, Clone)]
struct Settings {
    address: Option<String>,
    port: u32,
    caps: Option<gst::Caps>,
    chunk_size: u32,
    context: String,
    context_wait: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            address: DEFAULT_ADDRESS.map(Into::into),
            port: DEFAULT_PORT,
            caps: DEFAULT_CAPS,
            chunk_size: DEFAULT_CHUNK_SIZE,
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
        }
    }
}

static PROPERTIES: [subclass::Property; 6] = [
    subclass::Property("address", |name| {
        glib::ParamSpec::string(
            name,
            "Address",
            "Address to receive packets from",
            DEFAULT_ADDRESS,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("port", |name| {
        glib::ParamSpec::uint(
            name,
            "Port",
            "Port to receive packets from",
            0,
            u16::MAX as u32,
            DEFAULT_PORT,
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
    subclass::Property("chunk-size", |name| {
        glib::ParamSpec::uint(
            name,
            "Chunk Size",
            "Chunk Size",
            0,
            u16::MAX as u32,
            DEFAULT_CHUNK_SIZE,
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

pub struct TcpClientReader {
    connect_future: net::tcp::ConnectFuture,
    socket: Option<net::TcpStream>,
}

impl TcpClientReader {
    pub fn new(connect_future: net::tcp::ConnectFuture) -> Self {
        Self {
            connect_future,
            socket: None,
        }
    }
}

impl SocketRead for TcpClientReader {
    const DO_TIMESTAMP: bool = false;

    fn poll_read(
        &mut self,
        buf: &mut [u8],
    ) -> Poll<(usize, Option<std::net::SocketAddr>), io::Error> {
        let socket = match self.socket {
            Some(ref mut socket) => socket,
            None => match self.connect_future.poll() {
                Ok(Async::Ready(stream)) => {
                    self.socket = Some(stream);
                    self.socket.as_mut().unwrap()
                }
                Err(err) => {
                    return Err(err);
                }
                _ => return Ok(Async::NotReady),
            },
        };
        match socket.poll_read(buf) {
            Ok(Async::Ready(result)) => {
                return Ok(Async::Ready((result, None)));
            }
            Ok(Async::NotReady) => {
                return Ok(Async::NotReady);
            }
            Err(result) => return Err(result),
        };
    }
}

struct State {
    io_context: Option<IOContext>,
    pending_future_id: Option<PendingFutureId>,
    socket: Option<Socket<TcpClientReader>>,
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

struct TcpClientSrc {
    cat: gst::DebugCategory,
    src_pad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl TcpClientSrc {
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
                q.set(false, 0.into(), 0.into());
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
                        .unwrap_or_else(|| caps.clone())
                } else {
                    q.get_filter()
                        .map(|f| f.to_owned())
                        .unwrap_or_else(|| gst::Caps::new_any())
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

        if buffer.get_size() == 0 {
            events.push(gst::Event::new_eos().build());
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
        use std::net::{IpAddr, SocketAddr};

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

        let saddr = SocketAddr::new(addr, port as u16);
        gst_debug!(self.cat, obj: element, "Connecting to {:?}", saddr);
        let socket = net::TcpStream::connect(&saddr);

        let buffer_pool = gst::BufferPool::new();
        let mut config = buffer_pool.get_config();
        config.set_params(None, settings.chunk_size, 0, 0);
        buffer_pool.set_config(config).map_err(|_| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["Failed to configure buffer pool"]
            )
        })?;

        let socket = Socket::new(
            element.upcast_ref(),
            TcpClientReader::new(socket),
            buffer_pool,
        );

        let element_clone = element.clone();
        let element_clone2 = element.clone();
        socket
            .schedule(
                &io_context,
                move |(buffer, _)| {
                    let tcpclientsrc = Self::from_instance(&element_clone);
                    tcpclientsrc.push_buffer(&element_clone, buffer)
                },
                move |err| {
                    let tcpclientsrc = Self::from_instance(&element_clone2);
                    gst_error!(tcpclientsrc.cat, obj: &element_clone2, "Got error {}", err);
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

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
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

    fn start(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Starting");
        let state = self.state.lock().unwrap();

        if let Some(ref socket) = state.socket {
            socket.unpause(None, None);
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

impl ObjectSubclass for TcpClientSrc {
    const NAME: &'static str = "RsTsTcpClientSrc";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing TCP client source",
            "Source/Network",
            "Receives data over the network via TCP",
            "Sebastian Dröge <sebastian@centricular.com>, LEE Dongjun <redongjun@gmail.com>",
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

        klass.install_properties(&PROPERTIES);
    }

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("src").unwrap();
        let src_pad = gst::Pad::new_from_template(&templ, Some("src"));

        src_pad.set_event_function(|pad, parent, event| {
            TcpClientSrc::catch_panic_pad_function(
                parent,
                || false,
                |tcpclientsrc, element| tcpclientsrc.src_event(pad, element, event),
            )
        });
        src_pad.set_query_function(|pad, parent, query| {
            TcpClientSrc::catch_panic_pad_function(
                parent,
                || false,
                |tcpclientsrc, element| tcpclientsrc.src_query(pad, element, query),
            )
        });

        Self {
            cat: gst::DebugCategory::new(
                "ts-tcpclientsrc",
                gst::DebugColorFlags::empty(),
                Some("Thread-sharing TCP Client source"),
            ),
            src_pad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for TcpClientSrc {
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
            subclass::Property("caps", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.caps = value.get();
            }
            subclass::Property("chunk-size", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.chunk_size = value.get().unwrap();
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
                let settings = self.settings.lock().unwrap();
                Ok(settings.address.to_value())
            }
            subclass::Property("port", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.port.to_value())
            }
            subclass::Property("caps", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.caps.to_value())
            }
            subclass::Property("chunk-size", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.chunk_size.to_value())
            }
            subclass::Property("context", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.context.to_value())
            }
            subclass::Property("context-wait", ..) => {
                let settings = self.settings.lock().unwrap();
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

impl ElementImpl for TcpClientSrc {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(self.cat, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare(element)
                    .map_err(|err| {
                        element.post_error_message(&err);
                        gst::StateChangeError
                    })
                    .and_then(|_| self.start(element).map_err(|_| gst::StateChangeError))?;
            }
            gst::StateChange::PlayingToPaused => {
                self.stop(element)
                    .and_then(|_| self.unprepare(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::Success;
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
    gst::Element::register(
        Some(plugin),
        "ts-tcpclientsrc",
        gst::Rank::None,
        TcpClientSrc::get_type(),
    )
}
