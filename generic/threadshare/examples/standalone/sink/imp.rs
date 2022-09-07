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

use gst::error_msg;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::EventView;

use once_cell::sync::Lazy;

use gstthreadshare::runtime::prelude::*;
use gstthreadshare::runtime::{Context, PadSink, PadSinkRef, Task};

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
const DEFAULT_PUSH_PERIOD: Duration = Duration::from_millis(20);
const DEFAULT_MAX_BUFFERS: i32 = 50 * (100 - 25);

const LOG_PERIOD: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_wait: Duration,
    raise_log_level: bool,
    logs_stats: bool,
    push_period: Duration,
    max_buffers: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            raise_log_level: false,
            logs_stats: false,
            push_period: DEFAULT_PUSH_PERIOD,
            max_buffers: Some(DEFAULT_MAX_BUFFERS as u32),
        }
    }
}

#[derive(Debug)]
enum StreamItem {
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
            if sender.send_async(StreamItem::Buffer(buffer)).await.is_err() {
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
                if sender.send_async(StreamItem::Buffer(buffer)).await.is_err() {
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
            } else if sender.send_async(StreamItem::Event(event)).await.is_err() {
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
    ramp_up_instant: Option<Instant>,
    log_start_instant: Option<Instant>,
    last_delta_instant: Option<Instant>,
    max_buffers: Option<f32>,
    buffer_count: f32,
    buffer_count_delta: f32,
    latency_sum: f32,
    latency_square_sum: f32,
    latency_sum_delta: f32,
    latency_square_sum_delta: f32,
    latency_min: Duration,
    latency_min_delta: Duration,
    latency_max: Duration,
    latency_max_delta: Duration,
    interval_sum: f32,
    interval_square_sum: f32,
    interval_sum_delta: f32,
    interval_square_sum_delta: f32,
    interval_min: Duration,
    interval_min_delta: Duration,
    interval_max: Duration,
    interval_max_delta: Duration,
    interval_late_warn: Duration,
    interval_late_count: f32,
    interval_late_count_delta: f32,
    #[cfg(feature = "tuning")]
    parked_duration_init: Duration,
}

impl Stats {
    fn start(&mut self) {
        if !self.must_log {
            return;
        }

        self.buffer_count = 0.0;
        self.buffer_count_delta = 0.0;
        self.latency_sum = 0.0;
        self.latency_square_sum = 0.0;
        self.latency_sum_delta = 0.0;
        self.latency_square_sum_delta = 0.0;
        self.latency_min = Duration::MAX;
        self.latency_min_delta = Duration::MAX;
        self.latency_max = Duration::ZERO;
        self.latency_max_delta = Duration::ZERO;
        self.interval_sum = 0.0;
        self.interval_square_sum = 0.0;
        self.interval_sum_delta = 0.0;
        self.interval_square_sum_delta = 0.0;
        self.interval_min = Duration::MAX;
        self.interval_min_delta = Duration::MAX;
        self.interval_max = Duration::ZERO;
        self.interval_max_delta = Duration::ZERO;
        self.interval_late_count = 0.0;
        self.interval_late_count_delta = 0.0;
        self.last_delta_instant = None;
        self.log_start_instant = None;

        self.ramp_up_instant = Some(Instant::now());
        gst::info!(CAT, "First stats logs in {:2?}", 2 * LOG_PERIOD);
    }

    fn is_active(&mut self) -> bool {
        if !self.must_log {
            return false;
        }

        if let Some(ramp_up_instant) = self.ramp_up_instant {
            if ramp_up_instant.elapsed() < LOG_PERIOD {
                return false;
            }

            self.ramp_up_instant = None;
            gst::info!(CAT, "Ramp up complete. Stats logs in {:2?}", LOG_PERIOD);
            self.log_start_instant = Some(Instant::now());
            self.last_delta_instant = self.log_start_instant;

            #[cfg(feature = "tuning")]
            {
                self.parked_duration_init = Context::current().unwrap().parked_duration();
            }
        }

        use std::cmp::Ordering::*;
        match self.max_buffers.opt_cmp(self.buffer_count) {
            Some(Equal) => {
                self.log_global();
                self.buffer_count += 1.0;
                false
            }
            Some(Less) => false,
            _ => true,
        }
    }

    fn add_buffer(&mut self, latency: Duration, interval: Duration) {
        if !self.is_active() {
            return;
        }

        self.buffer_count += 1.0;
        self.buffer_count_delta += 1.0;

        // Latency
        let latency_f32 = latency.as_nanos() as f32;
        let latency_square = latency_f32.powi(2);

        self.latency_sum += latency_f32;
        self.latency_square_sum += latency_square;
        self.latency_min = self.latency_min.min(latency);
        self.latency_max = self.latency_max.max(latency);

        self.latency_sum_delta += latency_f32;
        self.latency_square_sum_delta += latency_square;
        self.latency_min_delta = self.latency_min_delta.min(latency);
        self.latency_max_delta = self.latency_max_delta.max(latency);

        // Interval
        let interval_f32 = interval.as_nanos() as f32;
        let interval_square = interval_f32.powi(2);

        self.interval_sum += interval_f32;
        self.interval_square_sum += interval_square;
        self.interval_min = self.interval_min.min(interval);
        self.interval_max = self.interval_max.max(interval);

        self.interval_sum_delta += interval_f32;
        self.interval_square_sum_delta += interval_square;
        self.interval_min_delta = self.interval_min_delta.min(interval);
        self.interval_max_delta = self.interval_max_delta.max(interval);

        if interval > self.interval_late_warn {
            self.interval_late_count += 1.0;
            self.interval_late_count_delta += 1.0;
        }

        let delta_duration = match self.last_delta_instant {
            Some(last_delta) => last_delta.elapsed(),
            None => return,
        };

        if delta_duration < LOG_PERIOD {
            return;
        }

        self.last_delta_instant = Some(Instant::now());

        gst::info!(CAT, "Delta stats:");
        let interval_mean = self.interval_sum_delta / self.buffer_count_delta;
        let interval_std_dev = f32::sqrt(
            self.interval_square_sum_delta / self.buffer_count_delta - interval_mean.powi(2),
        );

        gst::info!(
            CAT,
            "o interval: mean {:4.2?} σ {:4.1?} [{:4.1?}, {:4.1?}]",
            Duration::from_nanos(interval_mean as u64),
            Duration::from_nanos(interval_std_dev as u64),
            self.interval_min_delta,
            self.interval_max_delta,
        );

        if self.interval_late_count_delta > f32::EPSILON {
            gst::warning!(
                CAT,
                "o {:5.2}% late buffers",
                100f32 * self.interval_late_count_delta / self.buffer_count_delta
            );
        }

        self.interval_sum_delta = 0.0;
        self.interval_square_sum_delta = 0.0;
        self.interval_min_delta = Duration::MAX;
        self.interval_max_delta = Duration::ZERO;
        self.interval_late_count_delta = 0.0;

        let latency_mean = self.latency_sum_delta / self.buffer_count_delta;
        let latency_std_dev = f32::sqrt(
            self.latency_square_sum_delta / self.buffer_count_delta - latency_mean.powi(2),
        );

        gst::info!(
            CAT,
            "o latency: mean {:4.2?} σ {:4.1?} [{:4.1?}, {:4.1?}]",
            Duration::from_nanos(latency_mean as u64),
            Duration::from_nanos(latency_std_dev as u64),
            self.latency_min_delta,
            self.latency_max_delta,
        );

        self.latency_sum_delta = 0.0;
        self.latency_square_sum_delta = 0.0;
        self.latency_min_delta = Duration::MAX;
        self.latency_max_delta = Duration::ZERO;

        self.buffer_count_delta = 0.0;
    }

    fn log_global(&mut self) {
        if self.buffer_count < 1.0 {
            return;
        }

        let _log_start = if let Some(log_start) = self.log_start_instant {
            log_start
        } else {
            return;
        };

        gst::info!(CAT, "Global stats:");

        #[cfg(feature = "tuning")]
        {
            let duration = _log_start.elapsed();
            let parked_duration =
                Context::current().unwrap().parked_duration() - self.parked_duration_init;
            gst::info!(
                CAT,
                "o parked: {parked_duration:4.2?} ({:5.2?}%)",
                (parked_duration.as_nanos() as f32 * 100.0 / duration.as_nanos() as f32)
            );
        }

        let interval_mean = self.interval_sum / self.buffer_count;
        let interval_std_dev =
            f32::sqrt(self.interval_square_sum / self.buffer_count - interval_mean.powi(2));

        gst::info!(
            CAT,
            "o interval: mean {:4.2?} σ {:4.1?} [{:4.1?}, {:4.1?}]",
            Duration::from_nanos(interval_mean as u64),
            Duration::from_nanos(interval_std_dev as u64),
            self.interval_min,
            self.interval_max,
        );

        if self.interval_late_count > f32::EPSILON {
            gst::warning!(
                CAT,
                "o {:5.2}% late buffers",
                100f32 * self.interval_late_count / self.buffer_count
            );
        }

        let latency_mean = self.latency_sum / self.buffer_count;
        let latency_std_dev =
            f32::sqrt(self.latency_square_sum / self.buffer_count - latency_mean.powi(2));

        gst::info!(
            CAT,
            "o latency: mean {:4.2?} σ {:4.1?} [{:4.1?}, {:4.1?}]",
            Duration::from_nanos(latency_mean as u64),
            Duration::from_nanos(latency_std_dev as u64),
            self.latency_min,
            self.latency_max,
        );
    }
}

struct TestSinkTask {
    element: super::TestSink,
    raise_log_level: bool,
    last_dts: Option<gst::ClockTime>,
    item_receiver: Peekable<flume::r#async::RecvStream<'static, StreamItem>>,
    stats: Stats,
    segment: Option<gst::Segment>,
}

impl TestSinkTask {
    fn new(element: &super::TestSink, item_receiver: flume::Receiver<StreamItem>) -> Self {
        TestSinkTask {
            element: element.clone(),
            raise_log_level: false,
            last_dts: None,
            item_receiver: item_receiver.into_stream().peekable(),
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
    type Item = StreamItem;

    fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            let sink = self.element.imp();
            let settings = sink.settings.lock().unwrap();
            self.raise_log_level = settings.raise_log_level;

            if self.raise_log_level {
                gst::log!(CAT, obj: &self.element, "Preparing Task");
            } else {
                gst::trace!(CAT, obj: &self.element, "Preparing Task");
            }

            self.stats.must_log = settings.logs_stats;
            self.stats.max_buffers = settings.max_buffers.map(|max_buffers| max_buffers as f32);
            self.stats.interval_late_warn = settings.push_period + settings.context_wait / 2;

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

            self.last_dts = None;
            self.stats.start();
            Ok(())
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async {
            if self.raise_log_level {
                gst::log!(CAT, obj: &self.element, "Stopping Task");
            } else {
                gst::trace!(CAT, obj: &self.element, "Stopping Task");
            }

            self.flush().await;
            Ok(())
        }
        .boxed()
    }

    fn try_next(&mut self) -> BoxFuture<'_, Result<StreamItem, gst::FlowError>> {
        async move {
            let item = self.item_receiver.next().await.unwrap();

            if self.raise_log_level {
                gst::log!(CAT, obj: &self.element, "Popped item");
            } else {
                gst::trace!(CAT, obj: &self.element, "Popped item");
            }

            Ok(item)
        }
        .boxed()
    }

    fn handle_item(&mut self, item: StreamItem) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            if self.raise_log_level {
                gst::debug!(CAT, obj: &self.element, "Received {:?}", item);
            } else {
                gst::trace!(CAT, obj: &self.element, "Received {:?}", item);
            }

            match item {
                StreamItem::Buffer(buffer) => {
                    let dts = self
                        .segment
                        .as_ref()
                        .and_then(|segment| {
                            segment
                                .downcast_ref::<gst::format::Time>()
                                .and_then(|segment| segment.to_running_time(buffer.dts()))
                        })
                        .unwrap();

                    if let Some(last_dts) = self.last_dts {
                        let cur_ts = self.element.current_running_time().unwrap();
                        let latency: Duration = (cur_ts - dts).into();
                        let interval: Duration = (dts - last_dts).into();

                        self.stats.add_buffer(latency, interval);

                        if self.raise_log_level {
                            gst::debug!(CAT, obj: &self.element, "o latency {:.2?}", latency);
                            gst::debug!(CAT, obj: &self.element, "o interval {:.2?}", interval);
                        } else {
                            gst::trace!(CAT, obj: &self.element, "o latency {:.2?}", latency);
                            gst::trace!(CAT, obj: &self.element, "o interval {:.2?}", interval);
                        }
                    }

                    self.last_dts = Some(dts);

                    if self.raise_log_level {
                        gst::log!(CAT, obj: &self.element, "Buffer processed");
                    } else {
                        gst::trace!(CAT, obj: &self.element, "Buffer processed");
                    }
                }
                StreamItem::Event(event) => match event.view() {
                    EventView::Eos(_) => {
                        if self.raise_log_level {
                            gst::debug!(CAT, obj: &self.element, "EOS");
                        } else {
                            gst::trace!(CAT, obj: &self.element, "EOS");
                        }

                        let elem = self.element.clone();
                        self.element.call_async(move |_| {
                            let _ =
                                elem.post_message(gst::message::Eos::builder().src(&elem).build());
                        });

                        return Err(gst::FlowError::Eos);
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

#[derive(Debug)]
pub struct TestSink {
    sink_pad: PadSink,
    task: Task,
    item_sender: Mutex<Option<flume::Sender<StreamItem>>>,
    settings: Mutex<Settings>,
}

impl TestSink {
    #[track_caller]
    fn clone_item_sender(&self) -> flume::Sender<StreamItem> {
        self.item_sender.lock().unwrap().as_ref().unwrap().clone()
    }

    fn prepare(&self, element: &super::TestSink) -> Result<(), gst::ErrorMessage> {
        let raise_log_level = self.settings.lock().unwrap().raise_log_level;
        if raise_log_level {
            gst::debug!(CAT, obj: element, "Preparing");
        } else {
            gst::trace!(CAT, obj: element, "Preparing");
        }

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

        if raise_log_level {
            gst::debug!(CAT, obj: element, "Prepared");
        } else {
            gst::trace!(CAT, obj: element, "Prepared");
        }

        Ok(())
    }

    fn unprepare(&self, element: &super::TestSink) {
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

    fn stop(&self, element: &super::TestSink) -> Result<(), gst::ErrorMessage> {
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

    fn start(&self, element: &super::TestSink) -> Result<(), gst::ErrorMessage> {
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
                glib::ParamSpecBoolean::builder("raise-log-level")
                    .nick("Raise log level")
                    .blurb("Raises the log level so that this element stands out")
                    .write_only()
                    .build(),
                glib::ParamSpecBoolean::builder("logs-stats")
                    .nick("Logs Stats")
                    .blurb("Whether statistics should be logged")
                    .write_only()
                    .build(),
                glib::ParamSpecUInt::builder("push-period")
                    .nick("Src buffer Push Period")
                    .blurb("Push period used by `src` element (used for stats warnings)")
                    .default_value(DEFAULT_PUSH_PERIOD.as_millis() as u32)
                    .build(),
                glib::ParamSpecInt::builder("max-buffers")
                    .nick("Max Buffers")
                    .blurb("Number of buffers to count before stopping stats (-1 = unlimited)")
                    .minimum(-1i32)
                    .default_value(DEFAULT_MAX_BUFFERS)
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
            "raise-log-level" => {
                settings.raise_log_level = value.get::<bool>().expect("type checked upstream");
            }
            "logs-stats" => {
                let logs_stats = value.get().expect("type checked upstream");
                settings.logs_stats = logs_stats;
            }
            "push-period" => {
                settings.push_period = Duration::from_millis(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            "max-buffers" => {
                let value = value.get::<i32>().expect("type checked upstream");
                settings.max_buffers = if value > 0 { Some(value as u32) } else { None };
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "context" => settings.context.to_value(),
            "context-wait" => (settings.context_wait.as_millis() as u32).to_value(),
            "raise-log-level" => settings.raise_log_level.to_value(),
            "push-period" => (settings.push_period.as_millis() as u32).to_value(),
            "max-buffers" => settings
                .max_buffers
                .and_then(|val| val.try_into().ok())
                .unwrap_or(-1i32)
                .to_value(),
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
