// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2019-2020 François Laignel <fengalin@free.fr>
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

use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::lock::Mutex as FutMutex;
use futures::prelude::*;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_log, gst_trace};

use once_cell::sync::Lazy;

use std::convert::TryInto;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use std::u32;

use crate::runtime::prelude::*;
use crate::runtime::{Context, PadSrc, PadSrcRef, PadSrcWeak, Task, TaskState};

const DEFAULT_CONTEXT: &str = "";
// FIXME use Duration::ZERO when MSVC >= 1.53.2
const DEFAULT_CONTEXT_WAIT: Duration = Duration::from_nanos(0);
const DEFAULT_CAPS: Option<gst::Caps> = None;
const DEFAULT_MAX_BUFFERS: u32 = 10;
const DEFAULT_DO_TIMESTAMP: bool = false;

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_wait: Duration,
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

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-appsrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing app source"),
    )
});

#[derive(Debug)]
enum StreamItem {
    Buffer(gst::Buffer),
    Event(gst::Event),
}

#[derive(Debug)]
struct AppSrcPadHandlerState {
    need_initial_events: bool,
    need_segment: bool,
    caps: Option<gst::Caps>,
}

impl Default for AppSrcPadHandlerState {
    fn default() -> Self {
        AppSrcPadHandlerState {
            need_initial_events: true,
            need_segment: true,
            caps: None,
        }
    }
}

#[derive(Debug, Default)]
struct AppSrcPadHandlerInner {
    state: FutMutex<AppSrcPadHandlerState>,
    configured_caps: StdMutex<Option<gst::Caps>>,
}

#[derive(Clone, Debug, Default)]
struct AppSrcPadHandler(Arc<AppSrcPadHandlerInner>);

impl AppSrcPadHandler {
    fn prepare(&self, caps: Option<gst::Caps>) {
        self.0
            .state
            .try_lock()
            .expect("State locked elsewhere")
            .caps = caps;
    }

    async fn reset_state(&self) {
        *self.0.state.lock().await = Default::default();
    }

    async fn set_need_segment(&self) {
        self.0.state.lock().await.need_segment = true;
    }

    async fn push_prelude(&self, pad: &PadSrcRef<'_>, _element: &super::AppSrc) {
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

    async fn push_item(
        &self,
        pad: &PadSrcRef<'_>,
        element: &super::AppSrc,
        item: StreamItem,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", item);

        self.push_prelude(pad, element).await;

        match item {
            StreamItem::Buffer(buffer) => {
                gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", buffer);
                pad.push(buffer).await
            }
            StreamItem::Event(event) => {
                match event.view() {
                    gst::EventView::Eos(_) => {
                        // Let the caller push the event
                        Err(gst::FlowError::Eos)
                    }
                    _ => {
                        gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", event);
                        pad.push_event(event).await;
                        Ok(gst::FlowSuccess::Ok)
                    }
                }
            }
        }
    }
}

impl PadSrcHandler for AppSrcPadHandler {
    type ElementImpl = AppSrc;

    fn src_event(
        &self,
        pad: &PadSrcRef,
        appsrc: &AppSrc,
        _element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => appsrc.task.flush_start().is_ok(),
            EventView::FlushStop(..) => appsrc.task.flush_stop().is_ok(),
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
        _appsrc: &AppSrc,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", query);
        let ret = match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                q.set(true, gst::ClockTime::ZERO, gst::ClockTime::NONE);
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

#[derive(Debug)]
struct AppSrcTask {
    element: super::AppSrc,
    src_pad: PadSrcWeak,
    src_pad_handler: AppSrcPadHandler,
    receiver: mpsc::Receiver<StreamItem>,
}

impl AppSrcTask {
    fn new(
        element: &super::AppSrc,
        src_pad: &PadSrc,
        src_pad_handler: &AppSrcPadHandler,
        receiver: mpsc::Receiver<StreamItem>,
    ) -> Self {
        AppSrcTask {
            element: element.clone(),
            src_pad: src_pad.downgrade(),
            src_pad_handler: src_pad_handler.clone(),
            receiver,
        }
    }
}

impl AppSrcTask {
    fn flush(&mut self) {
        // Purge the channel
        while let Ok(Some(_item)) = self.receiver.try_next() {}
    }
}

impl TaskImpl for AppSrcTask {
    fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            let item = match self.receiver.next().await {
                Some(item) => item,
                None => {
                    gst_error!(CAT, obj: &self.element, "SrcPad channel aborted");
                    gst::element_error!(
                        &self.element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["streaming stopped, reason: channel aborted"]
                    );
                    return Err(gst::FlowError::Flushing);
                }
            };

            let pad = self.src_pad.upgrade().expect("PadSrc no longer exists");
            let res = self
                .src_pad_handler
                .push_item(&pad, &self.element, item)
                .await;
            match res {
                Ok(_) => {
                    gst_log!(CAT, obj: &self.element, "Successfully pushed item");
                }
                Err(gst::FlowError::Eos) => {
                    gst_debug!(CAT, obj: &self.element, "EOS");
                    pad.push_event(gst::event::Eos::new()).await;
                }
                Err(gst::FlowError::Flushing) => {
                    gst_debug!(CAT, obj: &self.element, "Flushing");
                }
                Err(err) => {
                    gst_error!(CAT, obj: &self.element, "Got error {}", err);
                    gst::element_error!(
                        &self.element,
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

            self.flush();
            self.src_pad_handler.reset_state().await;

            gst_log!(CAT, obj: &self.element, "Task stopped");
            Ok(())
        }
        .boxed()
    }

    fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Starting task flush");

            self.flush();
            self.src_pad_handler.set_need_segment().await;

            gst_log!(CAT, obj: &self.element, "Task flush started");
            Ok(())
        }
        .boxed()
    }
}

#[derive(Debug)]
pub struct AppSrc {
    src_pad: PadSrc,
    src_pad_handler: AppSrcPadHandler,
    task: Task,
    sender: StdMutex<Option<mpsc::Sender<StreamItem>>>,
    settings: StdMutex<Settings>,
}

impl AppSrc {
    fn push_buffer(&self, element: &super::AppSrc, mut buffer: gst::Buffer) -> bool {
        let state = self.task.lock_state();
        if *state != TaskState::Started && *state != TaskState::Paused {
            gst_debug!(CAT, obj: element, "Rejecting buffer due to element state");
            return false;
        }

        let do_timestamp = self.settings.lock().unwrap().do_timestamp;
        if do_timestamp {
            if let Some(clock) = element.clock() {
                let base_time = element.base_time();
                let now = clock.time();

                let buffer = buffer.make_mut();
                buffer.set_dts(
                    now.zip(base_time)
                        .and_then(|(now, base_time)| now.checked_sub(base_time)),
                );
                buffer.set_pts(None);
            } else {
                gst_error!(CAT, obj: element, "Don't have a clock yet");
                return false;
            }
        }

        match self
            .sender
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .try_send(StreamItem::Buffer(buffer))
        {
            Ok(_) => true,
            Err(err) => {
                gst_error!(CAT, obj: element, "Failed to queue buffer: {}", err);
                false
            }
        }
    }

    fn end_of_stream(&self, element: &super::AppSrc) -> bool {
        let mut sender = self.sender.lock().unwrap();
        let sender = match sender.as_mut() {
            Some(sender) => sender,
            None => return false,
        };

        match sender.try_send(StreamItem::Event(gst::event::Eos::new())) {
            Ok(_) => true,
            Err(err) => {
                gst_error!(CAT, obj: element, "Failed to queue EOS: {}", err);
                false
            }
        }
    }

    fn prepare(&self, element: &super::AppSrc) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        gst_debug!(CAT, obj: element, "Preparing");

        let context =
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;

        let max_buffers = settings.max_buffers.try_into().map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::Settings,
                ["Invalid max-buffers: {}, {}", settings.max_buffers, err]
            )
        })?;

        let (sender, receiver) = mpsc::channel(max_buffers);
        *self.sender.lock().unwrap() = Some(sender);

        self.src_pad_handler.prepare(settings.caps.clone());

        self.task
            .prepare(
                AppSrcTask::new(element, &self.src_pad, &self.src_pad_handler, receiver),
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

    fn unprepare(&self, element: &super::AppSrc) {
        gst_debug!(CAT, obj: element, "Unpreparing");

        *self.sender.lock().unwrap() = None;
        self.task.unprepare().unwrap();

        gst_debug!(CAT, obj: element, "Unprepared");
    }

    fn stop(&self, element: &super::AppSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Stopping");
        self.task.stop()?;
        gst_debug!(CAT, obj: element, "Stopped");
        Ok(())
    }

    fn start(&self, element: &super::AppSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Starting");
        self.task.start()?;
        gst_debug!(CAT, obj: element, "Started");
        Ok(())
    }

    fn pause(&self, element: &super::AppSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Pausing");
        self.task.pause()?;
        gst_debug!(CAT, obj: element, "Paused");
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for AppSrc {
    const NAME: &'static str = "RsTsAppSrc";
    type Type = super::AppSrc;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let src_pad_handler = AppSrcPadHandler::default();

        Self {
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap(), Some("src")),
                src_pad_handler.clone(),
            ),
            src_pad_handler,
            task: Task::default(),
            sender: StdMutex::new(None),
            settings: StdMutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for AppSrc {
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
                glib::ParamSpec::new_uint(
                    "max-buffers",
                    "Max Buffers",
                    "Maximum number of buffers to queue up",
                    1,
                    u32::MAX,
                    DEFAULT_MAX_BUFFERS,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_boxed(
                    "caps",
                    "Caps",
                    "Caps to use",
                    gst::Caps::static_type(),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_boolean(
                    "do-timestamp",
                    "Do Timestamp",
                    "Timestamp buffers with the current running time on arrival",
                    DEFAULT_DO_TIMESTAMP,
                    glib::ParamFlags::READWRITE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                glib::subclass::Signal::builder(
                    "push-buffer",
                    &[gst::Buffer::static_type().into()],
                    bool::static_type().into(),
                )
                .action()
                .class_handler(|_, args| {
                    let element = args[0].get::<super::AppSrc>().expect("signal arg");
                    let buffer = args[1].get::<gst::Buffer>().expect("signal arg");
                    let appsrc = AppSrc::from_instance(&element);

                    Some(appsrc.push_buffer(&element, buffer).to_value())
                })
                .build(),
                glib::subclass::Signal::builder("end-of-stream", &[], bool::static_type().into())
                    .action()
                    .class_handler(|_, args| {
                        let element = args[0].get::<super::AppSrc>().expect("signal arg");
                        let appsrc = AppSrc::from_instance(&element);

                        Some(appsrc.end_of_stream(&element).to_value())
                    })
                    .build(),
            ]
        });

        SIGNALS.as_ref()
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
            "caps" => {
                settings.caps = value.get().expect("type checked upstream");
            }
            "max-buffers" => {
                settings.max_buffers = value.get().expect("type checked upstream");
            }
            "do-timestamp" => {
                settings.do_timestamp = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "context" => settings.context.to_value(),
            "context-wait" => (settings.context_wait.as_millis() as u32).to_value(),
            "caps" => settings.caps.to_value(),
            "max-buffers" => settings.max_buffers.to_value(),
            "do-timestamp" => settings.do_timestamp.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(self.src_pad.gst_pad()).unwrap();

        crate::set_element_flags(obj, gst::ElementFlags::SOURCE);
    }
}

impl ElementImpl for AppSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing app source",
                "Source/Generic",
                "Thread-sharing app source",
                "Sebastian Dröge <sebastian@centricular.com>",
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
