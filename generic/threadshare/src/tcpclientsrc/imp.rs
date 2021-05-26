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

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_log, gst_trace};

use once_cell::sync::Lazy;

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use std::u16;
use std::u32;

use tokio::io::AsyncReadExt;

use crate::runtime::prelude::*;
use crate::runtime::task;
use crate::runtime::{Context, PadSrc, PadSrcRef, PadSrcWeak, Task, TaskState};

use crate::socket::{Socket, SocketError, SocketRead};

const DEFAULT_HOST: Option<&str> = Some("127.0.0.1");
const DEFAULT_PORT: i32 = 4953;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_BLOCKSIZE: u32 = 4096;
const DEFAULT_CONTEXT: &str = "";
// FIXME use Duration::ZERO when MSVC >= 1.53.2
const DEFAULT_CONTEXT_WAIT: Duration = Duration::from_nanos(0);

#[derive(Debug, Clone)]
struct Settings {
    host: Option<String>,
    port: i32,
    caps: Option<gst::Caps>,
    blocksize: u32,
    context: String,
    context_wait: Duration,
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

struct TcpClientReader(tokio::net::TcpStream);

impl TcpClientReader {
    pub fn new(socket: tokio::net::TcpStream) -> Self {
        TcpClientReader(socket)
    }
}

impl SocketRead for TcpClientReader {
    const DO_TIMESTAMP: bool = false;

    fn read<'buf>(
        &'buf mut self,
        buffer: &'buf mut [u8],
    ) -> BoxFuture<'buf, io::Result<(usize, Option<std::net::SocketAddr>)>> {
        async move { self.0.read(buffer).await.map(|read_size| (read_size, None)) }.boxed()
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

    async fn reset_state(&self) {
        *self.0.configured_caps.lock().unwrap() = None;
    }

    async fn set_need_segment(&self) {
        self.0.state.lock().await.need_segment = true;
    }

    async fn push_prelude(&self, pad: &PadSrcRef<'_>, _element: &super::TcpClientSrc) {
        let mut state = self.0.state.lock().await;
        if state.need_initial_events {
            gst_debug!(CAT, obj: pad.gst_pad(), "Pushing initial events");

            let stream_id = format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
            let stream_start_evt = gst::event::StreamStart::builder(&stream_id)
                .group_id(gst::GroupId::next())
                .build();
            pad.push_event(stream_start_evt).await;

            if let Some(ref caps) = state.caps {
                pad.push_event(gst::event::Caps::new(&caps)).await;
                *self.0.configured_caps.lock().unwrap() = Some(caps.clone());
            }

            state.need_initial_events = false;
        }

        if state.need_segment {
            let segment_evt =
                gst::event::Segment::new(&gst::FormattedSegment::<gst::format::Time>::new());
            pad.push_event(segment_evt).await;

            state.need_segment = false;
        }
    }

    async fn push_buffer(
        &self,
        pad: &PadSrcRef<'_>,
        element: &super::TcpClientSrc,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", buffer);

        self.push_prelude(pad, element).await;

        if buffer.size() == 0 {
            pad.push_event(gst::event::Eos::new()).await;
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
        _element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => tcpclientsrc.task.flush_start().is_ok(),
            EventView::FlushStop(..) => tcpclientsrc.task.flush_stop().is_ok(),
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
                q.set(false, gst::ClockTime::ZERO, gst::ClockTime::NONE);
                true
            }
            QueryView::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryView::Caps(ref mut q) => {
                let caps = if let Some(caps) = self.0.configured_caps.lock().unwrap().as_ref() {
                    q.filter()
                        .map(|f| f.intersect_with_mode(caps, gst::CapsIntersectMode::First))
                        .unwrap_or_else(|| caps.clone())
                } else {
                    q.filter()
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

struct TcpClientSrcTask {
    element: super::TcpClientSrc,
    src_pad: PadSrcWeak,
    src_pad_handler: TcpClientSrcPadHandler,
    saddr: SocketAddr,
    buffer_pool: Option<gst::BufferPool>,
    socket: Option<Socket<TcpClientReader>>,
}

impl TcpClientSrcTask {
    fn new(
        element: &super::TcpClientSrc,
        src_pad: &PadSrc,
        src_pad_handler: &TcpClientSrcPadHandler,
        saddr: SocketAddr,
        buffer_pool: gst::BufferPool,
    ) -> Self {
        TcpClientSrcTask {
            element: element.clone(),
            src_pad: src_pad.downgrade(),
            src_pad_handler: src_pad_handler.clone(),
            saddr,
            buffer_pool: Some(buffer_pool),
            socket: None,
        }
    }
}

impl TaskImpl for TcpClientSrcTask {
    fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Preparing task connecting to {:?}", self.saddr);

            let socket = tokio::net::TcpStream::connect(self.saddr)
                .await
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to connect to {:?}: {:?}", self.saddr, err]
                    )
                })?;

            self.socket = Some(
                Socket::try_new(
                    self.element.clone().upcast(),
                    self.buffer_pool.take().unwrap(),
                    TcpClientReader::new(socket),
                )
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to prepare socket {:?}", err]
                    )
                })?,
            );

            gst_log!(CAT, obj: &self.element, "Task prepared");
            Ok(())
        }
        .boxed()
    }

    fn handle_action_error(
        &mut self,
        trigger: task::Trigger,
        state: TaskState,
        err: gst::ErrorMessage,
    ) -> BoxFuture<'_, task::Trigger> {
        async move {
            match trigger {
                task::Trigger::Prepare => {
                    gst_error!(CAT, "Task preparation failed: {:?}", err);
                    self.element.post_error_message(err);

                    task::Trigger::Error
                }
                other => unreachable!("Action error for {:?} in state {:?}", other, state),
            }
        }
        .boxed()
    }

    fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            let item = self.socket.as_mut().unwrap().next().await;

            let buffer = match item {
                Some(Ok((buffer, _))) => buffer,
                Some(Err(err)) => {
                    gst_error!(CAT, obj: &self.element, "Got error {:?}", err);
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
                    return Err(gst::FlowError::Error);
                }
                None => {
                    gst_log!(CAT, obj: &self.element, "SocketStream Stopped");
                    return Err(gst::FlowError::Flushing);
                }
            };

            let pad = self.src_pad.upgrade().expect("PadSrc no longer exists");
            let res = self
                .src_pad_handler
                .push_buffer(&pad, &self.element, buffer)
                .await;
            match res {
                Ok(_) => {
                    gst_log!(CAT, obj: &self.element, "Successfully pushed buffer");
                }
                Err(gst::FlowError::Flushing) => {
                    gst_debug!(CAT, obj: &self.element, "Flushing");
                }
                Err(gst::FlowError::Eos) => {
                    gst_debug!(CAT, obj: &self.element, "EOS");
                    pad.push_event(gst::event::Eos::new()).await;
                }
                Err(err) => {
                    gst_error!(CAT, obj: &self.element, "Got error {}", err);
                    gst::element_error!(
                        self.element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["streaming stopped, reason {}", err]
                    );
                }
            }

            res.map(drop)
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Stopping task");
            self.src_pad_handler.reset_state().await;
            gst_log!(CAT, obj: &self.element, "Task stopped");
            Ok(())
        }
        .boxed()
    }

    fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Stopping task flush");
            self.src_pad_handler.set_need_segment().await;
            gst_log!(CAT, obj: &self.element, "Task flush stopped");
            Ok(())
        }
        .boxed()
    }
}

pub struct TcpClientSrc {
    src_pad: PadSrc,
    src_pad_handler: TcpClientSrcPadHandler,
    task: Task,
    settings: StdMutex<Settings>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-tcpclientsrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing TCP Client source"),
    )
});

impl TcpClientSrc {
    fn prepare(&self, element: &super::TcpClientSrc) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap().clone();

        gst_debug!(CAT, obj: element, "Preparing");

        let context =
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;

        let host: IpAddr = match settings.host {
            None => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["No host set"]
                ));
            }
            Some(ref host) => match host.parse() {
                Err(err) => {
                    return Err(gst::error_msg!(
                        gst::ResourceError::Settings,
                        ["Invalid host '{}' set: {}", host, err]
                    ));
                }
                Ok(host) => host,
            },
        };
        let port = settings.port;

        let buffer_pool = gst::BufferPool::new();
        let mut config = buffer_pool.config();
        config.set_params(None, settings.blocksize, 0, 0);
        buffer_pool.set_config(config).map_err(|_| {
            gst::error_msg!(
                gst::ResourceError::Settings,
                ["Failed to configure buffer pool"]
            )
        })?;

        let saddr = SocketAddr::new(host, port as u16);

        self.src_pad_handler.prepare(settings.caps);

        self.task
            .prepare(
                TcpClientSrcTask::new(
                    element,
                    &self.src_pad,
                    &self.src_pad_handler,
                    saddr,
                    buffer_pool,
                ),
                context,
            )
            .map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing Task: {:?}", err]
                )
            })?;

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &super::TcpClientSrc) {
        gst_debug!(CAT, obj: element, "Unpreparing");
        self.task.unprepare().unwrap();
        gst_debug!(CAT, obj: element, "Unprepared");
    }

    fn stop(&self, element: &super::TcpClientSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Stopping");
        self.task.stop()?;
        gst_debug!(CAT, obj: element, "Stopped");
        Ok(())
    }

    fn start(&self, element: &super::TcpClientSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Starting");
        self.task.start()?;
        gst_debug!(CAT, obj: element, "Started");
        Ok(())
    }

    fn pause(&self, element: &super::TcpClientSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Pausing");
        self.task.pause()?;
        gst_debug!(CAT, obj: element, "Paused");
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TcpClientSrc {
    const NAME: &'static str = "RsTsTcpClientSrc";
    type Type = super::TcpClientSrc;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let src_pad_handler = TcpClientSrcPadHandler::default();

        Self {
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap(), Some("src")),
                src_pad_handler.clone(),
            ),
            src_pad_handler,
            task: Task::default(),
            settings: StdMutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for TcpClientSrc {
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
                glib::ParamSpec::new_string(
                    "host",
                    "Host",
                    "The host IP address to receive packets from",
                    DEFAULT_HOST,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_int(
                    "port",
                    "Port",
                    "Port to receive packets from",
                    0,
                    u16::MAX as i32,
                    DEFAULT_PORT,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_boxed(
                    "caps",
                    "Caps",
                    "Caps to use",
                    gst::Caps::static_type(),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "blocksize",
                    "Blocksize",
                    "Size in bytes to read per buffer (-1 = default)",
                    0,
                    u32::MAX,
                    DEFAULT_BLOCKSIZE,
                    glib::ParamFlags::READWRITE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "host" => {
                settings.host = value.get().expect("type checked upstream");
            }
            "port" => {
                settings.port = value.get().expect("type checked upstream");
            }
            "caps" => {
                settings.caps = value.get().expect("type checked upstream");
            }
            "blocksize" => {
                settings.blocksize = value.get().expect("type checked upstream");
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
            "host" => settings.host.to_value(),
            "port" => settings.port.to_value(),
            "caps" => settings.caps.to_value(),
            "blocksize" => settings.blocksize.to_value(),
            "context" => settings.context.to_value(),
            "context-wait" => (settings.context_wait.as_millis() as u32).to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(self.src_pad.gst_pad()).unwrap();

        crate::set_element_flags(obj, gst::ElementFlags::SOURCE);
    }
}

impl ElementImpl for TcpClientSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing TCP client source",
                "Source/Network",
                "Receives data over the network via TCP",
                "Sebastian Dröge <sebastian@centricular.com>, LEE Dongjun <redongjun@gmail.com>",
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
            gst::StateChange::PlayingToPaused => {
                self.pause(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element);
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
