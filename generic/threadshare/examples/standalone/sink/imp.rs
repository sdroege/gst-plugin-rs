// Copyright (C) 2022 François Laignel <fengalin@free.fr>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::Peekable;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::EventView;
use gst::{element_error, error_msg};

use once_cell::sync::Lazy;

use gstthreadshare::runtime::prelude::*;
use gstthreadshare::runtime::{self, Context, PadSink, PadSinkRef, Task};

use std::pin::Pin;
use std::sync::Mutex;
use std::task::Poll;
use std::time::{Duration, Instant};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-standalone-test-sink",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing standalone test sink"),
    )
});

const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: Duration = Duration::from_millis(20);
const DEFAULT_SYNC: bool = true;
const DEFAULT_MUST_LOG_STATS: bool = false;

const LOG_PERIOD: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
struct Settings {
    sync: bool,
    context: String,
    context_wait: Duration,
    must_log_stats: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            sync: DEFAULT_SYNC,
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            must_log_stats: DEFAULT_MUST_LOG_STATS,
        }
    }
}

#[derive(Debug)]
enum TaskItem {
    Buffer(gst::Buffer),
    Event(gst::Event),
}

#[derive(Clone, Debug)]
struct TestSinkPadHandler;

impl PadSinkHandler for TestSinkPadHandler {
    type ElementImpl = TestSink;

    fn sink_chain(
        &self,
        _pad: &PadSinkRef,
        test_sink: &TestSink,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let sender = test_sink.clone_item_sender();
        let element = element.clone().downcast::<super::TestSink>().unwrap();

        async move {
            if sender.send_async(TaskItem::Buffer(buffer)).await.is_err() {
                gst::debug!(CAT, obj: &element, "Flushing");
                return Err(gst::FlowError::Flushing);
            }

            Ok(gst::FlowSuccess::Ok)
        }
        .boxed()
    }

    fn sink_chain_list(
        &self,
        _pad: &PadSinkRef,
        test_sink: &TestSink,
        element: &gst::Element,
        list: gst::BufferList,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let sender = test_sink.clone_item_sender();
        let element = element.clone().downcast::<super::TestSink>().unwrap();

        async move {
            for buffer in list.iter_owned() {
                if sender.send_async(TaskItem::Buffer(buffer)).await.is_err() {
                    gst::debug!(CAT, obj: &element, "Flushing");
                    return Err(gst::FlowError::Flushing);
                }
            }

            Ok(gst::FlowSuccess::Ok)
        }
        .boxed()
    }

    fn sink_event_serialized(
        &self,
        _pad: &PadSinkRef,
        test_sink: &TestSink,
        element: &gst::Element,
        event: gst::Event,
    ) -> BoxFuture<'static, bool> {
        let sender = test_sink.clone_item_sender();
        let element = element.clone().downcast::<super::TestSink>().unwrap();

        async move {
            if let EventView::FlushStop(_) = event.view() {
                let test_sink = element.imp();
                return test_sink.task.flush_stop().await_maybe_on_context().is_ok();
            } else if sender.send_async(TaskItem::Event(event)).await.is_err() {
                gst::debug!(CAT, obj: &element, "Flushing");
            }

            true
        }
        .boxed()
    }

    fn sink_event(
        &self,
        _pad: &PadSinkRef,
        test_sink: &TestSink,
        _element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        if let EventView::FlushStart(..) = event.view() {
            return test_sink
                .task
                .flush_start()
                .await_maybe_on_context()
                .is_ok();
        }

        true
    }
}

#[derive(Default)]
struct Stats {
    must_log: bool,
    sync: bool,
    ramp_up_instant: Option<Instant>,
    log_start_instant: Option<Instant>,
    last_delta_instant: Option<Instant>,
    buffer_count: u32,
    buffer_count_delta: u32,
    buffer_headroom: Duration,
    buffer_headroom_delta: Duration,
    late_buffer_count: u32,
    lateness: Duration,
    max_lateness: Duration,
    late_buffer_count_delta: u32,
    lateness_delta: Duration,
    max_lateness_delta: Duration,
}

impl Stats {
    fn start(&mut self) {
        if !self.must_log {
            return;
        }

        self.buffer_count = 0;
        self.buffer_count_delta = 0;
        self.buffer_headroom = Duration::ZERO;
        self.buffer_headroom_delta = Duration::ZERO;
        self.late_buffer_count = 0;
        self.lateness = Duration::ZERO;
        self.max_lateness = Duration::ZERO;
        self.late_buffer_count_delta = 0;
        self.lateness_delta = Duration::ZERO;
        self.max_lateness_delta = Duration::ZERO;
        self.last_delta_instant = None;
        self.log_start_instant = None;

        self.ramp_up_instant = Some(Instant::now());
        gst::info!(CAT, "First stats logs in {:.2?}", 2 * LOG_PERIOD);
    }

    fn can_count(&mut self) -> bool {
        if !self.must_log {
            return false;
        }

        if let Some(ramp_up_instant) = self.ramp_up_instant {
            if ramp_up_instant.elapsed() < LOG_PERIOD {
                return false;
            }

            self.ramp_up_instant = None;
            gst::info!(CAT, "Ramp up complete. Stats logs in {:.2?}", LOG_PERIOD);
            self.log_start_instant = Some(Instant::now());
            self.last_delta_instant = self.log_start_instant;
        }

        true
    }

    fn notify_buffer(&mut self) {
        if !self.can_count() {
            return;
        }

        self.buffer_count += 1;
        self.buffer_count_delta += 1;
    }

    fn notify_buffer_headroom(&mut self, headroom: Duration) {
        if !self.can_count() {
            return;
        }

        self.buffer_headroom += headroom;
        self.buffer_headroom_delta += headroom;
    }

    fn notify_late_buffer(&mut self, now: Option<gst::ClockTime>, pts: gst::ClockTime) {
        if !self.can_count() {
            return;
        }

        let lateness = now
            .opt_checked_sub(pts)
            .ok()
            .flatten()
            .map_or(Duration::ZERO, Duration::from);

        self.late_buffer_count += 1;
        self.lateness += lateness;
        self.max_lateness = self.max_lateness.max(lateness);

        self.late_buffer_count_delta += 1;
        self.lateness_delta += lateness;
        self.max_lateness_delta = self.max_lateness_delta.max(lateness);
    }

    fn log_delta(&mut self) {
        let delta_duration = match self.last_delta_instant {
            Some(last_delta) => last_delta.elapsed(),
            None => return,
        };

        if delta_duration < LOG_PERIOD {
            return;
        }

        self.last_delta_instant = Some(Instant::now());

        gst::info!(CAT, "Delta stats:");
        gst::info!(
            CAT,
            "o {:>5.2} buffers / s",
            self.buffer_count_delta as f32 / delta_duration.as_millis() as f32 * 1_000f32,
        );

        if self.sync && self.buffer_count_delta > 0 {
            let early_buffers_count = self
                .buffer_count_delta
                .saturating_sub(self.late_buffer_count_delta);
            if early_buffers_count > 0 {
                gst::info!(
                    CAT,
                    "o {:>5.2?} headroom / early buffers",
                    self.buffer_headroom_delta / early_buffers_count,
                );

                gst::info!(
                    CAT,
                    "o {:>5.2}% late buffers - mean {:>5.2?}, max {:>5.2?}",
                    self.late_buffer_count_delta as f32 / self.buffer_count_delta as f32 * 100f32,
                    self.lateness_delta
                        .checked_div(self.late_buffer_count_delta)
                        .unwrap_or(Duration::ZERO),
                    self.max_lateness_delta,
                );
            }

            self.buffer_headroom_delta = Duration::ZERO;
            self.late_buffer_count_delta = 0;
            self.lateness_delta = Duration::ZERO;
            self.max_lateness_delta = Duration::ZERO;
        }

        self.buffer_count_delta = 0;
    }

    fn log_global(&mut self) {
        let log_duration = match self.log_start_instant {
            Some(start) => start.elapsed(),
            None => return,
        };

        gst::info!(CAT, "Global stats:");
        gst::info!(
            CAT,
            "o {:>5.2} buffers / s",
            self.buffer_count as f32 / log_duration.as_millis() as f32 * 1_000f32,
        );

        if self.sync && self.buffer_count > 0 {
            let early_buffers_count = self.buffer_count.saturating_sub(self.late_buffer_count);
            if early_buffers_count > 0 {
                gst::info!(
                    CAT,
                    "o {:>5.2?} headroom / early buffers",
                    self.buffer_headroom / early_buffers_count,
                );

                gst::info!(
                    CAT,
                    "o {:>5.2}% late buffers - mean {:>5.2?}, max {:>5.2?}",
                    self.late_buffer_count as f32 / self.buffer_count as f32 * 100f32,
                    self.lateness
                        .checked_div(self.late_buffer_count)
                        .unwrap_or(Duration::ZERO),
                    self.max_lateness,
                );
            }
        }
    }
}

struct TestSinkTask {
    element: super::TestSink,
    item_receiver: Peekable<flume::r#async::RecvStream<'static, TaskItem>>,
    sync: bool,
    stats: Stats,
    segment: Option<gst::Segment>,
}

impl TestSinkTask {
    fn new(element: &super::TestSink, item_receiver: flume::Receiver<TaskItem>) -> Self {
        TestSinkTask {
            element: element.clone(),
            item_receiver: item_receiver.into_stream().peekable(),
            sync: DEFAULT_SYNC,
            stats: Stats::default(),
            segment: None,
        }
    }

    async fn flush(&mut self) {
        // Purge the channel
        while let Poll::Ready(Some(_item)) = futures::poll!(self.item_receiver.next()) {}
    }
}

impl TaskImpl for TestSinkTask {
    type Item = TaskItem;

    fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst::log!(CAT, obj: &self.element, "Preparing Task");

            let sink = self.element.imp();
            let settings = sink.settings.lock().unwrap();
            self.sync = settings.sync;
            self.stats.sync = self.sync;
            self.stats.must_log = settings.must_log_stats;

            Ok(())
        }
        .boxed()
    }

    fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async {
            gst::log!(CAT, obj: &self.element, "Starting Task");
            self.stats.start();
            Ok(())
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async {
            gst::log!(CAT, obj: &self.element, "Stopping Task");
            self.flush().await;
            Ok(())
        }
        .boxed()
    }

    fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async {
            gst::log!(CAT, obj: &self.element, "Starting Task Flush");
            self.flush().await;
            Ok(())
        }
        .boxed()
    }

    fn try_next(&mut self) -> BoxFuture<'_, Result<TaskItem, gst::FlowError>> {
        async move {
            let item_opt = Pin::new(&mut self.item_receiver).peek().await;

            // Check the peeked item in case we need to sync.
            // The item will still be available in the channel
            // in case this is cancelled by a state transition.
            match item_opt {
                Some(TaskItem::Buffer(buffer)) => {
                    self.stats.notify_buffer();

                    if self.sync {
                        let rtime = self.segment.as_ref().and_then(|segment| {
                            segment
                                .downcast_ref::<gst::format::Time>()
                                .and_then(|segment| segment.to_running_time(buffer.pts()))
                        });
                        if let Some(pts) = rtime {
                            // This can be cancelled by a state transition.
                            self.sync(pts).await;
                        }
                    }
                }
                Some(_) => (),
                None => {
                    panic!("Internal channel sender dropped while Task is Started");
                }
            }

            // An item was peeked above, we can now pop it without losing it.
            Ok(self.item_receiver.next().await.unwrap())
        }
        .boxed()
    }

    fn handle_item(&mut self, item: TaskItem) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            gst::debug!(CAT, obj: &self.element, "Handling {:?}", item);

            match item {
                TaskItem::Buffer(buffer) => {
                    self.render(buffer).await.map_err(|err| {
                        element_error!(
                            &self.element,
                            gst::StreamError::Failed,
                            ["Failed to render item, stopping task: {}", err]
                        );
                        gst::FlowError::Error
                    })?;

                    self.stats.log_delta();
                }
                TaskItem::Event(event) => match event.view() {
                    EventView::Eos(_) => {
                        self.stats.log_global();

                        let _ = self
                            .element
                            .post_message(gst::message::Eos::builder().src(&self.element).build());
                    }
                    EventView::Segment(e) => {
                        self.segment = Some(e.segment().clone());
                    }
                    EventView::SinkMessage(e) => {
                        let _ = self.element.post_message(e.message());
                    }
                    _ => (),
                },
            }

            Ok(())
        }
        .boxed()
    }
}

impl TestSinkTask {
    async fn render(&mut self, buffer: gst::Buffer) -> Result<(), gst::FlowError> {
        let _data = buffer.map_readable().map_err(|_| {
            element_error!(
                self.element,
                gst::StreamError::Format,
                ["Failed to map buffer readable"]
            );
            gst::FlowError::Error
        })?;

        gst::log!(CAT, obj: &self.element, "buffer {:?} rendered", buffer);

        Ok(())
    }

    /// Waits until specified time.
    async fn sync(&mut self, pts: gst::ClockTime) {
        let now = self.element.current_running_time();

        if let Ok(Some(delay)) = pts.opt_checked_sub(now) {
            let delay = delay.into();
            gst::trace!(CAT, obj: &self.element, "sync: waiting {:?}", delay);
            runtime::time::delay_for(delay).await;

            self.stats.notify_buffer_headroom(delay);
        } else {
            self.stats.notify_late_buffer(now, pts);
        }
    }
}

#[derive(Debug)]
pub struct TestSink {
    sink_pad: PadSink,
    task: Task,
    item_sender: Mutex<Option<flume::Sender<TaskItem>>>,
    settings: Mutex<Settings>,
}

impl TestSink {
    #[track_caller]
    fn clone_item_sender(&self) -> flume::Sender<TaskItem> {
        self.item_sender.lock().unwrap().as_ref().unwrap().clone()
    }

    fn prepare(&self, element: &super::TestSink) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Preparing");

        let context = {
            let settings = self.settings.lock().unwrap();

            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to acquire Context: {}", err]
                )
            })?
        };

        // Enable backpressure for items
        let (item_sender, item_receiver) = flume::bounded(0);
        let task_impl = TestSinkTask::new(element, item_receiver);
        self.task.prepare(task_impl, context).block_on()?;

        *self.item_sender.lock().unwrap() = Some(item_sender);

        gst::debug!(CAT, obj: element, "Started preparation");

        Ok(())
    }

    fn unprepare(&self, element: &super::TestSink) {
        gst::debug!(CAT, obj: element, "Unpreparing");
        self.task.unprepare().block_on().unwrap();
        gst::debug!(CAT, obj: element, "Unprepared");
    }

    fn stop(&self, element: &super::TestSink) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Stopping");
        self.task.stop().block_on()?;
        gst::debug!(CAT, obj: element, "Stopped");
        Ok(())
    }

    fn start(&self, element: &super::TestSink) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, obj: element, "Starting");
        self.task.start().block_on()?;
        gst::debug!(CAT, obj: element, "Started");
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TestSink {
    const NAME: &'static str = "StandaloneTestSink";
    type Type = super::TestSink;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        Self {
            sink_pad: PadSink::new(
                gst::Pad::from_template(&klass.pad_template("sink").unwrap(), Some("sink")),
                TestSinkPadHandler,
            ),
            task: Task::default(),
            item_sender: Default::default(),
            settings: Default::default(),
        }
    }
}

impl ObjectImpl for TestSink {
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
                glib::ParamSpecBoolean::builder("sync")
                    .nick("Sync")
                    .blurb("Sync on the clock")
                    .default_value(DEFAULT_SYNC)
                    .build(),
                glib::ParamSpecBoolean::builder("must-log-stats")
                    .nick("Must Log Stats")
                    .blurb("Whether statistics should be logged")
                    .default_value(DEFAULT_MUST_LOG_STATS)
                    .write_only()
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
            "sync" => {
                let sync = value.get().expect("type checked upstream");
                settings.sync = sync;
            }
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
            "must-log-stats" => {
                let must_log_stats = value.get().expect("type checked upstream");
                settings.must_log_stats = must_log_stats;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "sync" => settings.sync.to_value(),
            "context" => settings.context.to_value(),
            "context-wait" => (settings.context_wait.as_millis() as u32).to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(self.sink_pad.gst_pad()).unwrap();

        gstthreadshare::set_element_flags(obj, gst::ElementFlags::SINK);
    }
}

impl GstObjectImpl for TestSink {}

impl ElementImpl for TestSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing standalone test sink",
                "Sink/Test",
                "Thread-sharing standalone test sink",
                "François Laignel <fengalin@free.fr>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template]
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
            gst::StateChange::ReadyToPaused => {
                self.start(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PausedToReady => {
                self.stop(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element);
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }
}
