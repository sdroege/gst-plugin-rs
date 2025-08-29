// Copyright (C) 2022 François Laignel <fengalin@free.fr>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::error_msg;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::EventView;

use std::sync::LazyLock;

use gstthreadshare::runtime::prelude::*;
use gstthreadshare::runtime::{Context, PadSink, Task};

use std::sync::Mutex;

use super::super::{Settings, Stats, CAT};

#[derive(Debug)]
enum StreamItem {
    Buffer(gst::Buffer),
    Event(gst::Event),
}

#[derive(Clone, Debug)]
struct TaskPadSinkHandler;

impl PadSinkHandler for TaskPadSinkHandler {
    type ElementImpl = TaskSink;

    async fn sink_chain(
        self,
        _pad: gst::Pad,
        elem: super::TaskSink,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let sender = elem.imp().clone_item_sender();
        if sender.send_async(StreamItem::Buffer(buffer)).await.is_err() {
            return Err(gst::FlowError::Flushing);
        }

        Ok(gst::FlowSuccess::Ok)
    }

    async fn sink_event_serialized(
        self,
        pad: gst::Pad,
        elem: super::TaskSink,
        event: gst::Event,
    ) -> bool {
        let sender = elem.imp().clone_item_sender();
        match event.view() {
            EventView::Eos(_) => {
                let is_main_elem = elem.imp().settings.lock().unwrap().is_main_elem;
                if is_main_elem {
                    gst::info!(CAT, obj = elem, "EOS");

                    let _ = elem
                        .imp()
                        .clone_item_sender()
                        .send_async(StreamItem::Event(event))
                        .await;

                    let _ = elem.post_message(
                        gst::message::Application::builder(gst::Structure::new_empty(
                            "ts-standalone-sink/eos",
                        ))
                        .src(&elem)
                        .build(),
                    );
                }
            }
            EventView::FlushStop(_) => {
                let imp = elem.imp();
                return imp.task.flush_stop().block_on_or_add_subtask(&pad).is_ok();
            }
            EventView::Segment(_) => {
                let _ = sender.send_async(StreamItem::Event(event)).await;
                let _ = elem.post_message(
                    gst::message::Application::builder(gst::Structure::new_empty(
                        "ts-standalone-sink/streaming",
                    ))
                    .src(&elem)
                    .build(),
                );
            }
            EventView::SinkMessage(evt) => {
                let _ = elem.post_message(evt.message());
            }
            _ => (),
        }

        true
    }

    fn sink_event(self, pad: &gst::Pad, imp: &TaskSink, event: gst::Event) -> bool {
        if let EventView::FlushStart(..) = event.view() {
            return imp.task.flush_start().block_on_or_add_subtask(pad).is_ok();
        }

        true
    }
}

struct TaskSinkTask {
    elem: super::TaskSink,
    item_receiver: flume::Receiver<StreamItem>,
    is_main_elem: bool,
    last_ts: Option<gst::ClockTime>,
    segment: Option<gst::FormattedSegment<gst::format::Time>>,
    stats: Option<Box<Stats>>,
}

impl TaskSinkTask {
    fn new(
        elem: &super::TaskSink,
        item_receiver: flume::Receiver<StreamItem>,
        is_main_elem: bool,
        stats: Option<Box<Stats>>,
    ) -> Self {
        TaskSinkTask {
            elem: elem.clone(),
            item_receiver,
            is_main_elem,
            last_ts: None,
            stats,
            segment: None,
        }
    }

    fn flush(&mut self) {
        // Purge the channel
        while self.item_receiver.try_recv().is_ok() {}
    }
}

impl TaskImpl for TaskSinkTask {
    type Item = StreamItem;

    fn obj(&self) -> &impl IsA<glib::Object> {
        &self.elem
    }

    async fn prepare(&mut self) -> Result<(), gst::ErrorMessage> {
        log_or_trace!(CAT, self.is_main_elem, obj = self.elem, "Preparing Task");
        Ok(())
    }

    async fn start(&mut self) -> Result<(), gst::ErrorMessage> {
        log_or_trace!(CAT, self.is_main_elem, obj = self.elem, "Starting Task");
        self.last_ts = None;
        if let Some(stats) = self.stats.as_mut() {
            stats.start();
        }

        Ok(())
    }

    async fn stop(&mut self) -> Result<(), gst::ErrorMessage> {
        log_or_trace!(CAT, self.is_main_elem, obj = self.elem, "Stopping Task");
        self.flush();
        Ok(())
    }

    async fn try_next(&mut self) -> Result<StreamItem, gst::FlowError> {
        Ok(self.item_receiver.recv_async().await.unwrap())
    }

    async fn handle_item(&mut self, item: StreamItem) -> Result<(), gst::FlowError> {
        debug_or_trace!(CAT, self.is_main_elem, obj = self.elem, "Received {item:?}");

        match item {
            StreamItem::Buffer(buffer) => {
                let ts = self
                    .segment
                    .as_ref()
                    .expect("Buffer without Time Segment")
                    .to_running_time(buffer.dts_or_pts().expect("Buffer without ts"))
                    .unwrap();

                if let Some(last_ts) = self.last_ts {
                    let rt = self.elem.current_running_time().unwrap();
                    let lateness = rt.nseconds() as i64 - ts.nseconds() as i64;
                    let interval = ts.nseconds() as i64 - last_ts.nseconds() as i64;

                    if let Some(stats) = self.stats.as_mut() {
                        stats.add_buffer(lateness, interval);
                    }

                    debug_or_trace!(
                        CAT,
                        self.is_main_elem,
                        obj = self.elem,
                        "o lateness {lateness:.2?}",
                    );
                    debug_or_trace!(
                        CAT,
                        self.is_main_elem,
                        obj = self.elem,
                        "o interval {interval:.2?}",
                    );
                }

                self.last_ts = Some(ts);

                log_or_trace!(CAT, self.is_main_elem, obj = self.elem, "Buffer processed");
            }
            StreamItem::Event(evt) => match evt.view() {
                EventView::Eos(_) => {
                    if let Some(ref mut stats) = self.stats {
                        stats.log_global();
                    }
                }
                EventView::Segment(evt) => {
                    self.segment = evt.segment().downcast_ref::<gst::ClockTime>().cloned();
                }
                _ => (),
            },
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct TaskSink {
    sink_pad: PadSink,
    task: Task,
    item_sender: Mutex<Option<flume::Sender<StreamItem>>>,
    settings: Mutex<Settings>,
}

impl TaskSink {
    #[track_caller]
    fn clone_item_sender(&self) -> flume::Sender<StreamItem> {
        self.item_sender.lock().unwrap().as_ref().unwrap().clone()
    }

    fn prepare(&self) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let stats = if settings.logs_stats {
            Some(Box::new(Stats::new(
                settings.push_period + settings.context_wait / 2,
            )))
        } else {
            None
        };

        debug_or_trace!(CAT, settings.is_main_elem, imp = self, "Preparing");

        let ts_ctx = Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
            error_msg!(
                gst::ResourceError::OpenWrite,
                ["Failed to acquire Context: {}", err]
            )
        })?;

        // Enable backpressure for items
        let (item_sender, item_receiver) = flume::bounded(0);
        *self.item_sender.lock().unwrap() = Some(item_sender);

        let is_main_elem = settings.is_main_elem;
        let task_impl = TaskSinkTask::new(&self.obj(), item_receiver, settings.is_main_elem, stats);
        self.task
            .prepare(task_impl, ts_ctx)
            .block_on_or_add_subtask_then(self.obj(), move |elem, res| {
                if res.is_ok() {
                    debug_or_trace!(CAT, is_main_elem, obj = elem, "Prepared");
                }
            })
    }

    fn unprepare(&self) {
        let is_main_elem = self.settings.lock().unwrap().is_main_elem;
        debug_or_trace!(CAT, is_main_elem, imp = self, "Unpreparing");
        let _ = self
            .task
            .unprepare()
            .block_on_or_add_subtask_then(self.obj(), move |elem, _| {
                debug_or_trace!(CAT, is_main_elem, obj = elem, "Unprepared");
            });
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let is_main_elem = self.settings.lock().unwrap().is_main_elem;
        debug_or_trace!(CAT, is_main_elem, imp = self, "Stopping");
        self.task
            .stop()
            .block_on_or_add_subtask_then(self.obj(), move |elem, res| {
                if res.is_ok() {
                    debug_or_trace!(CAT, is_main_elem, obj = elem, "Stopped");
                }
            })
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let is_main_elem = self.settings.lock().unwrap().is_main_elem;
        debug_or_trace!(CAT, is_main_elem, imp = self, "Starting");
        self.task
            .start()
            .block_on_or_add_subtask_then(self.obj(), move |elem, res| {
                if res.is_ok() {
                    debug_or_trace!(CAT, is_main_elem, obj = elem, "Started");
                }
            })
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TaskSink {
    const NAME: &'static str = "TsStandaloneTaskSink";
    type Type = super::TaskSink;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        Self {
            sink_pad: PadSink::new(
                gst::Pad::from_template(&klass.pad_template("sink").unwrap()),
                TaskPadSinkHandler,
            ),
            task: Task::default(),
            item_sender: Default::default(),
            settings: Default::default(),
        }
    }
}

impl ObjectImpl for TaskSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(Settings::properties);
        PROPERTIES.as_ref()
    }

    fn set_property(&self, id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        self.settings.lock().unwrap().set_property(id, value, pspec);
    }

    fn property(&self, id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        self.settings.lock().unwrap().property(id, pspec)
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(self.sink_pad.gst_pad()).unwrap();
        obj.set_element_flags(gst::ElementFlags::SINK);
    }
}

impl GstObjectImpl for TaskSink {}

impl ElementImpl for TaskSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing standalone test task sink",
                "Sink/Test",
                "Thread-sharing standalone test task sink",
                "François Laignel <fengalin@free.fr>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
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
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {transition:?}");

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare().map_err(|err| {
                    self.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::ReadyToPaused => {
                self.start().map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PausedToReady => {
                self.stop().map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare();
            }
            _ => (),
        }

        self.parent_change_state(transition)
    }
}
