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
use std::time::Duration;

use gstthreadshare::runtime::prelude::*;
use gstthreadshare::runtime::{timer, Context, PadSrc, Task};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-standalone-test-src",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing standalone test src"),
    )
});

const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: Duration = Duration::from_millis(20);
const DEFAULT_PUSH_PERIOD: gst::ClockTime = gst::ClockTime::from_mseconds(20);
const DEFAULT_NUM_BUFFERS: i32 = 50 * 100;

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_wait: Duration,
    push_period: gst::ClockTime,
    raise_log_level: bool,
    num_buffers: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            push_period: DEFAULT_PUSH_PERIOD,
            raise_log_level: false,
            num_buffers: Some(DEFAULT_NUM_BUFFERS as u32),
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
    timer: Option<timer::Interval>,
    raise_log_level: bool,
    push_period: gst::ClockTime,
    need_initial_events: bool,
    need_segment: bool,
    num_buffers: Option<u32>,
    buffer_count: u32,
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
            timer: None,
            raise_log_level: false,
            push_period: gst::ClockTime::ZERO,
            need_initial_events: true,
            need_segment: true,
            num_buffers: Some(DEFAULT_NUM_BUFFERS as u32),
            buffer_count: 0,
        }
    }
}

impl TaskImpl for SrcTask {
    type Item = gst::Buffer;

    fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            let src = self.element.imp();
            let settings = src.settings.lock().unwrap();
            self.raise_log_level = settings.raise_log_level;

            if self.raise_log_level {
                gst::log!(CAT, obj: &self.element, "Preparing Task");
            } else {
                gst::trace!(CAT, obj: &self.element, "Preparing Task");
            }

            self.push_period = settings.push_period;
            self.num_buffers = settings.num_buffers;

            Ok(())
        }
        .boxed()
    }

    fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async {
            if self.raise_log_level {
                gst::log!(CAT, obj: &self.element, "Starting Task");
            } else {
                gst::trace!(CAT, obj: &self.element, "Starting Task");
            }

            self.timer = Some(
                timer::interval_delayed_by(
                    // Delay first buffer push so as to let others start.
                    Duration::from_secs(2),
                    self.push_period.into(),
                )
                .expect("push period must be greater than 0"),
            );
            self.buffer_count = 0;
            self.buffer_pool.set_active(true).unwrap();
            Ok(())
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            if self.raise_log_level {
                gst::log!(CAT, obj: &self.element, "Stopping Task");
            } else {
                gst::trace!(CAT, obj: &self.element, "Stopping Task");
            }

            self.buffer_pool.set_active(false).unwrap();
            self.timer = None;
            self.need_initial_events = true;
            self.need_segment = true;

            Ok(())
        }
        .boxed()
    }

    fn try_next(&mut self) -> BoxFuture<'_, Result<gst::Buffer, gst::FlowError>> {
        async move {
            if self.raise_log_level {
                gst::log!(CAT, obj: &self.element, "Awaiting timer");
            } else {
                gst::trace!(CAT, obj: &self.element, "Awaiting timer");
            }

            self.timer.as_mut().unwrap().next().await;

            if self.raise_log_level {
                gst::log!(CAT, obj: &self.element, "Timer ticked");
            } else {
                gst::trace!(CAT, obj: &self.element, "Timer ticked");
            }

            self.buffer_pool
                .acquire_buffer(None)
                .map(|mut buffer| {
                    {
                        let buffer = buffer.get_mut().unwrap();
                        let rtime = self.element.current_running_time().unwrap();
                        buffer.set_dts(rtime);
                    }
                    buffer
                })
                .map_err(|err| {
                    gst::error!(CAT, obj: &self.element, "Failed to acquire buffer {}", err);
                    err
                })
        }
        .boxed()
    }

    fn handle_item(&mut self, buffer: gst::Buffer) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            let res = self.push(buffer).await;
            match res {
                Ok(_) => {
                    if self.raise_log_level {
                        gst::log!(CAT, obj: &self.element, "Successfully pushed buffer");
                    } else {
                        gst::trace!(CAT, obj: &self.element, "Successfully pushed buffer");
                    }
                }
                Err(gst::FlowError::Eos) => {
                    if self.raise_log_level {
                        gst::debug!(CAT, obj: &self.element, "EOS");
                    } else {
                        gst::trace!(CAT, obj: &self.element, "EOS");
                    }
                    let test_src = self.element.imp();
                    test_src.src_pad.push_event(gst::event::Eos::new()).await;

                    return Err(gst::FlowError::Eos);
                }
                Err(gst::FlowError::Flushing) => {
                    if self.raise_log_level {
                        gst::debug!(CAT, obj: &self.element, "Flushing");
                    } else {
                        gst::trace!(CAT, obj: &self.element, "Flushing");
                    }
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
        if self.raise_log_level {
            gst::debug!(CAT, obj: &self.element, "Pushing {:?}", buffer);
        } else {
            gst::trace!(CAT, obj: &self.element, "Pushing {:?}", buffer);
        }

        let test_src = self.element.imp();

        if self.need_initial_events {
            if self.raise_log_level {
                gst::debug!(CAT, obj: &self.element, "Pushing initial events");
            } else {
                gst::trace!(CAT, obj: &self.element, "Pushing initial events");
            }

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

        if self.raise_log_level {
            gst::debug!(CAT, obj: &self.element, "Forwarding buffer");
        } else {
            gst::trace!(CAT, obj: &self.element, "Forwarding buffer");
        }

        let ok = test_src.src_pad.push(buffer).await?;

        self.buffer_count += 1;

        if self.num_buffers.opt_eq(self.buffer_count).unwrap_or(false) {
            if self.raise_log_level {
                gst::debug!(CAT, obj: &self.element, "Pushing EOS");
            } else {
                gst::trace!(CAT, obj: &self.element, "Pushing EOS");
            }

            let test_src = self.element.imp();
            if !test_src.src_pad.push_event(gst::event::Eos::new()).await {
                gst::error!(CAT, obj: &self.element, "Error pushing EOS");
            }
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
        let raise_log_level = self.settings.lock().unwrap().raise_log_level;
        if raise_log_level {
            gst::debug!(CAT, obj: element, "Preparing");
        } else {
            gst::trace!(CAT, obj: element, "Preparing");
        }

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

        if raise_log_level {
            gst::debug!(CAT, obj: element, "Prepared");
        } else {
            gst::trace!(CAT, obj: element, "Prepared");
        }

        Ok(())
    }

    fn unprepare(&self, element: &super::TestSrc) {
        let raise_log_level = self.settings.lock().unwrap().raise_log_level;
        if raise_log_level {
            gst::debug!(CAT, obj: element, "Unpreparing");
        } else {
            gst::trace!(CAT, obj: element, "Unpreparing");
        }

        self.task.unprepare().block_on().unwrap();

        if raise_log_level {
            gst::debug!(CAT, obj: element, "Unprepared");
        } else {
            gst::trace!(CAT, obj: element, "Unprepared");
        }
    }

    fn stop(&self, element: &super::TestSrc) -> Result<(), gst::ErrorMessage> {
        let raise_log_level = self.settings.lock().unwrap().raise_log_level;
        if raise_log_level {
            gst::debug!(CAT, obj: element, "Stopping");
        } else {
            gst::trace!(CAT, obj: element, "Stopping");
        }

        self.task.stop().block_on()?;

        if raise_log_level {
            gst::debug!(CAT, obj: element, "Stopped");
        } else {
            gst::trace!(CAT, obj: element, "Stopped");
        }

        Ok(())
    }

    fn start(&self, element: &super::TestSrc) -> Result<(), gst::ErrorMessage> {
        let raise_log_level = self.settings.lock().unwrap().raise_log_level;
        if raise_log_level {
            gst::debug!(CAT, obj: element, "Starting");
        } else {
            gst::trace!(CAT, obj: element, "Starting");
        }

        self.task.start().block_on()?;

        if raise_log_level {
            gst::debug!(CAT, obj: element, "Started");
        } else {
            gst::trace!(CAT, obj: element, "Started");
        }

        Ok(())
    }

    fn pause(&self, element: &super::TestSrc) -> Result<(), gst::ErrorMessage> {
        let raise_log_level = self.settings.lock().unwrap().raise_log_level;
        if raise_log_level {
            gst::debug!(CAT, obj: element, "Pausing");
        } else {
            gst::trace!(CAT, obj: element, "Pausing");
        }

        self.task.pause().block_on()?;

        if raise_log_level {
            gst::debug!(CAT, obj: element, "Paused");
        } else {
            gst::trace!(CAT, obj: element, "Paused");
        }

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
                glib::ParamSpecUInt::builder("push-period")
                    .nick("Buffer Push Period")
                    .blurb("Push a new buffer every this many ms")
                    .default_value(DEFAULT_PUSH_PERIOD.mseconds() as u32)
                    .build(),
                glib::ParamSpecBoolean::builder("raise-log-level")
                    .nick("Raise log level")
                    .blurb("Raises the log level so that this element stands out")
                    .write_only()
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
            "push-period" => {
                settings.push_period = gst::ClockTime::from_mseconds(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            "raise-log-level" => {
                settings.raise_log_level = value.get::<bool>().expect("type checked upstream");
            }
            "num-buffers" => {
                let value = value.get::<i32>().expect("type checked upstream");
                settings.num_buffers = if value > 0 { Some(value as u32) } else { None };
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "context" => settings.context.to_value(),
            "context-wait" => (settings.context_wait.as_millis() as u32).to_value(),
            "push-period" => (settings.push_period.mseconds() as u32).to_value(),
            "raise-log-level" => settings.raise_log_level.to_value(),
            "num-buffers" => settings
                .num_buffers
                .and_then(|val| val.try_into().ok())
                .unwrap_or(-1i32)
                .to_value(),
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
