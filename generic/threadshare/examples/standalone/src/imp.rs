// Copyright (C) 2022 François Laignel <fengalin@free.fr>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use futures::future::BoxFuture;
use futures::prelude::*;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use once_cell::sync::Lazy;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use gstthreadshare::runtime::prelude::*;
use gstthreadshare::runtime::{Context, PadSrc, Task, Timer};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-standalone-test-src",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing standalone test src"),
    )
});

const BUFFER_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(20);

const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: Duration = Duration::from_millis(20);
const DEFAULT_NUM_BUFFERS: i32 = 50 * 60 * 2;

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_wait: Duration,
    num_buffers: Option<i32>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            num_buffers: Some(DEFAULT_NUM_BUFFERS),
        }
    }
}

#[derive(Clone, Debug)]
struct TestSrcPadHandler;
impl PadSrcHandler for TestSrcPadHandler {
    type ElementImpl = TestSrc;
}

#[derive(Debug)]
struct SrcTask {
    element: super::TestSrc,
    buffer_pool: gst::BufferPool,
    last_pts: gst::ClockTime,
    last_buf_instant: Option<Instant>,
    push_period: Duration,
    need_initial_events: bool,
    need_segment: bool,
    num_buffers: Option<i32>,
    buffer_count: i32,
}

impl SrcTask {
    fn new(element: super::TestSrc) -> Self {
        let buffer_pool = gst::BufferPool::new();
        let mut pool_config = buffer_pool.config();
        pool_config
            .as_mut()
            .set_params(Some(&gst::Caps::builder("foo/bar").build()), 10, 10, 10);
        buffer_pool.set_config(pool_config).unwrap();

        SrcTask {
            element,
            buffer_pool,
            last_pts: gst::ClockTime::ZERO,
            last_buf_instant: None,
            push_period: Duration::ZERO,
            need_initial_events: true,
            need_segment: true,
            num_buffers: Some(DEFAULT_NUM_BUFFERS),
            buffer_count: 0,
        }
    }
}

impl TaskImpl for SrcTask {
    type Item = gst::Buffer;

    fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst::log!(CAT, obj: &self.element, "Preparing Task");

            let src = self.element.imp();
            let settings = src.settings.lock().unwrap();
            self.push_period = settings.context_wait;
            self.num_buffers = settings.num_buffers;

            Ok(())
        }
        .boxed()
    }

    fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async {
            gst::log!(CAT, obj: &self.element, "Starting Task");
            self.buffer_count = 0;
            self.last_buf_instant = None;
            self.buffer_pool.set_active(true).unwrap();
            Ok(())
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst::log!(CAT, obj: &self.element, "Stopping task");

            self.buffer_pool.set_active(false).unwrap();
            self.last_pts = gst::ClockTime::ZERO;
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

            self.buffer_pool.set_active(false).unwrap();
            self.need_segment = true;

            gst::log!(CAT, obj: &self.element, "Task flush started");
            Ok(())
        }
        .boxed()
    }

    fn try_next(&mut self) -> BoxFuture<'_, Result<gst::Buffer, gst::FlowError>> {
        async move {
            if let Some(delay) = self
                .last_buf_instant
                .map(|last| last.elapsed())
                .opt_checked_sub(self.push_period)
                .ok()
                .flatten()
            {
                Timer::after(delay).await;
            }

            self.last_buf_instant = Some(Instant::now());

            let start = self.last_pts;
            self.last_pts = start + BUFFER_DURATION;

            self.buffer_pool.acquire_buffer(None).map(|mut buffer| {
                {
                    let buffer = buffer.get_mut().unwrap();
                    buffer.set_pts(start);
                    buffer.set_duration(BUFFER_DURATION);
                }

                buffer
            })
        }
        .boxed()
    }

    fn handle_item(&mut self, buffer: gst::Buffer) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            let res = self.push(buffer).await;
            match res {
                Ok(_) => {
                    gst::log!(CAT, obj: &self.element, "Successfully pushed buffer");
                }
                Err(gst::FlowError::Eos) => {
                    gst::debug!(CAT, obj: &self.element, "EOS");
                    let test_src = self.element.imp();
                    test_src.src_pad.push_event(gst::event::Eos::new()).await;
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
}

impl SrcTask {
    async fn push(&mut self, buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj: &self.element, "Handling {:?}", buffer);
        let test_src = self.element.imp();

        if self.need_initial_events {
            gst::debug!(CAT, obj: &self.element, "Pushing initial events");

            let stream_id = format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
            let stream_start_evt = gst::event::StreamStart::builder(&stream_id)
                .group_id(gst::GroupId::next())
                .build();
            test_src.src_pad.push_event(stream_start_evt).await;

            test_src
                .src_pad
                .push_event(gst::event::Caps::new(
                    &gst::Caps::builder("foo/bar").build(),
                ))
                .await;

            self.need_initial_events = false;
        }

        if self.need_segment {
            let segment_evt =
                gst::event::Segment::new(&gst::FormattedSegment::<gst::format::Time>::new());
            test_src.src_pad.push_event(segment_evt).await;

            self.need_segment = false;
        }

        gst::debug!(CAT, obj: &self.element, "Forwarding {:?}", buffer);
        let ok = test_src.src_pad.push(buffer).await?;

        self.buffer_count += 1;

        if self.num_buffers.opt_eq(self.buffer_count).unwrap_or(false) {
            return Err(gst::FlowError::Eos);
        }

        Ok(ok)
    }
}

#[derive(Debug)]
pub struct TestSrc {
    src_pad: PadSrc,
    task: Task,
    settings: Mutex<Settings>,
}

impl TestSrc {
    fn prepare(&self, element: &super::TestSrc) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap();
        let context =
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;
        drop(settings);

        self.task
            .prepare(SrcTask::new(element.clone()), context)
            .block_on()?;

        gst::debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &super::TestSrc) {
        gst::debug!(CAT, obj: element, "Unpreparing");
        self.task.unprepare().block_on().unwrap();
        gst::debug!(CAT, obj: element, "Unprepared");
    }

    fn stop(&self, element: &super::TestSrc) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Stopping");
        self.task.stop().block_on()?;
        gst::debug!(CAT, obj: element, "Stopped");
        Ok(())
    }

    fn start(&self, element: &super::TestSrc) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Starting");
        self.task.start().block_on()?;
        gst::debug!(CAT, obj: element, "Started");
        Ok(())
    }

    fn pause(&self, element: &super::TestSrc) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Pausing");
        self.task.pause().block_on()?;
        gst::debug!(CAT, obj: element, "Paused");
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TestSrc {
    const NAME: &'static str = "StandaloneTestSrc";
    type Type = super::TestSrc;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        Self {
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap(), Some("src")),
                TestSrcPadHandler,
            ),
            task: Task::default(),
            settings: Default::default(),
        }
    }
}

impl ObjectImpl for TestSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("context")
                    .nick("Context")
                    .blurb("Context name to share threads with")
                    .default_value(Some(DEFAULT_CONTEXT))
                    .build(),
                glib::ParamSpecUInt::builder("context-wait")
                    .nick("Context Wait")
                    .blurb("Throttle poll loop to run at most once every this many ms")
                    .maximum(1000)
                    .default_value(DEFAULT_CONTEXT_WAIT.as_millis() as u32)
                    .build(),
                glib::ParamSpecInt::builder("num-buffers")
                    .nick("Num Buffers")
                    .blurb("Number of buffers to output before sending EOS (-1 = unlimited)")
                    .minimum(-1i32)
                    .default_value(DEFAULT_NUM_BUFFERS)
                    .build(),
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
            "num-buffers" => {
                let value = value.get::<i32>().expect("type checked upstream");
                settings.num_buffers = if value > 0 { Some(value) } else { None };
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "context" => settings.context.to_value(),
            "context-wait" => (settings.context_wait.as_millis() as u32).to_value(),
            "num-buffers" => settings.num_buffers.unwrap_or(-1).to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(self.src_pad.gst_pad()).unwrap();

        gstthreadshare::set_element_flags(obj, gst::ElementFlags::SOURCE);
    }
}

impl GstObjectImpl for TestSrc {}

impl ElementImpl for TestSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing standalone test source",
                "Source/Test",
                "Thread-sharing standalone test source",
                "François Laignel <fengalin@free.fr>",
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
