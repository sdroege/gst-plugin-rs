// Copyright (C) 2025 François Laignel <francois@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-ts-intersrc
 * @see_also: ts-intersink, ts-proxysink, ts-proxysrc, intersink, intersrc
 *
 * Thread-sharing source for inter-pipelines communication.
 *
 * `ts-intersrc` is an element that proxies events travelling downstream, non-serialized
 * queries and buffers (including metas) from another pipeline that contains a matching
 * `ts-intersink` element. The purpose is to allow one to many decoupled pipelines
 * to function as though they were one without having to manually shuttle buffers,
 * events, queries, etc.
 *
 * This doesn't support dynamically changing `ts-intersink` for now.
 *
 * The `ts-intersink` & `ts-intersrc` elements take advantage of the `threadshare`
 * runtime, reducing the number of threads & context switches which would be
 * necessary with other forms of inter-pipelines elements.
 *
 * ## Usage
 *
 * |[<!-- language="rust" -->
 * use futures::prelude::*;
 * use gst::prelude::*;
 *
 * let g_ctx = gst::glib::MainContext::default();
 * gst::init().unwrap();
 *
 * // An upstream pipeline producing a 1 second Opus encoded audio stream
 * let pipe_up = gst::parse::launch(
 *     "
 *         audiotestsrc is-live=true num-buffers=50 volume=0.02
 *         ! opusenc
 *         ! ts-intersink inter-context=my-inter-ctx
 *     ",
 * )
 * .unwrap()
 * .downcast::<gst::Pipeline>()
 * .unwrap();
 *
 * // A downstream pipeline which will receive the Opus encoded audio stream
 * // and render it locally.
 * let pipe_down = gst::parse::launch(
 *     "
 *         ts-intersrc inter-context=my-inter-ctx context=ts-group-01 context-wait=20
 *         ! opusdec
 *         ! audioconvert
 *         ! audioresample
 *         ! ts-queue context=ts-group-01 context-wait=20 max-size-buffers=1 max-size-bytes=0 max-size-time=0
 *         ! autoaudiosink
 *     ",
 * )
 * .unwrap()
 * .downcast::<gst::Pipeline>()
 * .unwrap();
 *
 * // Both pipelines must agree on the timing information or we'll get glitches
 * // or overruns/underruns. Ideally, we should tell pipe_up to use the same clock
 * // as pipe_down, but since that will be set asynchronously to the audio clock, it
 * // is simpler and likely accurate enough to use the system clock for both
 * // pipelines. If no element in either pipeline will provide a clock, this
 * // is not needed.
 * let clock = gst::SystemClock::obtain();
 * pipe_up.set_clock(Some(&clock)).unwrap();
 * pipe_down.set_clock(Some(&clock)).unwrap();
 *
 * // This is not really needed in this case since the pipelines are created and
 * // started at the same time. However, an application that dynamically
 * // generates pipelines must ensure that all the pipelines that will be
 * // connected together share the same base time.
 * pipe_up.set_base_time(gst::ClockTime::ZERO);
 * pipe_up.set_start_time(gst::ClockTime::NONE);
 * pipe_down.set_base_time(gst::ClockTime::ZERO);
 * pipe_down.set_start_time(gst::ClockTime::NONE);
 *
 * pipe_up.set_state(gst::State::Playing).unwrap();
 * pipe_down.set_state(gst::State::Playing).unwrap();
 *
 * g_ctx.block_on(async {
 *     use gst::MessageView::*;
 *
 *     let mut bus_up_stream = pipe_up.bus().unwrap().stream();
 *     let mut bus_down_stream = pipe_down.bus().unwrap().stream();
 *
 *     loop {
 *         futures::select! {
 *             msg = bus_up_stream.next() => {
 *                 let Some(msg) = msg else { continue };
 *                 match msg.view() {
 *                     Latency(_) => {
 *                         let _ = pipe_down.recalculate_latency();
 *                     }
 *                     Error(err) => {
 *                         eprintln!("Error with downstream pipeline {err:?}");
 *                         break;
 *                     }
 *                     _ => (),
 *                 }
 *             }
 *             msg = bus_down_stream.next() => {
 *                 let Some(msg) = msg else { continue };
 *                 match msg.view() {
 *                     Latency(_) => {
 *                         let _ = pipe_down.recalculate_latency();
 *                     }
 *                     Eos(_) => {
 *                         println!("Got EoS");
 *                         break;
 *                     }
 *                     Error(err) => {
 *                         eprintln!("Error with downstream pipeline {err:?}");
 *                         break;
 *                     }
 *                     _ => (),
 *                 }
 *             }
 *         };
 *     }
 * });
 *
 * pipe_up.set_state(gst::State::Null).unwrap();
 * pipe_down.set_state(gst::State::Null).unwrap();
 * ]|
 *
 * Since: plugins-rs-0.14.0
 */
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::ops::ControlFlow;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::runtime::executor::{block_on, block_on_or_add_sub_task};
use crate::runtime::prelude::*;
use crate::runtime::{Context, PadSrc, Task};

use crate::dataqueue::{DataQueue, DataQueueItem};

use crate::inter::{
    InterContext, InterContextWeak, DEFAULT_CONTEXT, DEFAULT_CONTEXT_WAIT, DEFAULT_INTER_CONTEXT,
    INTER_CONTEXTS,
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ts-intersrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing inter source"),
    )
});

/// Initial capacity for the `ts-intersrc` related `Slab`s in the shared `InterContext`.
// TODO Could had a property for users to allocate large `Slab` and avoid incremental re-allocations.
//      Let's see how it behaves in the wild like this first.
const DEFAULT_INTER_SRC_CAPACITY: u32 = 16;

const DEFAULT_MAX_SIZE_BUFFERS: u32 = 200;
const DEFAULT_MAX_SIZE_BYTES: u32 = 1024 * 1024;
const DEFAULT_MAX_SIZE_TIME: gst::ClockTime = gst::ClockTime::SECOND;

#[derive(Debug, Clone)]
struct Settings {
    max_size_buffers: u32,
    max_size_bytes: u32,
    max_size_time: gst::ClockTime,
    context: String,
    context_wait: Duration,
    inter_context: String,
    inter_src_capacity: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            max_size_buffers: DEFAULT_MAX_SIZE_BUFFERS,
            max_size_bytes: DEFAULT_MAX_SIZE_BYTES,
            max_size_time: DEFAULT_MAX_SIZE_TIME,
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            inter_context: DEFAULT_INTER_CONTEXT.into(),
            inter_src_capacity: DEFAULT_INTER_SRC_CAPACITY as usize,
        }
    }
}

#[derive(Debug)]
struct InterContextSrc {
    shared: InterContext,
    dataqueue_key: usize,
    src_key: usize,
}

impl InterContextSrc {
    async fn add(
        name: String,
        capacity: usize,
        dataqueue: DataQueue,
        src: super::InterSrc,
    ) -> Self {
        let mut inter_ctxs = INTER_CONTEXTS.lock().await;

        let shared = if let Some(shared) = inter_ctxs.get(&name).and_then(InterContextWeak::upgrade)
        {
            shared
        } else {
            let shared = InterContext::new(&name);
            {
                let mut shared = shared.write().await;
                shared.dataqueues.reserve(capacity);
                shared.sources.reserve(capacity);
            }
            inter_ctxs.insert(name, shared.downgrade());

            shared
        };

        let (dataqueue_key, srcpad_key) = {
            let mut shared = shared.write().await;
            (
                shared.dataqueues.insert(dataqueue),
                shared.sources.insert(src),
            )
        };

        InterContextSrc {
            shared,
            dataqueue_key,
            src_key: srcpad_key,
        }
    }
}

impl Drop for InterContextSrc {
    fn drop(&mut self) {
        let shared = self.shared.clone();
        let dataqueue_key = self.dataqueue_key;
        let src_key = self.src_key;

        block_on_or_add_sub_task(async move {
            let mut shared = shared.write().await;
            let _ = shared.dataqueues.remove(dataqueue_key);
            let _ = shared.sources.remove(src_key);
        });
    }
}

#[derive(Clone, Debug)]
struct InterSrcPadHandler;

impl PadSrcHandler for InterSrcPadHandler {
    type ElementImpl = InterSrc;

    fn src_event(self, pad: &gst::Pad, imp: &InterSrc, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling {event:?}");

        use gst::EventView::*;
        match event.view() {
            FlushStart(..) => imp.flush_start().is_ok(),
            FlushStop(..) => imp.flush_stop().is_ok(),
            _ => true,
        }
    }

    fn src_query(self, pad: &gst::Pad, imp: &InterSrc, query: &mut gst::QueryRef) -> bool {
        gst::debug!(CAT, obj = pad, "Handling {query:?}");

        use gst::QueryViewMut;
        let ret = match query.view_mut() {
            QueryViewMut::Latency(q) => {
                let (_, q_min, q_max) = q.result();

                let Some(upstream_latency) = *imp.upstream_latency.lock().unwrap() else {
                    gst::debug!(
                        CAT,
                        obj = pad,
                        "Upstream latency not available yet, can't handle {query:?}"
                    );
                    return false;
                };

                let max_time = imp.settings.lock().unwrap().max_size_time;
                let max = if max_time > gst::ClockTime::ZERO {
                    // TODO also use max-size-buffers & CAPS when applicable
                    Some(q_max.unwrap_or(gst::ClockTime::ZERO) + max_time)
                } else {
                    q_max
                };

                q.set(true, q_min + upstream_latency, max);

                true
            }
            QueryViewMut::Scheduling(q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes([gst::PadMode::Push]);
                true
            }
            QueryViewMut::Caps(q) => {
                let caps = if let Some(ref caps) = pad.current_caps() {
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
            gst::log!(CAT, obj = pad, "Handled {query:?}");
        } else {
            gst::log!(CAT, obj = pad, "Didn't handle {query:?}");
        }

        ret
    }
}

#[derive(Debug)]
struct InterSrcTask {
    elem: super::InterSrc,
    dataqueue: DataQueue,
    got_first_item: bool,
}

impl InterSrcTask {
    fn new(elem: super::InterSrc, dataqueue: DataQueue) -> Self {
        InterSrcTask {
            elem,
            dataqueue,
            got_first_item: false,
        }
    }

    /// Tries to get the upstream latency.
    ///
    /// This is needed when a `ts-intersrc` joins the `inter-context` after
    /// the matching `ts-intersink` has notified the upstream latency.
    async fn maybe_get_upstream_latency(&self) -> Result<(), gst::FlowError> {
        let imp = self.elem.imp();

        if imp.upstream_latency.lock().unwrap().is_some() {
            return Ok(());
        }

        gst::log!(CAT, imp = imp, "Getting upstream latency");

        let shared_ctx = imp.shared_ctx();
        let shared_ctx = shared_ctx.read().await;

        let Some(ref sinkpad) = shared_ctx.sinkpad else {
            gst::info!(
                CAT,
                imp = imp,
                "sinkpad is gone before we could get latency"
            );
            return Err(gst::FlowError::Error);
        };

        let sinkpad_parent = sinkpad.parent().expect("sinkpad should have a parent");
        let intersink = sinkpad_parent
            .downcast_ref::<crate::inter::sink::InterSink>()
            .expect("sinkpad parent should be a ts-intersink");

        if let Some(latency) = intersink.imp().latency() {
            imp.set_upstream_latency_priv(latency);
        } else {
            gst::log!(CAT, imp = imp, "Upstream latency is still unknown");
        }

        Ok(())
    }

    async fn push_item(&mut self, item: DataQueueItem) -> Result<(), gst::FlowError> {
        let imp = self.elem.imp();

        if !self.got_first_item {
            let shared_ctx = imp.shared_ctx();
            let shared_ctx = shared_ctx.read().await;

            let Some(ref sinkpad) = shared_ctx.sinkpad else {
                gst::info!(
                    CAT,
                    imp = imp,
                    "sinkpad is gone before we could handle first item"
                );
                return Err(gst::FlowError::Error);
            };

            let mut evts = Vec::new();
            sinkpad.sticky_events_foreach(|evt| {
                evts.push(evt.copy());
                ControlFlow::Continue(gst::EventForeachAction::Keep)
            });

            for evt in evts {
                if imp.srcpad.push_event(evt.copy()).await {
                    gst::log!(CAT, imp = imp, "Pushed sticky event {evt:?}");
                } else {
                    gst::error!(CAT, imp = imp, "Failed to push sticky event {evt:?}");
                    return Err(gst::FlowError::Error);
                }
            }

            self.got_first_item = true;
        }

        use DataQueueItem::*;
        match item {
            Buffer(buffer) => {
                self.maybe_get_upstream_latency().await?;
                gst::log!(CAT, obj = self.elem, "Forwarding {buffer:?}");
                imp.srcpad.push(buffer).await.map(drop)
            }
            BufferList(list) => {
                self.maybe_get_upstream_latency().await?;
                gst::log!(CAT, obj = self.elem, "Forwarding {list:?}");
                imp.srcpad.push_list(list).await.map(drop)
            }
            Event(event) => {
                gst::log!(CAT, obj = self.elem, "Forwarding {event:?}");

                let is_eos = event.type_() == gst::EventType::Eos;
                imp.srcpad.push_event(event).await;

                if is_eos {
                    return Err(gst::FlowError::Eos);
                }

                Ok(())
            }
        }
    }
}

impl TaskImpl for InterSrcTask {
    type Item = DataQueueItem;

    async fn start(&mut self) -> Result<(), gst::ErrorMessage> {
        gst::log!(CAT, obj = self.elem, "Starting task");
        self.dataqueue.start();
        gst::log!(CAT, obj = self.elem, "Task started");

        Ok(())
    }

    async fn try_next(&mut self) -> Result<DataQueueItem, gst::FlowError> {
        self.dataqueue
            .next()
            .await
            .ok_or_else(|| panic!("DataQueue stopped while Task is Started"))
    }

    async fn handle_item(&mut self, item: DataQueueItem) -> Result<(), gst::FlowError> {
        let res = self.push_item(item).await;
        match res {
            Ok(()) => {
                gst::log!(CAT, obj = self.elem, "Successfully pushed item");
            }
            Err(gst::FlowError::Flushing) => {
                gst::debug!(CAT, obj = self.elem, "Flushing");
            }
            Err(gst::FlowError::Eos) => {
                gst::debug!(CAT, obj = self.elem, "EOS");
            }
            Err(err) => {
                gst::error!(CAT, obj = self.elem, "Got error {err}");
                gst::element_error!(
                    &self.elem,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["streaming stopped, reason {err}"]
                );
            }
        }

        res
    }

    async fn stop(&mut self) -> Result<(), gst::ErrorMessage> {
        gst::log!(CAT, obj = self.elem, "Stopping task");

        self.dataqueue.stop();
        self.dataqueue.clear();
        self.got_first_item = false;

        gst::log!(CAT, obj = self.elem, "Task stopped");

        Ok(())
    }

    async fn flush_start(&mut self) -> Result<(), gst::ErrorMessage> {
        gst::log!(CAT, obj = self.elem, "Starting task flush");

        self.dataqueue.clear();
        self.got_first_item = false;

        gst::log!(CAT, obj = self.elem, "Task flush started");

        Ok(())
    }
}

#[derive(Debug)]
pub struct InterSrc {
    srcpad: PadSrc,
    task: Task,
    src_ctx: Mutex<Option<InterContextSrc>>,
    ts_ctx: Mutex<Option<Context>>,
    dataqueue: Mutex<Option<DataQueue>>,
    upstream_latency: Mutex<Option<gst::ClockTime>>,
    settings: Mutex<Settings>,
}

impl InterSrc {
    fn shared_ctx(&self) -> InterContext {
        let local_ctx = self.src_ctx.lock().unwrap();
        local_ctx.as_ref().expect("set in prepare").shared.clone()
    }

    // Sets the upstream latency without blocking the caller.
    pub fn set_upstream_latency(&self, up_latency: gst::ClockTime) {
        if let Some(ref ts_ctx) = *self.ts_ctx.lock().unwrap() {
            let obj = self.obj().clone();

            gst::log!(CAT, imp = self, "Setting upstream latency async");
            ts_ctx.spawn(async move {
                obj.imp().set_upstream_latency_priv(up_latency);
            });
        } else {
            gst::debug!(CAT, imp = self, "Not ready to handle upstream latency");
        }
    }

    // Sets the upstream latency blocking the caller until it's handled.
    fn set_upstream_latency_priv(&self, up_latency: gst::ClockTime) {
        let new_latency = up_latency
            + gst::ClockTime::from_mseconds(
                self.settings.lock().unwrap().context_wait.as_millis() as u64
            );

        {
            let mut upstream_latency = self.upstream_latency.lock().unwrap();
            if let Some(upstream_latency) = *upstream_latency {
                if upstream_latency == new_latency {
                    return;
                }
            }
            *upstream_latency = Some(new_latency);
        }

        gst::debug!(
            CAT,
            imp = self,
            "Got new upstream latency {up_latency} => will report {new_latency}"
        );

        self.post_message(gst::message::Latency::builder().src(&*self.obj()).build());
    }

    fn prepare(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let ts_ctx = Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to acquire Context: {err}"]
            )
        })?;

        let dataqueue = DataQueue::new(
            &self.obj().clone().upcast(),
            self.srcpad.gst_pad(),
            if settings.max_size_buffers == 0 {
                None
            } else {
                Some(settings.max_size_buffers)
            },
            if settings.max_size_bytes == 0 {
                None
            } else {
                Some(settings.max_size_bytes)
            },
            if settings.max_size_time.is_zero() {
                None
            } else {
                Some(settings.max_size_time)
            },
        );

        let obj = self.obj().clone();
        let (ctx_name, inter_src_capacity) = {
            let settings = self.settings.lock().unwrap();
            (settings.inter_context.clone(), settings.inter_src_capacity)
        };

        block_on(async move {
            let imp = obj.imp();
            let src_ctx =
                InterContextSrc::add(ctx_name, inter_src_capacity, dataqueue.clone(), obj.clone())
                    .await;

            *imp.src_ctx.lock().unwrap() = Some(src_ctx);
            *imp.ts_ctx.lock().unwrap() = Some(ts_ctx.clone());
            *imp.dataqueue.lock().unwrap() = Some(dataqueue.clone());

            if imp
                .task
                .prepare(InterSrcTask::new(obj.clone(), dataqueue), ts_ctx)
                .await
                .is_ok()
            {
                gst::debug!(CAT, imp = imp, "Prepared");
                Ok(())
            } else {
                Err(gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to start Task"]
                ))
            }
        })
    }

    fn unprepare(&self) {
        gst::debug!(CAT, imp = self, "Unpreparing");

        self.task.unprepare().block_on().unwrap();

        *self.dataqueue.lock().unwrap() = None;
        *self.src_ctx.lock().unwrap() = None;
        *self.ts_ctx.lock().unwrap() = None;

        gst::debug!(CAT, imp = self, "Unprepared");
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Stopping");

        self.task.stop().await_maybe_on_context()?;
        *self.upstream_latency.lock().unwrap() = gst::ClockTime::NONE;

        gst::debug!(CAT, imp = self, "Stopped");
        Ok(())
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Starting");
        self.task.start().await_maybe_on_context()?;
        gst::debug!(CAT, imp = self, "Started");
        Ok(())
    }

    fn pause(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Pausing");
        self.task.pause().await_maybe_on_context()?;
        gst::debug!(CAT, imp = self, "Paused");
        Ok(())
    }

    fn flush_start(&self) -> Result<(), gst::FlowError> {
        gst::debug!(CAT, imp = self, "Flushing");

        let res = self.task.flush_start().await_maybe_on_context();
        if let Err(err) = res {
            gst::error!(CAT, imp = self, "FlushStart failed {err:?}");
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ("Internal data stream error"),
                ["FlushStart failed {err:?}"]
            );

            return Err(gst::FlowError::Error);
        }

        Ok(())
    }

    fn flush_stop(&self) -> Result<(), gst::FlowError> {
        gst::debug!(CAT, imp = self, "Stopping flush");

        let res = self.task.flush_stop().await_maybe_on_context();
        if let Err(err) = res {
            gst::error!(CAT, imp = self, "FlushStop failed {err:?}");
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ("Internal data stream error"),
                ["FlushStop failed {err:?}"]
            );

            return Err(gst::FlowError::Error);
        }

        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for InterSrc {
    const NAME: &'static str = "GstTsInterSrc";
    type Type = super::InterSrc;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        Self {
            srcpad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap()),
                InterSrcPadHandler,
            ),
            task: Task::default(),
            src_ctx: Mutex::new(None),
            ts_ctx: Mutex::new(None),
            dataqueue: Mutex::new(None),
            upstream_latency: Mutex::new(gst::ClockTime::NONE),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for InterSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("context")
                    .nick("Context")
                    .blurb("Context name to share threads with")
                    .default_value(Some(DEFAULT_CONTEXT))
                    .readwrite()
                    .construct_only()
                    .build(),
                glib::ParamSpecUInt::builder("context-wait")
                    .nick("Context Wait")
                    .blurb("Throttle poll loop to run at most once every this many ms")
                    .maximum(1000)
                    .default_value(DEFAULT_CONTEXT_WAIT.as_millis() as u32)
                    .readwrite()
                    .construct_only()
                    .build(),
                glib::ParamSpecString::builder("inter-context")
                    .nick("Inter Context")
                    .blurb("Context name of the inter elements to share with")
                    .default_value(Some(DEFAULT_INTER_CONTEXT))
                    .readwrite()
                    .construct_only()
                    .build(),
                glib::ParamSpecUInt::builder("max-size-buffers")
                    .nick("Max Size Buffers")
                    .blurb("Maximum number of buffers to queue (0=unlimited)")
                    .default_value(DEFAULT_MAX_SIZE_BUFFERS)
                    .readwrite()
                    .construct_only()
                    .build(),
                glib::ParamSpecUInt::builder("max-size-bytes")
                    .nick("Max Size Bytes")
                    .blurb("Maximum number of bytes to queue (0=unlimited)")
                    .default_value(DEFAULT_MAX_SIZE_BYTES)
                    .readwrite()
                    .construct_only()
                    .build(),
                glib::ParamSpecUInt64::builder("max-size-time")
                    .nick("Max Size Time")
                    .blurb("Maximum number of nanoseconds to queue (0=unlimited)")
                    .maximum(u64::MAX - 1)
                    .default_value(DEFAULT_MAX_SIZE_TIME.nseconds())
                    .readwrite()
                    .construct_only()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "max-size-buffers" => {
                settings.max_size_buffers = value.get().expect("type checked upstream");
            }
            "max-size-bytes" => {
                settings.max_size_bytes = value.get().expect("type checked upstream");
            }
            "max-size-time" => {
                settings.max_size_time = value.get::<u64>().unwrap().nseconds();
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
            "inter-context" => {
                settings.inter_context = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| DEFAULT_INTER_CONTEXT.into());
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "max-size-buffers" => settings.max_size_buffers.to_value(),
            "max-size-bytes" => settings.max_size_bytes.to_value(),
            "max-size-time" => settings.max_size_time.nseconds().to_value(),
            "context" => settings.context.to_value(),
            "context-wait" => (settings.context_wait.as_millis() as u32).to_value(),
            "inter-context" => settings.inter_context.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(self.srcpad.gst_pad()).unwrap();
        obj.set_element_flags(gst::ElementFlags::SOURCE);
    }
}

impl GstObjectImpl for InterSrc {}

impl ElementImpl for InterSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing inter source",
                "Source/Generic",
                "Thread-sharing inter-pipelines source",
                "François Laignel <francois@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn send_event(&self, event: gst::Event) -> bool {
        gst::log!(CAT, imp = self, "Handling {event:?}");

        if let Some(ref ts_ctx) = *self.ts_ctx.lock().unwrap() {
            gst::log!(CAT, imp = self, "Handling {event:?}");

            let obj = self.obj().clone();
            ts_ctx.spawn(async move {
                let imp = obj.imp();
                if let gst::EventView::FlushStart(_) = event.view() {
                    let _ = obj.imp().flush_start();
                }

                imp.srcpad.gst_pad().push_event(event)
            });

            true
        } else {
            gst::info!(CAT, imp = self, "Not ready to handle {event:?}");
            false
        }
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
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
            gst::StateChange::PlayingToPaused => {
                self.pause().map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare();
            }
            _ => (),
        }

        let mut success = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => {
                self.start().map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToReady => {
                self.stop().map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        Ok(success)
    }
}
