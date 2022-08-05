// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2019-2022 François Laignel <fengalin@free.fr>
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
//
// SPDX-License-Identifier: LGPL-2.1-or-later

use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::prelude::*;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use once_cell::sync::Lazy;

use std::sync::Mutex;
use std::time::Duration;
use std::u32;

use crate::runtime::prelude::*;
use crate::runtime::{Context, PadSrc, PadSrcRef, Task, TaskState};

const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: Duration = Duration::ZERO;
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

#[derive(Clone, Debug)]
struct AppSrcPadHandler;

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

        gst::log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => appsrc.task.flush_start().await_maybe_on_context().is_ok(),
            EventView::FlushStop(..) => appsrc.task.flush_stop().await_maybe_on_context().is_ok(),
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst::log!(CAT, obj: pad.gst_pad(), "Handled {:?}", event);
        } else {
            gst::log!(CAT, obj: pad.gst_pad(), "Didn't handle {:?}", event);
        }

        ret
    }

    fn src_query(
        &self,
        pad: &PadSrcRef,
        appsrc: &AppSrc,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryViewMut;

        gst::log!(CAT, obj: pad.gst_pad(), "Handling {:?}", query);
        let ret = match query.view_mut() {
            QueryViewMut::Latency(q) => {
                q.set(true, gst::ClockTime::ZERO, gst::ClockTime::NONE);
                true
            }
            QueryViewMut::Scheduling(q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryViewMut::Caps(q) => {
                let caps = if let Some(caps) = appsrc.configured_caps.lock().unwrap().as_ref() {
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
            gst::log!(CAT, obj: pad.gst_pad(), "Handled {:?}", query);
        } else {
            gst::log!(CAT, obj: pad.gst_pad(), "Didn't handle {:?}", query);
        }
        ret
    }
}

#[derive(Debug)]
struct AppSrcTask {
    element: super::AppSrc,
    receiver: mpsc::Receiver<StreamItem>,
    need_initial_events: bool,
    need_segment: bool,
}

impl AppSrcTask {
    fn new(element: super::AppSrc, receiver: mpsc::Receiver<StreamItem>) -> Self {
        AppSrcTask {
            element,
            receiver,
            need_initial_events: true,
            need_segment: true,
        }
    }
}

impl AppSrcTask {
    fn flush(&mut self) {
        // Purge the channel
        while let Ok(Some(_item)) = self.receiver.try_next() {}
    }

    async fn push_item(&mut self, item: StreamItem) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj: &self.element, "Handling {:?}", item);
        let appsrc = self.element.imp();

        if self.need_initial_events {
            gst::debug!(CAT, obj: &self.element, "Pushing initial events");

            let stream_id = format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
            let stream_start_evt = gst::event::StreamStart::builder(&stream_id)
                .group_id(gst::GroupId::next())
                .build();
            appsrc.src_pad.push_event(stream_start_evt).await;

            let caps = appsrc.settings.lock().unwrap().caps.clone();
            if let Some(caps) = caps {
                appsrc
                    .src_pad
                    .push_event(gst::event::Caps::new(&caps))
                    .await;
                *appsrc.configured_caps.lock().unwrap() = Some(caps.clone());
            }

            self.need_initial_events = false;
        }

        if self.need_segment {
            let segment_evt =
                gst::event::Segment::new(&gst::FormattedSegment::<gst::format::Time>::new());
            appsrc.src_pad.push_event(segment_evt).await;

            self.need_segment = false;
        }

        match item {
            StreamItem::Buffer(buffer) => {
                gst::log!(CAT, obj: &self.element, "Forwarding {:?}", buffer);
                appsrc.src_pad.push(buffer).await
            }
            StreamItem::Event(event) => {
                match event.view() {
                    gst::EventView::Eos(_) => {
                        // Let the caller push the event
                        Err(gst::FlowError::Eos)
                    }
                    _ => {
                        gst::log!(CAT, obj: &self.element, "Forwarding {:?}", event);
                        appsrc.src_pad.push_event(event).await;
                        Ok(gst::FlowSuccess::Ok)
                    }
                }
            }
        }
    }
}

impl TaskImpl for AppSrcTask {
    fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            let item = self.receiver.next().await.ok_or_else(|| {
                gst::error!(CAT, obj: &self.element, "SrcPad channel aborted");
                gst::element_error!(
                    &self.element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["streaming stopped, reason: channel aborted"]
                );
                gst::FlowError::Flushing
            })?;

            let res = self.push_item(item).await;
            match res {
                Ok(_) => {
                    gst::log!(CAT, obj: &self.element, "Successfully pushed item");
                }
                Err(gst::FlowError::Eos) => {
                    gst::debug!(CAT, obj: &self.element, "EOS");
                    let appsrc = self.element.imp();
                    appsrc.src_pad.push_event(gst::event::Eos::new()).await;
                }
                Err(gst::FlowError::Flushing) => {
                    gst::debug!(CAT, obj: &self.element, "Flushing");
                }
                Err(err) => {
                    gst::error!(CAT, obj: &self.element, "Got error {}", err);
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
            gst::log!(CAT, obj: &self.element, "Stopping task");

            self.flush();
            self.need_initial_events = true;
            self.need_segment = true;

            gst::log!(CAT, obj: &self.element, "Task stopped");
            Ok(())
        }
        .boxed()
    }

    fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst::log!(CAT, obj: &self.element, "Starting task flush");

            self.flush();
            self.need_segment = true;

            gst::log!(CAT, obj: &self.element, "Task flush started");
            Ok(())
        }
        .boxed()
    }
}

#[derive(Debug)]
pub struct AppSrc {
    src_pad: PadSrc,
    task: Task,
    sender: Mutex<Option<mpsc::Sender<StreamItem>>>,
    configured_caps: Mutex<Option<gst::Caps>>,
    settings: Mutex<Settings>,
}

impl AppSrc {
    fn push_buffer(&self, element: &super::AppSrc, mut buffer: gst::Buffer) -> bool {
        let state = self.task.lock_state();
        if *state != TaskState::Started && *state != TaskState::Paused {
            gst::debug!(CAT, obj: element, "Rejecting buffer due to element state");
            return false;
        }

        let do_timestamp = self.settings.lock().unwrap().do_timestamp;
        if do_timestamp {
            if let Some(clock) = element.clock() {
                let base_time = element.base_time();
                let now = clock.time();

                let buffer = buffer.make_mut();
                buffer.set_dts(now.opt_checked_sub(base_time).ok().flatten());
                buffer.set_pts(None);
            } else {
                gst::error!(CAT, obj: element, "Don't have a clock yet");
                return false;
            }
        }

        let mut sender = self.sender.lock().unwrap();
        match sender
            .as_mut()
            .unwrap()
            .try_send(StreamItem::Buffer(buffer))
        {
            Ok(_) => true,
            Err(err) => {
                gst::error!(CAT, obj: element, "Failed to queue buffer: {}", err);
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
                gst::error!(CAT, obj: element, "Failed to queue EOS: {}", err);
                false
            }
        }
    }

    fn prepare(&self, element: &super::AppSrc) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap();
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
        drop(settings);

        *self.configured_caps.lock().unwrap() = None;

        let (sender, receiver) = mpsc::channel(max_buffers);
        *self.sender.lock().unwrap() = Some(sender);

        self.task
            .prepare(AppSrcTask::new(element.clone(), receiver), context)
            .block_on()?;

        gst::debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &super::AppSrc) {
        gst::debug!(CAT, obj: element, "Unpreparing");

        *self.sender.lock().unwrap() = None;
        self.task.unprepare().block_on().unwrap();

        gst::debug!(CAT, obj: element, "Unprepared");
    }

    fn stop(&self, element: &super::AppSrc) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Stopping");
        self.task.stop().block_on()?;
        gst::debug!(CAT, obj: element, "Stopped");
        Ok(())
    }

    fn start(&self, element: &super::AppSrc) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Starting");
        self.task.start().block_on()?;
        gst::debug!(CAT, obj: element, "Started");
        Ok(())
    }

    fn pause(&self, element: &super::AppSrc) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Pausing");
        let _ = self.task.pause().check()?;
        gst::debug!(CAT, obj: element, "Paused");
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for AppSrc {
    const NAME: &'static str = "RsTsAppSrc";
    type Type = super::AppSrc;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        Self {
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap(), Some("src")),
                AppSrcPadHandler,
            ),
            task: Task::default(),
            sender: Default::default(),
            configured_caps: Default::default(),
            settings: Default::default(),
        }
    }
}

impl ObjectImpl for AppSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::new(
                    "context",
                    "Context",
                    "Context name to share threads with",
                    Some(DEFAULT_CONTEXT),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecUInt::new(
                    "context-wait",
                    "Context Wait",
                    "Throttle poll loop to run at most once every this many ms",
                    0,
                    1000,
                    DEFAULT_CONTEXT_WAIT.as_millis() as u32,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecUInt::new(
                    "max-buffers",
                    "Max Buffers",
                    "Maximum number of buffers to queue up",
                    1,
                    u32::MAX,
                    DEFAULT_MAX_BUFFERS,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecBoxed::new(
                    "caps",
                    "Caps",
                    "Caps to use",
                    gst::Caps::static_type(),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecBoolean::new(
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
                glib::subclass::Signal::builder("push-buffer")
                    .param_types(&[gst::Buffer::static_type()])
                    .return_type::<bool>()
                    .action()
                    .class_handler(|_, args| {
                        let element = args[0].get::<super::AppSrc>().expect("signal arg");
                        let buffer = args[1].get::<gst::Buffer>().expect("signal arg");
                        let appsrc = element.imp();

                        Some(appsrc.push_buffer(&element, buffer).to_value())
                    })
                    .build(),
                glib::subclass::Signal::builder("end-of-stream")
                    .return_type::<bool>()
                    .action()
                    .class_handler(|_, args| {
                        let element = args[0].get::<super::AppSrc>().expect("signal arg");
                        let appsrc = element.imp();

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
                    .unwrap_or_else(|| DEFAULT_CONTEXT.into());
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

impl GstObjectImpl for AppSrc {}

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
        gst::trace!(CAT, obj: element, "Changing state {:?}", transition);

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
