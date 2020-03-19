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

use futures::future::BoxFuture;
use futures::lock::Mutex as FutMutex;
use futures::prelude::*;

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use glib::{glib_object_impl, glib_object_subclass};

use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_element_error, gst_error, gst_error_msg, gst_log, gst_trace};

use lazy_static::lazy_static;

use rand;

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::u16;

use tokio::io::AsyncReadExt;

use crate::runtime::prelude::*;
use crate::runtime::{Context, PadSrc, PadSrcRef};

use super::socket::{Socket, SocketError, SocketRead, SocketState, SocketStream};

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

struct TcpClientReaderInner {
    socket: tokio::net::TcpStream,
}

pub struct TcpClientReader(Arc<FutMutex<TcpClientReaderInner>>);

impl TcpClientReader {
    pub fn new(socket: tokio::net::TcpStream) -> Self {
        TcpClientReader(Arc::new(FutMutex::new(TcpClientReaderInner { socket })))
    }
}

impl SocketRead for TcpClientReader {
    const DO_TIMESTAMP: bool = false;

    fn read<'buf>(
        &self,
        buffer: &'buf mut [u8],
    ) -> BoxFuture<'buf, io::Result<(usize, Option<std::net::SocketAddr>)>> {
        let this = Arc::clone(&self.0);

        async move {
            this.lock()
                .await
                .socket
                .read(buffer)
                .await
                .map(|read_size| (read_size, None))
        }
        .boxed()
    }
}

#[derive(Debug)]
struct TcpClientSrcPadHandlerState {
    need_initial_events: bool,
    need_segment: bool,
    caps: Option<gst::Caps>,
}

impl Default for TcpClientSrcPadHandlerState {
    fn default() -> Self {
        TcpClientSrcPadHandlerState {
            need_initial_events: true,
            need_segment: true,
            caps: None,
        }
    }
}

#[derive(Debug)]
struct TcpClientSrcPadHandlerInner {
    state: FutMutex<TcpClientSrcPadHandlerState>,
    configured_caps: StdMutex<Option<gst::Caps>>,
}

impl TcpClientSrcPadHandlerInner {
    fn new(caps: Option<gst::Caps>) -> Self {
        TcpClientSrcPadHandlerInner {
            state: FutMutex::new(TcpClientSrcPadHandlerState {
                caps,
                ..Default::default()
            }),
            configured_caps: StdMutex::new(None),
        }
    }
}

#[derive(Clone, Debug)]
struct TcpClientSrcPadHandler(Arc<TcpClientSrcPadHandlerInner>);

impl TcpClientSrcPadHandler {
    fn new(caps: Option<gst::Caps>) -> Self {
        TcpClientSrcPadHandler(Arc::new(TcpClientSrcPadHandlerInner::new(caps)))
    }

    fn reset(&self, pad: &PadSrcRef<'_>) {
        // Precondition: task must be stopped
        // TODO: assert the task state when Task & PadSrc are separated

        gst_debug!(CAT, obj: pad.gst_pad(), "Resetting handler");

        *self.0.state.try_lock().expect("State locked elsewhere") = Default::default();
        *self.0.configured_caps.lock().unwrap() = None;

        gst_debug!(CAT, obj: pad.gst_pad(), "Handler reset");
    }

    fn flush(&self, pad: &PadSrcRef<'_>) {
        // Precondition: task must be stopped
        // TODO: assert the task state when Task & PadSrc are separated

        gst_debug!(CAT, obj: pad.gst_pad(), "Flushing");

        self.0
            .state
            .try_lock()
            .expect("state is locked elsewhere")
            .need_segment = true;

        gst_debug!(CAT, obj: pad.gst_pad(), "Flushed");
    }

    fn start_task(
        &self,
        pad: PadSrcRef<'_>,
        element: &gst::Element,
        socket_stream: SocketStream<TcpClientReader>,
    ) {
        let this = self.clone();
        let pad_weak = pad.downgrade();
        let element = element.clone();
        let socket_stream = Arc::new(FutMutex::new(socket_stream));

        pad.start_task(move || {
            let this = this.clone();
            let pad_weak = pad_weak.clone();
            let element = element.clone();
            let socket_stream = socket_stream.clone();

            async move {
                let item = socket_stream.lock().await.next().await;

                let pad = pad_weak.upgrade().expect("PadSrc no longer exists");
                let buffer = match item {
                    Some(Ok((buffer, _))) => buffer,
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

                let res = this.push_buffer(&pad, &element, buffer).await;

                match res {
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

        if buffer.get_size() == 0 {
            let event = gst::Event::new_eos().build();
            pad.push_event(event).await;
            return Ok(gst::FlowSuccess::Ok);
        }

        pad.push(buffer).await
    }
}

impl PadSrcHandler for TcpClientSrcPadHandler {
    type ElementImpl = TcpClientSrc;

    fn src_event(
        &self,
        pad: &PadSrcRef,
        tcpclientsrc: &TcpClientSrc,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                tcpclientsrc.flush_start(element);

                true
            }
            EventView::FlushStop(..) => {
                tcpclientsrc.flush_stop(element);

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
        _tcpclientsrc: &TcpClientSrc,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", query);
        let ret = match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                q.set(false, 0.into(), gst::CLOCK_TIME_NONE);
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

struct TcpClientSrc {
    src_pad: PadSrc,
    src_pad_handler: StdMutex<Option<TcpClientSrcPadHandler>>,
    socket: StdMutex<Option<Socket<TcpClientReader>>>,
    settings: StdMutex<Settings>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-tcpclientsrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing TCP Client source"),
    );
}

impl TcpClientSrc {
    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let mut socket_storage = self.socket.lock().unwrap();
        let settings = self.settings.lock().unwrap().clone();

        gst_debug!(CAT, obj: element, "Preparing");

        let context =
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
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

        let buffer_pool = gst::BufferPool::new();
        let mut config = buffer_pool.get_config();
        config.set_params(None, settings.chunk_size, 0, 0);
        buffer_pool.set_config(config).map_err(|_| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["Failed to configure buffer pool"]
            )
        })?;

        let saddr = SocketAddr::new(addr, port as u16);
        let element_clone = element.clone();
        let socket = Socket::new(element.upcast_ref(), buffer_pool, async move {
            gst_debug!(CAT, obj: &element_clone, "Connecting to {:?}", saddr);
            let socket = tokio::net::TcpStream::connect(saddr)
                .await
                .map_err(SocketError::Io)?;
            Ok(TcpClientReader::new(socket))
        })
        .map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to prepare socket {:?}", err]
            )
        })?;

        *socket_storage = Some(socket);
        drop(socket_storage);

        let src_pad_handler = TcpClientSrcPadHandler::new(settings.caps);

        self.src_pad
            .prepare(context, &src_pad_handler)
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing src_pads: {:?}", err]
                )
            })?;

        *self.src_pad_handler.lock().unwrap() = Some(src_pad_handler);

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Unpreparing");

        if let Some(socket) = self.socket.lock().unwrap().take() {
            drop(socket);
        }

        let _ = self.src_pad.unprepare();
        *self.src_pad_handler.lock().unwrap() = None;

        gst_debug!(CAT, obj: element, "Unprepared");

        Ok(())
    }

    fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Stopping");

        // Now stop the task if it was still running, blocking
        // until this has actually happened
        self.src_pad.stop_task();

        self.src_pad_handler
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .reset(&self.src_pad.as_ref());

        gst_debug!(CAT, obj: element, "Stopped");

        Ok(())
    }

    fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let socket = self.socket.lock().unwrap();
        if let Some(socket) = socket.as_ref() {
            if socket.state() == SocketState::Started {
                gst_debug!(CAT, obj: element, "Already started");
                return Err(());
            }

            gst_debug!(CAT, obj: element, "Starting");

            self.start_unchecked(element, socket);

            gst_debug!(CAT, obj: element, "Started");

            Ok(())
        } else {
            Err(())
        }
    }

    fn flush_stop(&self, element: &gst::Element) {
        // Keep the lock on the `socket` until `flush_stop` is complete
        // so as to prevent race conditions due to concurrent state transitions.
        // Note that this won't deadlock as it doesn't lock the `SocketStream`
        // in use within the `src_pad`'s `Task`.
        let socket = self.socket.lock().unwrap();
        let socket = socket.as_ref().unwrap();
        if socket.state() == SocketState::Started {
            gst_debug!(CAT, obj: element, "Already started");
            return;
        }

        gst_debug!(CAT, obj: element, "Stopping Flush");

        self.src_pad.stop_task();

        self.src_pad_handler
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .flush(&self.src_pad.as_ref());

        self.start_unchecked(element, socket);

        gst_debug!(CAT, obj: element, "Stopped Flush");
    }

    fn start_unchecked(&self, element: &gst::Element, socket: &Socket<TcpClientReader>) {
        let socket_stream = socket
            .start(element.get_clock(), Some(element.get_base_time()))
            .unwrap();

        self.src_pad_handler
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .start_task(self.src_pad.as_ref(), element, socket_stream);
    }

    fn flush_start(&self, element: &gst::Element) {
        let socket = self.socket.lock().unwrap();
        gst_debug!(CAT, obj: element, "Starting Flush");

        if let Some(socket) = socket.as_ref() {
            socket.pause();
        }

        self.src_pad.cancel_task();

        gst_debug!(CAT, obj: element, "Flush Started");
    }

    fn pause(&self, element: &gst::Element) -> Result<(), ()> {
        let socket = self.socket.lock().unwrap();
        gst_debug!(CAT, obj: element, "Pausing");

        if let Some(socket) = socket.as_ref() {
            socket.pause();
        }

        self.src_pad.pause_task();

        gst_debug!(CAT, obj: element, "Paused");

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
        let src_pad = PadSrc::new_from_template(&templ, Some("src"));

        Self {
            src_pad,
            src_pad_handler: StdMutex::new(None),
            socket: StdMutex::new(None),
            settings: StdMutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for TcpClientSrc {
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
            subclass::Property("caps", ..) => {
                settings.caps = value.get().expect("type checked upstream");
            }
            subclass::Property("chunk-size", ..) => {
                settings.chunk_size = value.get_some().expect("type checked upstream");
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
            subclass::Property("address", ..) => Ok(settings.address.to_value()),
            subclass::Property("port", ..) => Ok(settings.port.to_value()),
            subclass::Property("caps", ..) => Ok(settings.caps.to_value()),
            subclass::Property("chunk-size", ..) => Ok(settings.chunk_size.to_value()),
            subclass::Property("context", ..) => Ok(settings.context.to_value()),
            subclass::Property("context-wait", ..) => Ok(settings.context_wait.to_value()),
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

impl ElementImpl for TcpClientSrc {
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
        "ts-tcpclientsrc",
        gst::Rank::None,
        TcpClientSrc::get_type(),
    )
}
