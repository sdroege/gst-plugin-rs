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

use either::Either;

use futures::channel::mpsc;
use futures::future::BoxFuture;
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
use gst::{gst_debug, gst_element_error, gst_error, gst_error_msg, gst_log, gst_trace};
use gst::{EventView, QueryView};

use lazy_static::lazy_static;

use rand;

use std::convert::TryInto;
use std::sync::Arc;
use std::u32;

use crate::runtime::prelude::*;
use crate::runtime::{self, Context, PadSrc, PadSrcRef};

const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_MAX_BUFFERS: u32 = 10;
const DEFAULT_DO_TIMESTAMP: bool = false;

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_wait: u32,
    caps: Option<gst::Caps>,
    max_buffers: u32,
    do_timestamp: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            caps: DEFAULT_CAPS,
            max_buffers: DEFAULT_MAX_BUFFERS,
            do_timestamp: DEFAULT_DO_TIMESTAMP,
        }
    }
}

static PROPERTIES: [subclass::Property; 5] = [
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
    subclass::Property("max-buffers", |name| {
        glib::ParamSpec::uint(
            name,
            "Max Buffers",
            "Maximum number of buffers to queue up",
            1,
            u32::MAX,
            DEFAULT_MAX_BUFFERS,
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
    subclass::Property("do-timestamp", |name| {
        glib::ParamSpec::boolean(
            name,
            "Do Timestamp",
            "Timestamp buffers with the current running time on arrival",
            DEFAULT_DO_TIMESTAMP,
            glib::ParamFlags::READWRITE,
        )
    }),
];

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-appsrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing app source"),
    );
}

#[derive(Debug)]
enum StreamItem {
    Buffer(gst::Buffer),
    Event(gst::Event),
}

#[derive(Debug)]
struct AppSrcPadHandlerInner {
    need_initial_events: bool,
    configured_caps: Option<gst::Caps>,
}

impl Default for AppSrcPadHandlerInner {
    fn default() -> Self {
        AppSrcPadHandlerInner {
            need_initial_events: true,
            configured_caps: None,
        }
    }
}

#[derive(Clone, Debug)]
struct AppSrcPadHandler(Arc<Mutex<AppSrcPadHandlerInner>>);

impl AppSrcPadHandler {
    fn new() -> Self {
        AppSrcPadHandler(Arc::new(Mutex::new(AppSrcPadHandlerInner::default())))
    }

    #[inline]
    async fn lock(&self) -> MutexGuard<'_, AppSrcPadHandlerInner> {
        self.0.lock().await
    }

    async fn start_task(
        &self,
        pad: PadSrcRef<'_>,
        element: &gst::Element,
        receiver: mpsc::Receiver<StreamItem>,
    ) {
        let this = self.clone();
        let pad_weak = pad.downgrade();
        let element = element.clone();
        let receiver = Arc::new(Mutex::new(receiver));
        pad.start_task(move || {
            let this = this.clone();
            let pad_weak = pad_weak.clone();
            let element = element.clone();
            let receiver = Arc::clone(&receiver);
            async move {
                let item = receiver.lock().await.next().await;

                let pad = pad_weak.upgrade().expect("PadSrc no longer exists");
                let item = match item {
                    Some(item) => item,
                    None => {
                        gst_log!(CAT, obj: pad.gst_pad(), "SrcPad channel aborted");
                        pad.pause_task().await;
                        return;
                    }
                };

                this.push_item(pad, &element, item).await;
            }
        })
        .await;
    }

    async fn push_item(self, pad: PadSrcRef<'_>, element: &gst::Element, item: StreamItem) {
        // Don't keep the `events` in scope so as to reduce the `Future`'s size
        {
            let mut events = Vec::new();

            {
                let mut inner = self.lock().await;
                if inner.need_initial_events {
                    gst_debug!(CAT, obj: pad.gst_pad(), "Pushing initial events");

                    let stream_id =
                        format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
                    events.push(
                        gst::Event::new_stream_start(&stream_id)
                            .group_id(gst::util_group_id_next())
                            .build(),
                    );
                    let appsrc = AppSrc::from_instance(element);
                    if let Some(ref caps) = appsrc.settings.lock().await.caps {
                        events.push(gst::Event::new_caps(&caps).build());
                        inner.configured_caps = Some(caps.clone());
                    }
                    events.push(
                        gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new())
                            .build(),
                    );

                    inner.need_initial_events = false;
                }
            }

            for event in events {
                pad.push_event(event).await;
            }
        }

        let res = match item {
            StreamItem::Buffer(buffer) => {
                gst_log!(CAT, obj: pad.gst_pad(), "Forwarding buffer {:?}", buffer);
                pad.push(buffer).await
            }
            StreamItem::Event(event) => {
                gst_log!(CAT, obj: pad.gst_pad(), "Forwarding event {:?}", event);
                pad.push_event(event).await;
                Ok(gst::FlowSuccess::Ok)
            }
        };

        match res {
            Ok(_) => gst_log!(CAT, obj: pad.gst_pad(), "Successfully pushed item"),
            Err(gst::FlowError::Eos) => gst_debug!(CAT, obj: pad.gst_pad(), "EOS"),
            Err(gst::FlowError::Flushing) => gst_debug!(CAT, obj: pad.gst_pad(), "Flushing"),
            Err(err) => {
                gst_error!(CAT, obj: pad.gst_pad(), "Got error {}", err);
                gst_element_error!(
                    &element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["streaming stopped, reason {}", err]
                );
            }
        }
    }
}

impl PadSrcHandler for AppSrcPadHandler {
    type ElementImpl = AppSrc;

    fn src_event(
        &self,
        pad: PadSrcRef,
        app_src: &AppSrc,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        gst_log!(CAT, obj: pad.gst_pad(), "Handling event {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                let _ = runtime::executor::block_on(app_src.pause(element));
                true
            }
            EventView::FlushStop(..) => {
                let (res, state, pending) = element.get_state(0.into());
                if res == Ok(gst::StateChangeSuccess::Success) && state == gst::State::Playing
                    || res == Ok(gst::StateChangeSuccess::Async) && pending == gst::State::Playing
                {
                    let _ = runtime::executor::block_on(app_src.start(element));
                }
                true
            }
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst_log!(CAT, obj: pad.gst_pad(), "Handled event {:?}", event);
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Didn't handle event {:?}", event);
        }

        Either::Left(ret)
    }

    fn src_query(
        &self,
        pad: PadSrcRef,
        _app_src: &AppSrc,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(CAT, obj: pad.gst_pad(), "Handling query {:?}", query);
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
                let inner = runtime::executor::block_on(self.lock());
                let caps = if let Some(ref caps) = inner.configured_caps {
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
            gst_log!(CAT, obj: pad.gst_pad(), "Handled query {:?}", query);
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Didn't handle query {:?}", query);
        }
        ret
    }
}

struct State {
    sender: Option<mpsc::Sender<StreamItem>>,
}

impl Default for State {
    fn default() -> Self {
        State { sender: None }
    }
}

struct AppSrc {
    src_pad: PadSrc,
    src_pad_handler: AppSrcPadHandler,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl AppSrc {
    async fn push_buffer(&self, element: &gst::Element, mut buffer: gst::Buffer) -> bool {
        if self.settings.lock().await.do_timestamp {
            if let Some(clock) = element.get_clock() {
                let base_time = element.get_base_time();
                let now = clock.get_time();

                let buffer = buffer.make_mut();
                buffer.set_dts(now - base_time);
                buffer.set_pts(gst::CLOCK_TIME_NONE);
            } else {
                gst_error!(CAT, obj: element, "Don't have a clock yet");
                return false;
            }
        }

        let mut state = self.state.lock().await;
        let sender = match state.sender.as_mut() {
            Some(sender) => sender,
            None => return false,
        };
        match sender.send(StreamItem::Buffer(buffer)).await {
            Ok(_) => true,
            Err(err) => {
                gst_error!(CAT, obj: element, "Failed to queue buffer: {}", err);
                false
            }
        }
    }

    async fn end_of_stream(&self, element: &gst::Element) -> bool {
        let mut state = self.state.lock().await;
        let sender = match state.sender.as_mut() {
            Some(sender) => sender,
            None => return false,
        };
        let eos = StreamItem::Event(gst::Event::new_eos().build());
        match sender.send(eos).await {
            Ok(_) => true,
            Err(err) => {
                gst_error!(CAT, obj: element, "Failed to queue EOS: {}", err);
                false
            }
        }
    }

    async fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let _state = self.state.lock().await;
        gst_debug!(CAT, obj: element, "Preparing");

        let context = {
            let settings = self.settings.lock().await;
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?
        };

        self.src_pad
            .prepare(context, &self.src_pad_handler)
            .await
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing src_pads: {:?}", err]
                )
            })?;

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    async fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        let _state = self.state.lock().await;
        gst_debug!(CAT, obj: element, "Unpreparing");

        self.src_pad.stop_task().await;
        let _ = self.src_pad.unprepare().await;
        self.src_pad_handler.lock().await.configured_caps = None;

        gst_debug!(CAT, obj: element, "Unprepared");

        Ok(())
    }

    async fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().await;
        gst_debug!(CAT, obj: element, "Starting");

        let max_buffers = self.settings.lock().await.max_buffers.try_into().unwrap();
        let (sender, receiver) = mpsc::channel(max_buffers);
        state.sender = Some(sender);

        self.src_pad_handler
            .start_task(self.src_pad.as_ref(), element, receiver)
            .await;

        gst_debug!(CAT, obj: element, "Started");

        Ok(())
    }

    async fn pause(&self, element: &gst::Element) -> Result<(), ()> {
        let pause_completion = {
            let mut state = self.state.lock().await;
            gst_debug!(CAT, obj: element, "Pausing");

            let pause_completion = self.src_pad.pause_task().await;
            // Prevent subsequent items from being enqueued
            state.sender = None;

            pause_completion
        };

        gst_debug!(CAT, obj: element, "Waiting for Task Pause to complete");
        pause_completion.await;

        gst_debug!(CAT, obj: element, "Paused");

        Ok(())
    }
}

impl ObjectSubclass for AppSrc {
    const NAME: &'static str = "RsTsAppSrc";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing app source",
            "Source/Generic",
            "Thread-sharing app source",
            "Sebastian Dröge <sebastian@centricular.com>",
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

        klass.add_signal_with_class_handler(
            "push-buffer",
            glib::SignalFlags::RUN_LAST | glib::SignalFlags::ACTION,
            &[gst::Buffer::static_type()],
            bool::static_type(),
            |_, args| {
                let element = args[0]
                    .get::<gst::Element>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let buffer = args[1]
                    .get::<gst::Buffer>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let appsrc = Self::from_instance(&element);

                Some(runtime::executor::block_on(appsrc.push_buffer(&element, buffer)).to_value())
            },
        );

        klass.add_signal_with_class_handler(
            "end-of-stream",
            glib::SignalFlags::RUN_LAST | glib::SignalFlags::ACTION,
            &[],
            bool::static_type(),
            |_, args| {
                let element = args[0]
                    .get::<gst::Element>()
                    .expect("signal arg")
                    .expect("missing signal arg");
                let appsrc = Self::from_instance(&element);
                Some(runtime::executor::block_on(appsrc.end_of_stream(&element)).to_value())
            },
        );
    }

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("src").unwrap();
        let src_pad = PadSrc::new_from_template(&templ, Some("src"));

        Self {
            src_pad,
            src_pad_handler: AppSrcPadHandler::new(),
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for AppSrc {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        let mut settings = runtime::executor::block_on(self.settings.lock());
        match *prop {
            subclass::Property("context", ..) => {
                settings.context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                settings.context_wait = value.get_some().expect("type checked upstream");
            }
            subclass::Property("caps", ..) => {
                settings.caps = value.get().expect("type checked upstream");
            }
            subclass::Property("max-buffers", ..) => {
                settings.max_buffers = value.get_some().expect("type checked upstream");
            }
            subclass::Property("do-timestamp", ..) => {
                settings.do_timestamp = value.get_some().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        let settings = runtime::executor::block_on(self.settings.lock());
        match *prop {
            subclass::Property("context", ..) => Ok(settings.context.to_value()),
            subclass::Property("context-wait", ..) => Ok(settings.context_wait.to_value()),
            subclass::Property("caps", ..) => Ok(settings.caps.to_value()),
            subclass::Property("max-buffers", ..) => Ok(settings.max_buffers.to_value()),
            subclass::Property("do-timestamp", ..) => Ok(settings.do_timestamp.to_value()),
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

impl ElementImpl for AppSrc {
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
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => {
                runtime::executor::block_on(self.start(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PausedToReady => {
                runtime::executor::block_on(async {
                    self.src_pad_handler.lock().await.need_initial_events = true;
                });
            }
            _ => (),
        }

        Ok(success)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-appsrc",
        gst::Rank::None,
        AppSrc::get_type(),
    )
}
