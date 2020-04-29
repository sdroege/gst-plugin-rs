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

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use glib::{glib_object_impl, glib_object_subclass};

use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_element_error, gst_error, gst_error_msg, gst_log, gst_trace};

use lazy_static::lazy_static;

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::u16;
use std::u32;

use tokio::io::AsyncReadExt;

use crate::runtime::prelude::*;
use crate::runtime::{Context, PadSrc, PadSrcRef, Task};

use super::socket::{Socket, SocketError, SocketRead, SocketState};

const DEFAULT_HOST: Option<&str> = Some("127.0.0.1");
const DEFAULT_PORT: i32 = 4953;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_BLOCKSIZE: u32 = 4096;
const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;

#[derive(Debug, Clone)]
struct Settings {
    host: Option<String>,
    port: i32,
    caps: Option<gst::Caps>,
    blocksize: u32,
    context: String,
    context_wait: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            host: DEFAULT_HOST.map(Into::into),
            port: DEFAULT_PORT,
            caps: DEFAULT_CAPS,
            blocksize: DEFAULT_BLOCKSIZE,
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
        }
    }
}

static PROPERTIES: [subclass::Property; 6] = [
    subclass::Property("host", |name| {
        glib::ParamSpec::string(
            name,
            "Host",
            "The host IP address to receive packets from",
            DEFAULT_HOST,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("port", |name| {
        glib::ParamSpec::int(
            name,
            "Port",
            "Port to receive packets from",
            0,
            u16::MAX as i32,
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
    subclass::Property("blocksize", |name| {
        glib::ParamSpec::uint(
            name,
            "Blocksize",
            "Size in bytes to read per buffer (-1 = default)",
            0,
            u32::MAX,
            DEFAULT_BLOCKSIZE,
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

#[derive(Debug, Default)]
struct TcpClientSrcPadHandlerInner {
    state: FutMutex<TcpClientSrcPadHandlerState>,
    configured_caps: StdMutex<Option<gst::Caps>>,
}

#[derive(Clone, Debug, Default)]
struct TcpClientSrcPadHandler(Arc<TcpClientSrcPadHandlerInner>);

impl TcpClientSrcPadHandler {
    fn prepare(&self, caps: Option<gst::Caps>) {
        self.0
            .state
            .try_lock()
            .expect("State locked elsewhere")
            .caps = caps;
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
    src_pad_handler: TcpClientSrcPadHandler,
    task: Task,
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
        let settings = self.settings.lock().unwrap().clone();

        gst_debug!(CAT, obj: element, "Preparing");

        let context =
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;

        let host: IpAddr = match settings.host {
            None => {
                return Err(gst_error_msg!(
                    gst::ResourceError::Settings,
                    ["No host set"]
                ));
            }
            Some(ref host) => match host.parse() {
                Err(err) => {
                    return Err(gst_error_msg!(
                        gst::ResourceError::Settings,
                        ["Invalid host '{}' set: {}", host, err]
                    ));
                }
                Ok(host) => host,
            },
        };
        let port = settings.port;

        let buffer_pool = gst::BufferPool::new();
        let mut config = buffer_pool.get_config();
        config.set_params(None, settings.blocksize, 0, 0);
        buffer_pool.set_config(config).map_err(|_| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["Failed to configure buffer pool"]
            )
        })?;

        let saddr = SocketAddr::new(host, port as u16);
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

        *self.socket.lock().unwrap() = Some(socket);

        self.task.prepare(context).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Error preparing Task: {:?}", err]
            )
        })?;
        self.src_pad_handler.prepare(settings.caps);

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Unpreparing");

        *self.socket.lock().unwrap() = None;
        self.task.unprepare().unwrap();

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

    fn start_task(&self, element: &gst::Element, socket: &Socket<TcpClientReader>) {
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
        let src_pad_handler = TcpClientSrcPadHandler::default();

        Self {
            src_pad: PadSrc::new(
                gst::Pad::new_from_template(&klass.get_pad_template("src").unwrap(), Some("src")),
                src_pad_handler.clone(),
            ),
            src_pad_handler,
            task: Task::default(),
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
            subclass::Property("host", ..) => {
                settings.host = value.get().expect("type checked upstream");
            }
            subclass::Property("port", ..) => {
                settings.port = value.get_some().expect("type checked upstream");
            }
            subclass::Property("caps", ..) => {
                settings.caps = value.get().expect("type checked upstream");
            }
            subclass::Property("blocksize", ..) => {
                settings.blocksize = value.get_some().expect("type checked upstream");
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
            subclass::Property("caps", ..) => Ok(settings.caps.to_value()),
            subclass::Property("blocksize", ..) => Ok(settings.blocksize.to_value()),
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
