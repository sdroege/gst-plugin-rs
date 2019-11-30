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

use futures::channel::oneshot;
use futures::executor::block_on;
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

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};
use std::{u32, u64};

use crate::runtime::prelude::*;
use crate::runtime::{Context, PadSink, PadSinkRef, PadSrc, PadSrcRef};

use super::dataqueue::{DataQueue, DataQueueItem};

lazy_static! {
    static ref CONTEXTS: Mutex<HashMap<String, Weak<Mutex<SharedQueueInner>>>> =
        Mutex::new(HashMap::new());
}

const DEFAULT_PROXY_CONTEXT: &str = "";

const DEFAULT_MAX_SIZE_BUFFERS: u32 = 200;
const DEFAULT_MAX_SIZE_BYTES: u32 = 1024 * 1024;
const DEFAULT_MAX_SIZE_TIME: u64 = gst::SECOND_VAL;
const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;

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
    max_size_time: u64,
    context: String,
    context_wait: u32,
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

static PROPERTIES_SRC: [subclass::Property; 6] = [
    subclass::Property("max-size-buffers", |name| {
        glib::ParamSpec::uint(
            name,
            "Max Size Buffers",
            "Maximum number of buffers to queue (0=unlimited)",
            0,
            u32::MAX,
            DEFAULT_MAX_SIZE_BUFFERS,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("max-size-bytes", |name| {
        glib::ParamSpec::uint(
            name,
            "Max Size Bytes",
            "Maximum number of bytes to queue (0=unlimited)",
            0,
            u32::MAX,
            DEFAULT_MAX_SIZE_BYTES,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("max-size-time", |name| {
        glib::ParamSpec::uint64(
            name,
            "Max Size Time",
            "Maximum number of nanoseconds to queue (0=unlimited)",
            0,
            u64::MAX - 1,
            DEFAULT_MAX_SIZE_TIME,
            glib::ParamFlags::READWRITE,
        )
    }),
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
    subclass::Property("proxy-context", |name| {
        glib::ParamSpec::string(
            name,
            "Proxy Context",
            "Context name of the proxy to share with",
            Some(DEFAULT_PROXY_CONTEXT),
            glib::ParamFlags::READWRITE,
        )
    }),
];

static PROPERTIES_SINK: [subclass::Property; 1] = [subclass::Property("proxy-context", |name| {
    glib::ParamSpec::string(
        name,
        "Proxy Context",
        "Context name of the proxy to share with",
        Some(DEFAULT_PROXY_CONTEXT),
        glib::ParamFlags::READWRITE,
    )
})];

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
struct SharedQueueInner {
    name: String,
    dataqueue: Option<DataQueue>,
    last_res: Result<gst::FlowSuccess, gst::FlowError>,
    pending_queue: Option<PendingQueue>,
    have_sink: bool,
    have_src: bool,
}

impl SharedQueueInner {
    async fn unprepare(&self) {
        let mut contexts = CONTEXTS.lock().await;
        contexts.remove(&self.name);
    }
}

impl Drop for SharedQueueInner {
    fn drop(&mut self) {
        // Check invariants which can't be held automatically in `SharedQueue`
        // because `drop` can't be `async`
        if self.pending_queue.is_some() || self.dataqueue.is_some() {
            panic!("Missing call to `SharedQueue::unprepare`");
        }
    }
}

#[derive(Debug)]
struct SharedQueue {
    context: Arc<Mutex<SharedQueueInner>>,
    as_sink: bool,
}

impl SharedQueue {
    #[inline]
    async fn lock(&self) -> MutexGuard<'_, SharedQueueInner> {
        self.context.lock().await
    }

    async fn get(name: &str, as_sink: bool) -> Option<Self> {
        let mut contexts = CONTEXTS.lock().await;

        let mut queue = None;
        if let Some(context) = contexts.get(name) {
            if let Some(context) = context.upgrade() {
                {
                    let inner = context.lock().await;
                    if (inner.have_sink && as_sink) || (inner.have_src && !as_sink) {
                        return None;
                    }
                }

                let share_queue = SharedQueue { context, as_sink };
                {
                    let mut inner = share_queue.context.lock().await;
                    if as_sink {
                        inner.have_sink = true;
                    } else {
                        inner.have_src = true;
                    }
                }

                queue = Some(share_queue);
            }
        }

        if queue.is_none() {
            let context = Arc::new(Mutex::new(SharedQueueInner {
                name: name.into(),
                dataqueue: None,
                last_res: Err(gst::FlowError::Flushing),
                pending_queue: None,
                have_sink: as_sink,
                have_src: !as_sink,
            }));

            contexts.insert(name.into(), Arc::downgrade(&context));

            queue = Some(SharedQueue { context, as_sink });
        }

        queue
    }

    async fn unprepare(&self) {
        let mut inner = self.context.lock().await;
        if self.as_sink {
            assert!(inner.have_sink);
            inner.have_sink = false;
            let _ = inner.pending_queue.take();
        } else {
            assert!(inner.have_src);
            inner.have_src = false;
            let _ = inner.dataqueue.take();
        }
        inner.unprepare().await;
    }
}

#[derive(Clone, Debug)]
struct ProxySinkPadHandler;

impl PadSinkHandler for ProxySinkPadHandler {
    type ElementImpl = ProxySink;

    fn sink_chain(
        &self,
        pad: PadSinkRef,
        _proxysink: &ProxySink,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let pad_weak = pad.downgrade();
        let element = element.clone();

        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            gst_log!(SINK_CAT, obj: pad.gst_pad(), "Handling buffer {:?}", buffer);
            let proxysink = ProxySink::from_instance(&element);
            proxysink
                .enqueue_item(&element, DataQueueItem::Buffer(buffer))
                .await
        }
        .boxed()
    }

    fn sink_chain_list(
        &self,
        pad: PadSinkRef,
        _proxysink: &ProxySink,
        element: &gst::Element,
        list: gst::BufferList,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let pad_weak = pad.downgrade();
        let element = element.clone();
        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            gst_log!(SINK_CAT, obj: pad.gst_pad(), "Handling buffer list {:?}", list);
            let proxysink = ProxySink::from_instance(&element);
            proxysink
                .enqueue_item(&element, DataQueueItem::BufferList(list))
                .await
        }
        .boxed()
    }

    fn sink_event(
        &self,
        pad: PadSinkRef,
        proxysink: &ProxySink,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        if event.is_serialized() {
            let pad_weak = pad.downgrade();
            let element = element.clone();
            Either::Right(
                async move {
                    let pad = pad_weak.upgrade().expect("PadSink no longer exists");
                    gst_log!(SINK_CAT, obj: pad.gst_pad(), "Handling event {:?}", event);

                    let proxysink = ProxySink::from_instance(&element);
                    if let EventView::Eos(..) = event.view() {
                        let _ = element
                            .post_message(&gst::Message::new_eos().src(Some(&element)).build());
                    }

                    gst_log!(SINK_CAT, obj: pad.gst_pad(), "Queuing event {:?}", event);
                    let _ = proxysink
                        .enqueue_item(&element, DataQueueItem::Event(event))
                        .await;

                    true
                }
                .boxed(),
            )
        } else {
            match event.view() {
                EventView::FlushStart(..) => {
                    let _ = block_on(proxysink.stop(element));
                }
                EventView::FlushStop(..) => {
                    let (res, state, pending) = element.get_state(0.into());
                    if res == Ok(gst::StateChangeSuccess::Success) && state == gst::State::Paused
                        || res == Ok(gst::StateChangeSuccess::Async)
                            && pending == gst::State::Paused
                    {
                        let _ = block_on(proxysink.start(&element));
                    }
                }
                _ => (),
            }

            gst_debug!(SINK_CAT, obj: pad.gst_pad(), "Fowarding non-serialized event {:?}", event);
            // FIXME proxysink can't forward directly to the src_pad of the proxysrc
            let _ = block_on(proxysink.enqueue_item(&element, DataQueueItem::Event(event)));

            Either::Left(true)
        }
    }
}

#[derive(Debug)]
struct StateSink {
    queue: Option<SharedQueue>,
}

impl StateSink {
    #[inline]
    async fn lock_queue(&self) -> MutexGuard<'_, SharedQueueInner> {
        self.queue.as_ref().unwrap().lock().await
    }
}

impl Default for StateSink {
    fn default() -> StateSink {
        StateSink { queue: None }
    }
}

#[derive(Debug)]
struct ProxySink {
    sink_pad: PadSink,
    state: Mutex<StateSink>,
    settings: Mutex<SettingsSink>,
}

lazy_static! {
    static ref SINK_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-proxysink",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing proxy sink"),
    );
}

impl ProxySink {
    fn schedule_pending_queue(
        &self,
        element: &gst::Element,
        pending_queue: &mut PendingQueue,
    ) -> impl Future<Output = ()> {
        gst_log!(SINK_CAT, obj: element, "Scheduling pending queue now");

        pending_queue.scheduled = true;

        let element = element.clone();
        async move {
            let sink = Self::from_instance(&element);

            loop {
                let more_queue_space_receiver = {
                    let state = sink.state.lock().await;

                    let queue_opt = state.queue.as_ref();
                    if queue_opt.is_none() {
                        return;
                    }
                    let mut queue = queue_opt.unwrap().context.lock().await;

                    gst_log!(SINK_CAT, obj: &element, "Trying to empty pending queue");

                    let SharedQueueInner {
                        pending_queue: ref mut pq,
                        ref dataqueue,
                        ..
                    } = *queue;

                    if let Some(ref mut pending_queue) = *pq {
                        if let Some(ref dataqueue) = dataqueue {
                            let mut failed_item = None;
                            while let Some(item) = pending_queue.items.pop_front() {
                                if let Err(item) = dataqueue.push(item).await {
                                    failed_item = Some(item);
                                }
                            }

                            if let Some(failed_item) = failed_item {
                                pending_queue.items.push_front(failed_item);
                                let (sender, receiver) = oneshot::channel();
                                pending_queue.more_queue_space_sender = Some(sender);

                                receiver
                            } else {
                                gst_log!(SINK_CAT, obj: &element, "Pending queue is empty now");
                                *pq = None;
                                return;
                            }
                        } else {
                            let (sender, receiver) = oneshot::channel();
                            pending_queue.more_queue_space_sender = Some(sender);

                            receiver
                        }
                    } else {
                        gst_log!(SINK_CAT, obj: &element, "Flushing, dropping pending queue");
                        *pq = None;
                        return;
                    }
                };

                gst_log!(SINK_CAT, obj: &element, "Waiting for more queue space");
                let _ = more_queue_space_receiver.await;
            }
        }
    }

    async fn enqueue_item(
        &self,
        element: &gst::Element,
        item: DataQueueItem,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let wait_fut = {
            let state = self.state.lock().await;

            let queue = state.queue.as_ref().ok_or(gst::FlowError::Error)?;
            let mut queue = queue.lock().await;

            let item = {
                let SharedQueueInner {
                    ref mut pending_queue,
                    ref dataqueue,
                    ..
                } = *queue;

                match (pending_queue, dataqueue) {
                    (None, Some(ref dataqueue)) => dataqueue.push(item).await,
                    (Some(ref mut pending_queue), Some(ref dataqueue)) => {
                        if !pending_queue.scheduled {
                            let mut failed_item = None;
                            while let Some(item) = pending_queue.items.pop_front() {
                                if let Err(item) = dataqueue.push(item).await {
                                    failed_item = Some(item);
                                }
                            }

                            if let Some(failed_item) = failed_item {
                                pending_queue.items.push_front(failed_item);

                                Err(item)
                            } else {
                                dataqueue.push(item).await
                            }
                        } else {
                            Err(item)
                        }
                    }
                    _ => Err(item),
                }
            };

            if let Err(item) = item {
                if queue
                    .pending_queue
                    .as_ref()
                    .map(|pending_queue| !pending_queue.scheduled)
                    .unwrap_or(true)
                {
                    if queue.pending_queue.is_none() {
                        queue.pending_queue = Some(PendingQueue::default());
                    }

                    let pending_queue = queue.pending_queue.as_mut().unwrap();

                    let schedule_now = match item {
                        DataQueueItem::Event(ref ev) if ev.get_type() != gst::EventType::Eos => {
                            false
                        }
                        _ => true,
                    };

                    pending_queue.items.push_back(item);

                    gst_log!(
                        SINK_CAT,
                        obj: element,
                        "Proxy is full - Pushing first item on pending queue"
                    );

                    if schedule_now {
                        let wait_fut = self.schedule_pending_queue(element, pending_queue);
                        Some(wait_fut)
                    } else {
                        gst_log!(SINK_CAT, obj: element, "Scheduling pending queue later");

                        None
                    }
                } else {
                    queue.pending_queue.as_mut().unwrap().items.push_back(item);

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

        let state = self.state.lock().await;
        let queue = state.queue.as_ref().unwrap().lock().await;
        queue.last_res
    }

    async fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().await;
        gst_debug!(SINK_CAT, obj: element, "Preparing");

        let settings = self.settings.lock().await;
        state.queue = match SharedQueue::get(&settings.proxy_context, true).await {
            Some(queue) => Some(queue),
            None => {
                return Err(gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to create or get queue"]
                ));
            }
        };

        self.sink_pad.prepare(&ProxySinkPadHandler {}).await;

        gst_debug!(SINK_CAT, obj: element, "Prepared");

        Ok(())
    }

    async fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().await;
        gst_debug!(SINK_CAT, obj: element, "Unpreparing");

        state.queue.as_ref().unwrap().unprepare().await;
        *state = StateSink::default();

        self.sink_pad.unprepare().await;

        gst_debug!(SINK_CAT, obj: element, "Unprepared");
        Ok(())
    }

    async fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let state = self.state.lock().await;
        gst_debug!(SINK_CAT, obj: element, "Starting");

        let mut queue = state.lock_queue().await;
        queue.last_res = Ok(gst::FlowSuccess::Ok);

        gst_debug!(SINK_CAT, obj: element, "Started");

        Ok(())
    }

    async fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        let state = self.state.lock().await;
        gst_debug!(SINK_CAT, obj: element, "Stopping");

        let mut queue = state.lock_queue().await;
        let _ = queue.pending_queue.take();
        queue.last_res = Err(gst::FlowError::Flushing);

        gst_debug!(SINK_CAT, obj: element, "Stopped");

        Ok(())
    }
}

impl ObjectSubclass for ProxySink {
    const NAME: &'static str = "RsTsProxySink";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing proxy sink",
            "Sink/Generic",
            "Thread-sharing proxy sink",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_any();

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        klass.install_properties(&PROPERTIES_SINK);
    }

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sink_pad = PadSink::new_from_template(&templ, Some("sink"));

        Self {
            sink_pad,
            state: Mutex::new(StateSink::default()),
            settings: Mutex::new(SettingsSink::default()),
        }
    }
}

impl ObjectImpl for ProxySink {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES_SINK[id];

        match *prop {
            subclass::Property("proxy-context", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.proxy_context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES_SINK[id];

        match *prop {
            subclass::Property("proxy-context", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.proxy_context.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(self.sink_pad.gst_pad()).unwrap();

        super::set_element_flags(element, gst::ElementFlags::SINK);
    }
}

impl ElementImpl for ProxySink {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(SINK_CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                block_on(self.prepare(element)).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PausedToReady => {
                block_on(self.stop(element)).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                block_on(self.unprepare(element)).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let success = self.parent_change_state(element, transition)?;

        if transition == gst::StateChange::ReadyToPaused {
            block_on(self.start(element)).map_err(|_| gst::StateChangeError)?;
        }

        Ok(success)
    }
}

#[derive(Clone, Debug)]
struct ProxySrcPadHandler;

impl ProxySrcPadHandler {
    async fn start_task(pad: PadSrcRef<'_>, element: &gst::Element, dataqueue: DataQueue) {
        let pad_weak = pad.downgrade();
        let element = element.clone();
        let dataqueue = dataqueue.clone();
        pad.start_task(move || {
            let pad_weak = pad_weak.clone();
            let element = element.clone();
            let mut dataqueue = dataqueue.clone();
            async move {
                let item = dataqueue.next().await;

                let pad = pad_weak.upgrade().expect("PadSrc no longer exists");
                let item = match item {
                    Some(item) => item,
                    None => {
                        gst_log!(SRC_CAT, obj: pad.gst_pad(), "DataQueue Stopped");
                        pad.pause_task().await;
                        return;
                    }
                };

                Self::push_item(pad, &element, item).await;
            }
        })
        .await;
    }

    async fn push_item(pad: PadSrcRef<'_>, element: &gst::Element, item: DataQueueItem) {
        let proxysrc = ProxySrc::from_instance(element);

        {
            let state = proxysrc.state.lock().await;
            let mut queue = state.queue.as_ref().unwrap().lock().await;
            if let Some(ref mut pending_queue) = queue.pending_queue {
                pending_queue.notify_more_queue_space();
            }
        }

        let res = match item {
            DataQueueItem::Buffer(buffer) => {
                gst_log!(SRC_CAT, obj: pad.gst_pad(), "Forwarding buffer {:?}", buffer);
                pad.push(buffer).await.map(drop)
            }
            DataQueueItem::BufferList(list) => {
                gst_log!(SRC_CAT, obj: pad.gst_pad(), "Forwarding buffer list {:?}", list);
                pad.push_list(list).await.map(drop)
            }
            DataQueueItem::Event(event) => {
                gst_log!(SRC_CAT, obj: pad.gst_pad(), "Forwarding event {:?}", event);
                pad.push_event(event).await;
                Ok(())
            }
        };

        match res {
            Ok(_) => {
                gst_log!(SRC_CAT, obj: pad.gst_pad(), "Successfully pushed item");
                let state = proxysrc.state.lock().await;
                let mut queue = state.queue.as_ref().unwrap().lock().await;
                queue.last_res = Ok(gst::FlowSuccess::Ok);
            }
            Err(gst::FlowError::Flushing) => {
                gst_debug!(SRC_CAT, obj: pad.gst_pad(), "Flushing");
                let state = proxysrc.state.lock().await;
                pad.pause_task().await;
                let mut queue = state.queue.as_ref().unwrap().lock().await;
                queue.last_res = Err(gst::FlowError::Flushing);
            }
            Err(gst::FlowError::Eos) => {
                gst_debug!(SRC_CAT, obj: pad.gst_pad(), "EOS");
                let state = proxysrc.state.lock().await;
                pad.pause_task().await;
                let mut queue = state.queue.as_ref().unwrap().lock().await;
                queue.last_res = Err(gst::FlowError::Eos);
            }
            Err(err) => {
                gst_error!(SRC_CAT, obj: pad.gst_pad(), "Got error {}", err);
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["streaming stopped, reason {}", err]
                );
                let state = proxysrc.state.lock().await;
                let mut queue = state.queue.as_ref().unwrap().lock().await;
                queue.last_res = Err(err);
            }
        }
    }
}

impl PadSrcHandler for ProxySrcPadHandler {
    type ElementImpl = ProxySrc;

    fn src_event(
        &self,
        pad: PadSrcRef,
        proxysrc: &ProxySrc,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handling event {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                let _ = block_on(proxysrc.pause(element));
                true
            }
            EventView::FlushStop(..) => {
                let (res, state, pending) = element.get_state(0.into());
                if res == Ok(gst::StateChangeSuccess::Success) && state == gst::State::Playing
                    || res == Ok(gst::StateChangeSuccess::Async) && pending == gst::State::Playing
                {
                    let _ = block_on(proxysrc.start(element));
                }
                true
            }
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handled event {:?}", event);
        } else {
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Didn't handle event {:?}", event);
        }

        // FIXME can't forward to sink_pad
        Either::Left(ret)
    }

    fn src_query(
        &self,
        pad: PadSrcRef,
        _proxysrc: &ProxySrc,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handling query {:?}", query);
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
                let caps = if let Some(ref caps) = pad.gst_pad().get_current_caps() {
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
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handled query {:?}", query);
        } else {
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Didn't handle query {:?}", query);
        }

        ret
    }
}

#[derive(Debug)]
struct StateSrc {
    queue: Option<SharedQueue>,
}

impl StateSrc {
    #[inline]
    async fn lock_queue(&self) -> MutexGuard<'_, SharedQueueInner> {
        self.queue.as_ref().unwrap().lock().await
    }
}

impl Default for StateSrc {
    fn default() -> StateSrc {
        StateSrc { queue: None }
    }
}

#[derive(Debug)]
struct ProxySrc {
    src_pad: PadSrc,
    state: Mutex<StateSrc>,
    settings: Mutex<SettingsSrc>,
}

lazy_static! {
    static ref SRC_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-proxysrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing proxy source"),
    );
}

impl ProxySrc {
    async fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().await;
        gst_debug!(SRC_CAT, obj: element, "Preparing");

        let settings = self.settings.lock().await;

        let queue = SharedQueue::get(&settings.proxy_context, false)
            .await
            .ok_or_else(|| {
                gst_error_msg!(gst::ResourceError::OpenRead, ["Failed to create get queue"])
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
            if settings.max_size_time == 0 {
                None
            } else {
                Some(settings.max_size_time)
            },
        );

        queue.lock().await.dataqueue = Some(dataqueue);
        state.queue = Some(queue);

        let context =
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;

        self.src_pad
            .prepare(context, &ProxySrcPadHandler {})
            .await
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing src_pad: {:?}", err]
                )
            })?;

        gst_debug!(SRC_CAT, obj: element, "Prepared");

        Ok(())
    }

    async fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().await;
        gst_debug!(SRC_CAT, obj: element, "Unpreparing");

        self.src_pad.stop_task().await;
        let _ = self.src_pad.unprepare().await;

        if let Some(queue) = state.queue.take() {
            queue.unprepare().await;
        }

        *state = StateSrc::default();

        gst_debug!(SRC_CAT, obj: element, "Unprepared");

        Ok(())
    }

    async fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let state = self.state.lock().await;
        gst_debug!(SRC_CAT, obj: element, "Starting");

        let dataqueue = state.lock_queue().await.dataqueue.as_ref().unwrap().clone();
        dataqueue.start().await;

        ProxySrcPadHandler::start_task(self.src_pad.as_ref(), element, dataqueue).await;

        gst_debug!(SRC_CAT, obj: element, "Started");

        Ok(())
    }

    async fn pause(&self, element: &gst::Element) -> Result<(), ()> {
        let pause_completion = {
            let state = self.state.lock().await;
            gst_debug!(SRC_CAT, obj: element, "Stopping");

            let pause_completion = self.src_pad.pause_task().await;

            if let Some(dataqueue) = state.lock_queue().await.dataqueue.as_ref() {
                dataqueue.pause().await;
                dataqueue.clear().await;
                dataqueue.stop().await;
            }

            pause_completion
        };

        gst_debug!(SRC_CAT, obj: element, "Waiting for Task Pause to complete");
        pause_completion.await;

        gst_debug!(SRC_CAT, obj: element, "Stopped");

        Ok(())
    }
}

impl ObjectSubclass for ProxySrc {
    const NAME: &'static str = "RsTsProxySrc";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing proxy source",
            "Source/Generic",
            "Thread-sharing proxy source",
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

        klass.install_properties(&PROPERTIES_SRC);
    }

    fn new() -> Self {
        unreachable!()
    }

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("src").unwrap();
        let src_pad = PadSrc::new_from_template(&templ, Some("src"));

        Self {
            src_pad,
            state: Mutex::new(StateSrc::default()),
            settings: Mutex::new(SettingsSrc::default()),
        }
    }
}

impl ObjectImpl for ProxySrc {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES_SRC[id];

        match *prop {
            subclass::Property("max-size-buffers", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.max_size_buffers = value.get_some().expect("type checked upstream");
            }
            subclass::Property("max-size-bytes", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.max_size_bytes = value.get_some().expect("type checked upstream");
            }
            subclass::Property("max-size-time", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.max_size_time = value.get_some().expect("type checked upstream");
            }
            subclass::Property("context", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.context_wait = value.get_some().expect("type checked upstream");
            }
            subclass::Property("proxy-context", ..) => {
                let mut settings = block_on(self.settings.lock());
                settings.proxy_context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES_SRC[id];

        match *prop {
            subclass::Property("max-size-buffers", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.max_size_buffers.to_value())
            }
            subclass::Property("max-size-bytes", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.max_size_bytes.to_value())
            }
            subclass::Property("max-size-time", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.max_size_time.to_value())
            }
            subclass::Property("context", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.context.to_value())
            }
            subclass::Property("context-wait", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.context_wait.to_value())
            }
            subclass::Property("proxy-context", ..) => {
                let settings = block_on(self.settings.lock());
                Ok(settings.proxy_context.to_value())
            }
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

impl ElementImpl for ProxySrc {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(SRC_CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                block_on(self.prepare(element)).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                block_on(self.pause(element)).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                block_on(self.unprepare(element)).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => {
                block_on(self.start(element)).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        Ok(success)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-proxysink",
        gst::Rank::None,
        ProxySink::get_type(),
    )?;
    gst::Element::register(
        Some(plugin),
        "ts-proxysrc",
        gst::Rank::None,
        ProxySrc::get_type(),
    )
}
