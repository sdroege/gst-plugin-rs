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

use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::prelude::*;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_log, gst_trace};

use once_cell::sync::Lazy;

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex as StdMutex;
use std::sync::MutexGuard as StdMutexGuard;
use std::sync::{Arc, Weak};
use std::time::Duration;
use std::{u32, u64};

use crate::runtime::prelude::*;
use crate::runtime::{
    Context, PadSink, PadSinkRef, PadSinkWeak, PadSrc, PadSrcRef, PadSrcWeak, Task,
};

use crate::dataqueue::{DataQueue, DataQueueItem};

static PROXY_CONTEXTS: Lazy<StdMutex<HashMap<String, Weak<StdMutex<ProxyContextInner>>>>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));
static PROXY_SRC_PADS: Lazy<StdMutex<HashMap<String, PadSrcWeak>>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));
static PROXY_SINK_PADS: Lazy<StdMutex<HashMap<String, PadSinkWeak>>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));

const DEFAULT_PROXY_CONTEXT: &str = "";

const DEFAULT_MAX_SIZE_BUFFERS: u32 = 200;
const DEFAULT_MAX_SIZE_BYTES: u32 = 1024 * 1024;
const DEFAULT_MAX_SIZE_TIME: gst::ClockTime = gst::ClockTime::SECOND;
const DEFAULT_CONTEXT: &str = "";
// FIXME use Duration::ZERO when MSVC >= 1.53.2
const DEFAULT_CONTEXT_WAIT: Duration = Duration::from_nanos(0);

#[derive(Debug, Clone)]
struct SettingsSink {
    proxy_context: String,
}

impl Default for SettingsSink {
    fn default() -> Self {
        SettingsSink {
            proxy_context: DEFAULT_PROXY_CONTEXT.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct SettingsSrc {
    max_size_buffers: u32,
    max_size_bytes: u32,
    max_size_time: gst::ClockTime,
    context: String,
    context_wait: Duration,
    proxy_context: String,
}

impl Default for SettingsSrc {
    fn default() -> Self {
        SettingsSrc {
            max_size_buffers: DEFAULT_MAX_SIZE_BUFFERS,
            max_size_bytes: DEFAULT_MAX_SIZE_BYTES,
            max_size_time: DEFAULT_MAX_SIZE_TIME,
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            proxy_context: DEFAULT_PROXY_CONTEXT.into(),
        }
    }
}

// TODO: Refactor into a Sender and Receiver instead of the have_ booleans

#[derive(Debug, Default)]
struct PendingQueue {
    more_queue_space_sender: Option<oneshot::Sender<()>>,
    scheduled: bool,
    items: VecDeque<DataQueueItem>,
}

impl PendingQueue {
    fn notify_more_queue_space(&mut self) {
        self.more_queue_space_sender.take();
    }
}

#[derive(Debug)]
struct ProxyContextInner {
    name: String,
    dataqueue: Option<DataQueue>,
    last_res: Result<gst::FlowSuccess, gst::FlowError>,
    pending_queue: Option<PendingQueue>,
    have_sink: bool,
    have_src: bool,
}

impl Drop for ProxyContextInner {
    fn drop(&mut self) {
        let mut proxy_ctxs = PROXY_CONTEXTS.lock().unwrap();
        proxy_ctxs.remove(&self.name);
    }
}

#[derive(Debug)]
struct ProxyContext {
    shared: Arc<StdMutex<ProxyContextInner>>,
    as_sink: bool,
    name: String,
}

impl ProxyContext {
    #[inline]
    fn lock_shared(&self) -> StdMutexGuard<'_, ProxyContextInner> {
        self.shared.lock().unwrap()
    }

    fn get(name: &str, as_sink: bool) -> Option<Self> {
        let mut proxy_ctxs = PROXY_CONTEXTS.lock().unwrap();

        let mut proxy_ctx = None;
        if let Some(shared_weak) = proxy_ctxs.get(name) {
            if let Some(shared) = shared_weak.upgrade() {
                {
                    let shared = shared.lock().unwrap();
                    if (shared.have_sink && as_sink) || (shared.have_src && !as_sink) {
                        return None;
                    }
                }

                proxy_ctx = Some({
                    let proxy_ctx = ProxyContext {
                        shared,
                        as_sink,
                        name: name.into(),
                    };
                    {
                        let mut shared = proxy_ctx.lock_shared();
                        if as_sink {
                            shared.have_sink = true;
                        } else {
                            shared.have_src = true;
                        }
                    }

                    proxy_ctx
                });
            }
        }

        if proxy_ctx.is_none() {
            let shared = Arc::new(StdMutex::new(ProxyContextInner {
                name: name.into(),
                dataqueue: None,
                last_res: Err(gst::FlowError::Flushing),
                pending_queue: None,
                have_sink: as_sink,
                have_src: !as_sink,
            }));

            proxy_ctxs.insert(name.into(), Arc::downgrade(&shared));

            proxy_ctx = Some(ProxyContext {
                shared,
                as_sink,
                name: name.into(),
            });
        }

        proxy_ctx
    }
}

impl Drop for ProxyContext {
    fn drop(&mut self) {
        let mut shared_ctx = self.lock_shared();
        if self.as_sink {
            assert!(shared_ctx.have_sink);
            shared_ctx.have_sink = false;
            let _ = shared_ctx.pending_queue.take();
        } else {
            assert!(shared_ctx.have_src);
            shared_ctx.have_src = false;
            let _ = shared_ctx.dataqueue.take();
        }
    }
}

#[derive(Clone, Debug)]
struct ProxySinkPadHandler;

impl PadSinkHandler for ProxySinkPadHandler {
    type ElementImpl = ProxySink;

    fn sink_chain(
        &self,
        pad: &PadSinkRef,
        _proxysink: &ProxySink,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let pad_weak = pad.downgrade();
        let element = element.clone().downcast::<super::ProxySink>().unwrap();

        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            gst_log!(SINK_CAT, obj: pad.gst_pad(), "Handling {:?}", buffer);
            let proxysink = ProxySink::from_instance(&element);
            proxysink
                .enqueue_item(&element, DataQueueItem::Buffer(buffer))
                .await
        }
        .boxed()
    }

    fn sink_chain_list(
        &self,
        pad: &PadSinkRef,
        _proxysink: &ProxySink,
        element: &gst::Element,
        list: gst::BufferList,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let pad_weak = pad.downgrade();
        let element = element.clone().downcast::<super::ProxySink>().unwrap();
        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            gst_log!(SINK_CAT, obj: pad.gst_pad(), "Handling {:?}", list);
            let proxysink = ProxySink::from_instance(&element);
            proxysink
                .enqueue_item(&element, DataQueueItem::BufferList(list))
                .await
        }
        .boxed()
    }

    fn sink_event(
        &self,
        pad: &PadSinkRef,
        proxysink: &ProxySink,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_debug!(SINK_CAT, obj: pad.gst_pad(), "Handling non-serialized {:?}", event);

        let src_pad = {
            let proxy_ctx = proxysink.proxy_ctx.lock().unwrap();

            PROXY_SRC_PADS
                .lock()
                .unwrap()
                .get(&proxy_ctx.as_ref().unwrap().name)
                .and_then(|src_pad| src_pad.upgrade())
                .map(|src_pad| src_pad.gst_pad().clone())
        };

        if let EventView::FlushStart(..) = event.view() {
            proxysink.stop(element.downcast_ref::<super::ProxySink>().unwrap());
        }

        if let Some(src_pad) = src_pad {
            gst_log!(SINK_CAT, obj: pad.gst_pad(), "Forwarding non-serialized {:?}", event);
            src_pad.push_event(event)
        } else {
            gst_error!(SINK_CAT, obj: pad.gst_pad(), "No src pad to forward non-serialized {:?} to", event);
            true
        }
    }

    fn sink_event_serialized(
        &self,
        pad: &PadSinkRef,
        _proxysink: &ProxySink,
        element: &gst::Element,
        event: gst::Event,
    ) -> BoxFuture<'static, bool> {
        use gst::EventView;

        gst_log!(SINK_CAT, obj: pad.gst_pad(), "Handling serialized {:?}", event);

        let pad_weak = pad.downgrade();
        let element = element.clone().downcast::<super::ProxySink>().unwrap();
        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            let proxysink = ProxySink::from_instance(&element);

            match event.view() {
                EventView::Eos(..) => {
                    let _ =
                        element.post_message(gst::message::Eos::builder().src(&element).build());
                }
                EventView::FlushStop(..) => proxysink.start(&element),
                _ => (),
            }

            gst_log!(SINK_CAT, obj: pad.gst_pad(), "Queuing serialized {:?}", event);
            proxysink
                .enqueue_item(&element, DataQueueItem::Event(event))
                .await
                .is_ok()
        }
        .boxed()
    }
}

#[derive(Debug)]
pub struct ProxySink {
    sink_pad: PadSink,
    proxy_ctx: StdMutex<Option<ProxyContext>>,
    settings: StdMutex<SettingsSink>,
}

static SINK_CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-proxysink",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing proxy sink"),
    )
});

impl ProxySink {
    async fn schedule_pending_queue(&self, element: &super::ProxySink) {
        loop {
            let more_queue_space_receiver = {
                let proxy_ctx = self.proxy_ctx.lock().unwrap();
                let mut shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();

                gst_log!(SINK_CAT, obj: element, "Trying to empty pending queue");

                let ProxyContextInner {
                    pending_queue: ref mut pq,
                    ref dataqueue,
                    ..
                } = *shared_ctx;

                if let Some(ref mut pending_queue) = *pq {
                    if let Some(ref dataqueue) = dataqueue {
                        let mut failed_item = None;
                        while let Some(item) = pending_queue.items.pop_front() {
                            if let Err(item) = dataqueue.push(item) {
                                failed_item = Some(item);
                                break;
                            }
                        }

                        if let Some(failed_item) = failed_item {
                            pending_queue.items.push_front(failed_item);
                            let (sender, receiver) = oneshot::channel();
                            pending_queue.more_queue_space_sender = Some(sender);

                            receiver
                        } else {
                            gst_log!(SINK_CAT, obj: element, "Pending queue is empty now");
                            *pq = None;
                            return;
                        }
                    } else {
                        let (sender, receiver) = oneshot::channel();
                        pending_queue.more_queue_space_sender = Some(sender);

                        receiver
                    }
                } else {
                    gst_log!(SINK_CAT, obj: element, "Flushing, dropping pending queue");
                    *pq = None;
                    return;
                }
            };

            gst_log!(SINK_CAT, obj: element, "Waiting for more queue space");
            let _ = more_queue_space_receiver.await;
        }
    }

    async fn enqueue_item(
        &self,
        element: &super::ProxySink,
        item: DataQueueItem,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let wait_fut = {
            let proxy_ctx = self.proxy_ctx.lock().unwrap();
            let mut shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();

            /* We've taken the lock again, make sure not to recreate
             * a pending queue if tearing down */
            shared_ctx.last_res?;

            let item = {
                let ProxyContextInner {
                    ref mut pending_queue,
                    ref dataqueue,
                    ..
                } = *shared_ctx;

                match (pending_queue, dataqueue) {
                    (None, Some(ref dataqueue)) => dataqueue.push(item),
                    (Some(ref mut pending_queue), Some(ref dataqueue)) => {
                        if !pending_queue.scheduled {
                            let mut failed_item = None;
                            while let Some(item) = pending_queue.items.pop_front() {
                                if let Err(item) = dataqueue.push(item) {
                                    failed_item = Some(item);
                                    break;
                                }
                            }

                            if let Some(failed_item) = failed_item {
                                pending_queue.items.push_front(failed_item);

                                Err(item)
                            } else {
                                dataqueue.push(item)
                            }
                        } else {
                            Err(item)
                        }
                    }
                    _ => Err(item),
                }
            };

            if let Err(item) = item {
                if shared_ctx
                    .pending_queue
                    .as_ref()
                    .map(|pending_queue| !pending_queue.scheduled)
                    .unwrap_or(true)
                {
                    if shared_ctx.pending_queue.is_none() {
                        shared_ctx.pending_queue = Some(PendingQueue::default());
                    }

                    let pending_queue = shared_ctx.pending_queue.as_mut().unwrap();

                    let schedule_now = !matches!(
                        item,
                        DataQueueItem::Event(ref ev) if ev.type_() != gst::EventType::Eos,
                    );

                    pending_queue.items.push_back(item);

                    gst_log!(
                        SINK_CAT,
                        obj: element,
                        "Proxy is full - Pushing first item on pending queue"
                    );

                    if schedule_now {
                        gst_log!(SINK_CAT, obj: element, "Scheduling pending queue now");
                        pending_queue.scheduled = true;

                        let wait_fut = self.schedule_pending_queue(element);
                        Some(wait_fut)
                    } else {
                        gst_log!(SINK_CAT, obj: element, "Scheduling pending queue later");

                        None
                    }
                } else {
                    shared_ctx
                        .pending_queue
                        .as_mut()
                        .unwrap()
                        .items
                        .push_back(item);

                    None
                }
            } else {
                None
            }
        };

        if let Some(wait_fut) = wait_fut {
            gst_log!(
                SINK_CAT,
                obj: element,
                "Blocking until queue has space again"
            );
            wait_fut.await;
        }

        let proxy_ctx = self.proxy_ctx.lock().unwrap();
        let shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();
        shared_ctx.last_res
    }

    fn prepare(&self, element: &super::ProxySink) -> Result<(), gst::ErrorMessage> {
        gst_debug!(SINK_CAT, obj: element, "Preparing");

        let proxy_context = self.settings.lock().unwrap().proxy_context.to_string();

        let proxy_ctx = ProxyContext::get(&proxy_context, true).ok_or_else(|| {
            gst::error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to create or get ProxyContext"]
            )
        })?;

        {
            let mut proxy_sink_pads = PROXY_SINK_PADS.lock().unwrap();
            assert!(!proxy_sink_pads.contains_key(&proxy_context));
            proxy_sink_pads.insert(proxy_context, self.sink_pad.downgrade());
        }

        *self.proxy_ctx.lock().unwrap() = Some(proxy_ctx);

        gst_debug!(SINK_CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &super::ProxySink) {
        gst_debug!(SINK_CAT, obj: element, "Unpreparing");
        *self.proxy_ctx.lock().unwrap() = None;
        gst_debug!(SINK_CAT, obj: element, "Unprepared");
    }

    fn start(&self, element: &super::ProxySink) {
        let proxy_ctx = self.proxy_ctx.lock().unwrap();
        let mut shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();

        gst_debug!(SINK_CAT, obj: element, "Starting");

        {
            let settings = self.settings.lock().unwrap();
            let mut proxy_sink_pads = PROXY_SINK_PADS.lock().unwrap();
            proxy_sink_pads.remove(&settings.proxy_context);
        }

        shared_ctx.last_res = Ok(gst::FlowSuccess::Ok);

        gst_debug!(SINK_CAT, obj: element, "Started");
    }

    fn stop(&self, element: &super::ProxySink) {
        let proxy_ctx = self.proxy_ctx.lock().unwrap();
        let mut shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();

        gst_debug!(SINK_CAT, obj: element, "Stopping");

        let _ = shared_ctx.pending_queue.take();
        shared_ctx.last_res = Err(gst::FlowError::Flushing);

        gst_debug!(SINK_CAT, obj: element, "Stopped");
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ProxySink {
    const NAME: &'static str = "RsTsProxySink";
    type Type = super::ProxySink;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        Self {
            sink_pad: PadSink::new(
                gst::Pad::from_template(&klass.pad_template("sink").unwrap(), Some("sink")),
                ProxySinkPadHandler,
            ),
            proxy_ctx: StdMutex::new(None),
            settings: StdMutex::new(SettingsSink::default()),
        }
    }
}

impl ObjectImpl for ProxySink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpec::new_string(
                "proxy-context",
                "Proxy Context",
                "Context name of the proxy to share with",
                Some(DEFAULT_PROXY_CONTEXT),
                glib::ParamFlags::READWRITE,
            )]
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
            "proxy-context" => {
                settings.proxy_context = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "proxy-context" => settings.proxy_context.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(self.sink_pad.gst_pad()).unwrap();

        crate::set_element_flags(obj, gst::ElementFlags::SINK);
    }
}

impl ElementImpl for ProxySink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing proxy sink",
                "Sink/Generic",
                "Thread-sharing proxy sink",
                "Sebastian Dröge <sebastian@centricular.com>",
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
        gst_trace!(SINK_CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare(element).map_err(|err| {
                    element.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PausedToReady => {
                self.stop(element);
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element);
            }
            _ => (),
        }

        let success = self.parent_change_state(element, transition)?;

        if transition == gst::StateChange::ReadyToPaused {
            self.start(element);
        }

        Ok(success)
    }
}

#[derive(Clone, Debug)]
struct ProxySrcPadHandler;

impl ProxySrcPadHandler {
    async fn push_item(
        pad: &PadSrcRef<'_>,
        proxysrc: &ProxySrc,
        item: DataQueueItem,
    ) -> Result<(), gst::FlowError> {
        {
            let proxy_ctx = proxysrc.proxy_ctx.lock().unwrap();
            let mut shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();
            if let Some(pending_queue) = shared_ctx.pending_queue.as_mut() {
                pending_queue.notify_more_queue_space();
            }
        }

        match item {
            DataQueueItem::Buffer(buffer) => {
                gst_log!(SRC_CAT, obj: pad.gst_pad(), "Forwarding {:?}", buffer);
                pad.push(buffer).await.map(drop)
            }
            DataQueueItem::BufferList(list) => {
                gst_log!(SRC_CAT, obj: pad.gst_pad(), "Forwarding {:?}", list);
                pad.push_list(list).await.map(drop)
            }
            DataQueueItem::Event(event) => {
                gst_log!(SRC_CAT, obj: pad.gst_pad(), "Forwarding {:?}", event);
                pad.push_event(event).await;
                Ok(())
            }
        }
    }
}

impl PadSrcHandler for ProxySrcPadHandler {
    type ElementImpl = ProxySrc;

    fn src_event(
        &self,
        pad: &PadSrcRef,
        proxysrc: &ProxySrc,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        let sink_pad = {
            let proxy_ctx = proxysrc.proxy_ctx.lock().unwrap();

            PROXY_SINK_PADS
                .lock()
                .unwrap()
                .get(&proxy_ctx.as_ref().unwrap().name)
                .and_then(|sink_pad| sink_pad.upgrade())
                .map(|sink_pad| sink_pad.gst_pad().clone())
        };

        match event.view() {
            EventView::FlushStart(..) => {
                if let Err(err) = proxysrc.task.flush_start() {
                    gst_error!(SRC_CAT, obj: pad.gst_pad(), "FlushStart failed {:?}", err);
                    gst::element_error!(
                        element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["FlushStart failed {:?}", err]
                    );
                    return false;
                }
            }
            EventView::FlushStop(..) => {
                if let Err(err) = proxysrc.task.flush_stop() {
                    gst_error!(SRC_CAT, obj: pad.gst_pad(), "FlushStop failed {:?}", err);
                    gst::element_error!(
                        element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["FlushStop failed {:?}", err]
                    );
                    return false;
                }
            }
            _ => (),
        }

        if let Some(sink_pad) = sink_pad {
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Forwarding {:?}", event);
            sink_pad.push_event(event)
        } else {
            gst_error!(SRC_CAT, obj: pad.gst_pad(), "No sink pad to forward {:?} to", event);
            false
        }
    }

    fn src_query(
        &self,
        pad: &PadSrcRef,
        _proxysrc: &ProxySrc,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handling {:?}", query);
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
                let caps = if let Some(ref caps) = pad.gst_pad().current_caps() {
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
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handled {:?}", query);
        } else {
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Didn't handle {:?}", query);
        }

        ret
    }
}

#[derive(Debug)]
struct ProxySrcTask {
    element: super::ProxySrc,
    src_pad: PadSrcWeak,
    dataqueue: DataQueue,
}

impl ProxySrcTask {
    fn new(element: &super::ProxySrc, src_pad: &PadSrc, dataqueue: DataQueue) -> Self {
        ProxySrcTask {
            element: element.clone(),
            src_pad: src_pad.downgrade(),
            dataqueue,
        }
    }
}

impl TaskImpl for ProxySrcTask {
    fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(SRC_CAT, obj: &self.element, "Starting task");

            let proxysrc = ProxySrc::from_instance(&self.element);
            let proxy_ctx = proxysrc.proxy_ctx.lock().unwrap();
            let mut shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();

            shared_ctx.last_res = Ok(gst::FlowSuccess::Ok);

            if let Some(pending_queue) = shared_ctx.pending_queue.as_mut() {
                pending_queue.notify_more_queue_space();
            }

            self.dataqueue.start();

            gst_log!(SRC_CAT, obj: &self.element, "Task started");
            Ok(())
        }
        .boxed()
    }

    fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            let item = self.dataqueue.next().await;

            let item = match item {
                Some(item) => item,
                None => {
                    gst_log!(SRC_CAT, obj: &self.element, "DataQueue Stopped");
                    return Err(gst::FlowError::Flushing);
                }
            };

            let pad = self.src_pad.upgrade().expect("PadSrc no longer exists");
            let proxysrc = ProxySrc::from_instance(&self.element);
            let res = ProxySrcPadHandler::push_item(&pad, &proxysrc, item).await;
            match res {
                Ok(()) => {
                    gst_log!(SRC_CAT, obj: &self.element, "Successfully pushed item");
                    let proxy_ctx = proxysrc.proxy_ctx.lock().unwrap();
                    let mut shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();
                    shared_ctx.last_res = Ok(gst::FlowSuccess::Ok);
                }
                Err(gst::FlowError::Flushing) => {
                    gst_debug!(SRC_CAT, obj: &self.element, "Flushing");
                    let proxy_ctx = proxysrc.proxy_ctx.lock().unwrap();
                    let mut shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();
                    shared_ctx.last_res = Err(gst::FlowError::Flushing);
                }
                Err(gst::FlowError::Eos) => {
                    gst_debug!(SRC_CAT, obj: &self.element, "EOS");
                    let proxy_ctx = proxysrc.proxy_ctx.lock().unwrap();
                    let mut shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();
                    shared_ctx.last_res = Err(gst::FlowError::Eos);
                }
                Err(err) => {
                    gst_error!(SRC_CAT, obj: &self.element, "Got error {}", err);
                    gst::element_error!(
                        &self.element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["streaming stopped, reason {}", err]
                    );
                    let proxy_ctx = proxysrc.proxy_ctx.lock().unwrap();
                    let mut shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();
                    shared_ctx.last_res = Err(err);
                }
            }

            res
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(SRC_CAT, obj: &self.element, "Stopping task");

            let proxysrc = ProxySrc::from_instance(&self.element);
            let proxy_ctx = proxysrc.proxy_ctx.lock().unwrap();
            let mut shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();

            self.dataqueue.clear();
            self.dataqueue.stop();

            shared_ctx.last_res = Err(gst::FlowError::Flushing);

            if let Some(mut pending_queue) = shared_ctx.pending_queue.take() {
                pending_queue.notify_more_queue_space();
            }

            gst_log!(SRC_CAT, obj: &self.element, "Task stopped");
            Ok(())
        }
        .boxed()
    }

    fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(SRC_CAT, obj: &self.element, "Starting task flush");

            let proxysrc = ProxySrc::from_instance(&self.element);
            let proxy_ctx = proxysrc.proxy_ctx.lock().unwrap();
            let mut shared_ctx = proxy_ctx.as_ref().unwrap().lock_shared();

            self.dataqueue.clear();

            shared_ctx.last_res = Err(gst::FlowError::Flushing);

            gst_log!(SRC_CAT, obj: &self.element, "Task flush started");
            Ok(())
        }
        .boxed()
    }
}

#[derive(Debug)]
pub struct ProxySrc {
    src_pad: PadSrc,
    task: Task,
    proxy_ctx: StdMutex<Option<ProxyContext>>,
    dataqueue: StdMutex<Option<DataQueue>>,
    settings: StdMutex<SettingsSrc>,
}

static SRC_CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-proxysrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing proxy source"),
    )
});

impl ProxySrc {
    fn prepare(&self, element: &super::ProxySrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(SRC_CAT, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let proxy_ctx = ProxyContext::get(&settings.proxy_context, false).ok_or_else(|| {
            gst::error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to create get shared_state"]
            )
        })?;

        let ts_ctx = Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to acquire Context: {}", err]
            )
        })?;

        let dataqueue = DataQueue::new(
            &element.clone().upcast(),
            self.src_pad.gst_pad(),
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

        {
            let mut shared_ctx = proxy_ctx.lock_shared();
            shared_ctx.dataqueue = Some(dataqueue.clone());

            let mut proxy_src_pads = PROXY_SRC_PADS.lock().unwrap();
            assert!(!proxy_src_pads.contains_key(&settings.proxy_context));
            proxy_src_pads.insert(settings.proxy_context, self.src_pad.downgrade());
        }

        *self.proxy_ctx.lock().unwrap() = Some(proxy_ctx);

        *self.dataqueue.lock().unwrap() = Some(dataqueue.clone());

        self.task
            .prepare(ProxySrcTask::new(element, &self.src_pad, dataqueue), ts_ctx)
            .map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing Task: {:?}", err]
                )
            })?;

        gst_debug!(SRC_CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &super::ProxySrc) {
        gst_debug!(SRC_CAT, obj: element, "Unpreparing");

        {
            let settings = self.settings.lock().unwrap();
            let mut proxy_src_pads = PROXY_SRC_PADS.lock().unwrap();
            proxy_src_pads.remove(&settings.proxy_context);
        }

        self.task.unprepare().unwrap();

        *self.dataqueue.lock().unwrap() = None;
        *self.proxy_ctx.lock().unwrap() = None;

        gst_debug!(SRC_CAT, obj: element, "Unprepared");
    }

    fn stop(&self, element: &super::ProxySrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(SRC_CAT, obj: element, "Stopping");
        self.task.stop()?;
        gst_debug!(SRC_CAT, obj: element, "Stopped");
        Ok(())
    }

    fn start(&self, element: &super::ProxySrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(SRC_CAT, obj: element, "Starting");
        self.task.start()?;
        gst_debug!(SRC_CAT, obj: element, "Started");
        Ok(())
    }

    fn pause(&self, element: &super::ProxySrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(SRC_CAT, obj: element, "Pausing");
        self.task.pause()?;
        gst_debug!(SRC_CAT, obj: element, "Paused");
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ProxySrc {
    const NAME: &'static str = "RsTsProxySrc";
    type Type = super::ProxySrc;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        Self {
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap(), Some("src")),
                ProxySrcPadHandler,
            ),
            task: Task::default(),
            proxy_ctx: StdMutex::new(None),
            dataqueue: StdMutex::new(None),
            settings: StdMutex::new(SettingsSrc::default()),
        }
    }
}

impl ObjectImpl for ProxySrc {
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
                glib::ParamSpec::new_string(
                    "proxy-context",
                    "Proxy Context",
                    "Context name of the proxy to share with",
                    Some(DEFAULT_PROXY_CONTEXT),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "max-size-buffers",
                    "Max Size Buffers",
                    "Maximum number of buffers to queue (0=unlimited)",
                    0,
                    u32::MAX,
                    DEFAULT_MAX_SIZE_BUFFERS,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "max-size-bytes",
                    "Max Size Bytes",
                    "Maximum number of bytes to queue (0=unlimited)",
                    0,
                    u32::MAX,
                    DEFAULT_MAX_SIZE_BYTES,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint64(
                    "max-size-time",
                    "Max Size Time",
                    "Maximum number of nanoseconds to queue (0=unlimited)",
                    0,
                    u64::MAX - 1,
                    DEFAULT_MAX_SIZE_TIME.nseconds(),
                    glib::ParamFlags::READWRITE,
                ),
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
            "max-size-buffers" => {
                settings.max_size_buffers = value.get().expect("type checked upstream");
            }
            "max-size-bytes" => {
                settings.max_size_bytes = value.get().expect("type checked upstream");
            }
            "max-size-time" => {
                settings.max_size_time =
                    gst::ClockTime::from_nseconds(value.get().expect("type checked upstream"));
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
            "proxy-context" => {
                settings.proxy_context = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "max-size-buffers" => settings.max_size_buffers.to_value(),
            "max-size-bytes" => settings.max_size_bytes.to_value(),
            "max-size-time" => settings.max_size_time.nseconds().to_value(),
            "context" => settings.context.to_value(),
            "context-wait" => (settings.context_wait.as_millis() as u32).to_value(),
            "proxy-context" => settings.proxy_context.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(self.src_pad.gst_pad()).unwrap();

        crate::set_element_flags(obj, gst::ElementFlags::SOURCE);
    }
}

impl ElementImpl for ProxySrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing proxy source",
                "Source/Generic",
                "Thread-sharing proxy source",
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
        gst_trace!(SRC_CAT, obj: element, "Changing state {:?}", transition);

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
