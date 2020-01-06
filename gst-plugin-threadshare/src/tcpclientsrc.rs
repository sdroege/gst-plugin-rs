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

use either::Either;
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
use gst::{gst_debug, gst_element_error, gst_error, gst_error_msg, gst_log, gst_trace};

use lazy_static::lazy_static;

use rand;

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{self, Arc};
use std::u16;

use tokio::io::AsyncReadExt;

use crate::runtime::prelude::*;
use crate::runtime::{self, Context, JoinHandle, PadSrc, PadSrcRef};

use super::socket::{Socket, SocketRead, SocketStream};

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

pub struct TcpClientReader(Arc<Mutex<TcpClientReaderInner>>);

impl TcpClientReader {
    pub fn new(socket: tokio::net::TcpStream) -> Self {
        TcpClientReader(Arc::new(Mutex::new(TcpClientReaderInner { socket })))
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
    caps: Option<gst::Caps>,
    configured_caps: Option<gst::Caps>,
}

impl Default for TcpClientSrcPadHandlerState {
    fn default() -> Self {
        TcpClientSrcPadHandlerState {
            need_initial_events: true,
            caps: None,
            configured_caps: None,
        }
    }
}

#[derive(Debug, Default)]
struct TcpClientSrcPadHandlerInner {
    state: sync::RwLock<TcpClientSrcPadHandlerState>,
    socket_stream: Mutex<Option<SocketStream<TcpClientReader>>>,
    flush_join_handle: sync::Mutex<Option<JoinHandle<Result<(), ()>>>>,
}

#[derive(Clone, Debug, Default)]
struct TcpClientSrcPadHandler(Arc<TcpClientSrcPadHandlerInner>);

impl TcpClientSrcPadHandler {
    async fn start_task(&self, pad: PadSrcRef<'_>, element: &gst::Element) {
        let this = self.clone();
        let pad_weak = pad.downgrade();
        let element = element.clone();
        pad.start_task(move || {
            let this = this.clone();
            let pad_weak = pad_weak.clone();
            let element = element.clone();
            async move {
                let item = this
                    .0
                    .socket_stream
                    .lock()
                    .await
                    .as_mut()
                    .expect("Missing SocketStream")
                    .next()
                    .await;

                let pad = pad_weak.upgrade().expect("PadSrc no longer exists");
                let buffer = match item {
                    Some(Ok((buffer, _))) => buffer,
                    Some(Err(err)) => {
                        gst_error!(CAT, obj: &element, "Got error {}", err);
                        match err {
                            Either::Left(gst::FlowError::CustomError) => (),
                            Either::Left(err) => {
                                gst_element_error!(
                                    element,
                                    gst::StreamError::Failed,
                                    ("Internal data stream error"),
                                    ["streaming stopped, reason {}", err]
                                );
                            }
                            Either::Right(err) => {
                                gst_element_error!(
                                    element,
                                    gst::StreamError::Failed,
                                    ("I/O error"),
                                    ["streaming stopped, I/O error {}", err]
                                );
                            }
                        }
                        return;
                    }
                    None => {
                        gst_log!(CAT, obj: pad.gst_pad(), "SocketStream Stopped");
                        pad.pause_task().await;
                        return;
                    }
                };

                this.push_buffer(pad, &element, buffer).await;
            }
        })
        .await;
    }

    async fn push_buffer(&self, pad: PadSrcRef<'_>, element: &gst::Element, buffer: gst::Buffer) {
        {
            let mut events = Vec::new();
            {
                // Only `read` the state in the hot path
                if self.0.state.read().unwrap().need_initial_events {
                    // We will need to `write` and we also want to prevent
                    // any changes on the state while we are handling initial events
                    let mut state = self.0.state.write().unwrap();
                    assert!(state.need_initial_events);

                    gst_debug!(CAT, obj: pad.gst_pad(), "Pushing initial events");

                    let stream_id =
                        format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
                    events.push(
                        gst::Event::new_stream_start(&stream_id)
                            .group_id(gst::util_group_id_next())
                            .build(),
                    );

                    if let Some(ref caps) = state.caps {
                        events.push(gst::Event::new_caps(&caps).build());
                        state.configured_caps = Some(caps.clone());
                    }
                    events.push(
                        gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new())
                            .build(),
                    );

                    state.need_initial_events = false;
                }

                if buffer.get_size() == 0 {
                    events.push(gst::Event::new_eos().build());
                }
            }

            for event in events {
                pad.push_event(event).await;
            }
        }

        match pad.push(buffer).await {
            Ok(_) => {
                gst_log!(CAT, obj: pad.gst_pad(), "Successfully pushed buffer");
            }
            Err(gst::FlowError::Flushing) => {
                gst_debug!(CAT, obj: pad.gst_pad(), "Flushing");
                pad.pause_task().await;
            }
            Err(gst::FlowError::Eos) => {
                gst_debug!(CAT, obj: pad.gst_pad(), "EOS");
                pad.pause_task().await;
            }
            Err(err) => {
                gst_error!(CAT, obj: pad.gst_pad(), "Got error {}", err);
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["streaming stopped, reason {}", err]
                );
            }
        }
    }
}

impl PadSrcHandler for TcpClientSrcPadHandler {
    type ElementImpl = TcpClientSrc;

    fn src_event(
        &self,
        pad: &PadSrcRef,
        _tcpclientsrc: &TcpClientSrc,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                let mut flush_join_handle = self.0.flush_join_handle.lock().unwrap();
                if flush_join_handle.is_none() {
                    let element = element.clone();
                    let pad_weak = pad.downgrade();

                    *flush_join_handle = Some(pad.spawn(async move {
                        let res = TcpClientSrc::from_instance(&element).pause(&element).await;
                        let pad = pad_weak.upgrade().unwrap();
                        if res.is_ok() {
                            gst_debug!(CAT, obj: pad.gst_pad(), "FlushStart complete");
                        } else {
                            gst_debug!(CAT, obj: pad.gst_pad(), "FlushStart failed");
                        }

                        res
                    }));
                } else {
                    gst_debug!(CAT, obj: pad.gst_pad(), "FlushStart ignored: previous Flush in progress");
                }

                true
            }
            EventView::FlushStop(..) => {
                let element = element.clone();
                let inner_weak = Arc::downgrade(&self.0);
                let pad_weak = pad.downgrade();

                let fut = async move {
                    let mut ret = false;

                    let pad = pad_weak.upgrade().unwrap();
                    let inner_weak = inner_weak.upgrade().unwrap();
                    let flush_join_handle = inner_weak.flush_join_handle.lock().unwrap().take();
                    if let Some(flush_join_handle) = flush_join_handle {
                        if let Ok(Ok(())) = flush_join_handle.await {
                            ret = TcpClientSrc::from_instance(&element)
                                .start(&element)
                                .await
                                .is_ok();
                            gst_debug!(CAT, obj: pad.gst_pad(), "FlushStop complete");
                        } else {
                            gst_debug!(CAT, obj: pad.gst_pad(), "FlushStop aborted: FlushStart failed");
                        }
                    } else {
                        gst_debug!(CAT, obj: pad.gst_pad(), "FlushStop ignored: no Flush in progress");
                    }

                    ret
                }
                .boxed();

                return Either::Right(fut);
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

        Either::Left(ret)
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
                q.set(false, 0.into(), 0.into());
                true
            }
            QueryView::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryView::Caps(ref mut q) => {
                let state = self.0.state.read().unwrap();
                let caps = if let Some(ref caps) = state.configured_caps {
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

struct State {
    socket: Option<Socket<TcpClientReader>>,
}

impl Default for State {
    fn default() -> State {
        State { socket: None }
    }
}

#[derive(Debug)]
struct PreparationSet {
    join_handle: JoinHandle<Result<(), gst::ErrorMessage>>,
    context: Context,
}

struct TcpClientSrc {
    src_pad: PadSrc,
    src_pad_handler: TcpClientSrcPadHandler,
    state: Mutex<State>,
    settings: Mutex<Settings>,
    preparation_set: Mutex<Option<PreparationSet>>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-tcpclientsrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing TCP Client source"),
    );
}

impl TcpClientSrc {
    async fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let _state = self.state.lock().await;
        gst_debug!(CAT, obj: element, "Preparing");

        let context = {
            let settings = self.settings.lock().await;

            self.src_pad_handler.0.state.write().unwrap().caps = settings.caps.clone();

            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?
        };

        // TcpStream needs to be instantiated in the thread of its I/O reactor
        *self.preparation_set.lock().await = Some(PreparationSet {
            join_handle: context.spawn(Self::prepare_socket(element.clone())),
            context,
        });

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    async fn prepare_socket(element: gst::Element) -> Result<(), gst::ErrorMessage> {
        let this = Self::from_instance(&element);

        let settings = this.settings.lock().await.clone();
        gst_debug!(CAT, obj: &element, "Preparing Socket");

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
        gst_debug!(CAT, obj: &element, "Connecting to {:?}", saddr);
        let socket = tokio::net::TcpStream::connect(saddr).await.map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["Failed to connect to {:?} {:?}", saddr, err]
            )
        })?;

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

        let socket_stream = socket.prepare().await.map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to prepare socket {:?}", err]
            )
        })?;

        *this.src_pad_handler.0.socket_stream.lock().await = Some(socket_stream);

        this.state.lock().await.socket = Some(socket);

        gst_debug!(CAT, obj: &element, "Socket Prepared");

        Ok(())
    }

    async fn complete_preparation(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let preparation_set = self.preparation_set.lock().await.take();
        if preparation_set.is_none() {
            gst_log!(CAT, obj: element, "Preparation already completed");
            return Ok(());
        }

        gst_debug!(CAT, obj: element, "Completing preparation");

        let PreparationSet {
            join_handle,
            context,
        } = preparation_set.unwrap();

        join_handle
            .await
            .expect("The socket preparation has panicked")?;

        self.src_pad
            .prepare(context, &self.src_pad_handler)
            .await
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing src_pads: {:?}", err]
                )
            })?;

        gst_debug!(CAT, obj: element, "Preparation completed");

        Ok(())
    }

    async fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().await;
        gst_debug!(CAT, obj: element, "Unpreparing");

        self.src_pad.stop_task().await;

        {
            let socket = state.socket.take().unwrap();
            socket.unprepare().await.unwrap();
        }

        let _ = self.src_pad.unprepare().await;
        self.src_pad_handler
            .0
            .state
            .write()
            .unwrap()
            .configured_caps = None;

        gst_debug!(CAT, obj: element, "Unprepared");
        Ok(())
    }

    async fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let state = self.state.lock().await;
        gst_debug!(CAT, obj: element, "Starting");

        if let Some(ref socket) = state.socket {
            socket
                .start(element.get_clock(), Some(element.get_base_time()))
                .await;
        }

        self.src_pad_handler
            .start_task(self.src_pad.as_ref(), element)
            .await;

        gst_debug!(CAT, obj: element, "Started");

        Ok(())
    }

    async fn pause(&self, element: &gst::Element) -> Result<(), ()> {
        let pause_completion = {
            let state = self.state.lock().await;
            gst_debug!(CAT, obj: element, "Pausing");

            let pause_completion = self.src_pad.pause_task().await;
            state.socket.as_ref().unwrap().pause().await;

            pause_completion
        };

        gst_debug!(CAT, obj: element, "Waiting for Task Pause to complete");
        pause_completion.await;

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
            src_pad_handler: TcpClientSrcPadHandler::default(),
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
            preparation_set: Mutex::new(None),
        }
    }
}

impl ObjectImpl for TcpClientSrc {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        let mut settings = runtime::executor::block_on(self.settings.lock());
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

        let settings = runtime::executor::block_on(self.settings.lock());
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
                runtime::executor::block_on(self.prepare(element)).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::ReadyToPaused => {
                runtime::executor::block_on(self.complete_preparation(element)).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                runtime::executor::block_on(self.pause(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                runtime::executor::block_on(self.unprepare(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::Success;
            }
            gst::StateChange::PausedToPlaying => {
                runtime::executor::block_on(self.start(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PausedToReady => {
                self.src_pad_handler
                    .0
                    .state
                    .write()
                    .unwrap()
                    .need_initial_events = true;
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
