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

use lazy_static::lazy_static;

use std::collections::{HashMap, VecDeque};
use std::sync::{self, Arc, Weak};
use std::{u32, u64};

use crate::runtime::prelude::*;
use crate::runtime::{
    self, Context, JoinHandle, PadSink, PadSinkRef, PadSinkWeak, PadSrc, PadSrcRef, PadSrcWeak,
};

use super::dataqueue::{DataQueue, DataQueueItem};

lazy_static! {
    static ref PROXY_CONTEXTS: Mutex<HashMap<String, Weak<Mutex<ProxyContextInner>>>> =
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

/// ProxySrc fields which are necessary for ProxySink
#[derive(Debug)]
struct SharedSrc {
    src_pad: PadSrcWeak,
}

/// ProxySink fields which are necessary for ProxySrc
#[derive(Debug)]
struct SharedSink {
    sink_pad: PadSinkWeak,
}

#[derive(Debug)]
struct ProxyContextInner {
    name: String,
    dataqueue: Option<DataQueue>,
    last_res: Result<gst::FlowSuccess, gst::FlowError>,
    pending_queue: Option<PendingQueue>,
    have_sink: bool,
    have_src: bool,
    shared_src_tx: Option<oneshot::Sender<SharedSrc>>,
    shared_src_rx: Option<oneshot::Receiver<SharedSrc>>,
    shared_sink_tx: Option<oneshot::Sender<SharedSink>>,
    shared_sink_rx: Option<oneshot::Receiver<SharedSink>>,
}

impl ProxyContextInner {
    async fn unprepare(&self) {
        let mut proxy_ctxs = PROXY_CONTEXTS.lock().await;
        proxy_ctxs.remove(&self.name);
    }
}

impl Drop for ProxyContextInner {
    fn drop(&mut self) {
        // Check invariants which can't be held automatically in `ProxyContext`
        // because `drop` can't be `async`
        if self.pending_queue.is_some() || self.dataqueue.is_some() {
            panic!("Missing call to `ProxyContext::unprepare`");
        }
    }
}

#[derive(Debug)]
struct ProxyContext {
    shared: Arc<Mutex<ProxyContextInner>>,
    as_sink: bool,
}

impl ProxyContext {
    #[inline]
    async fn lock_shared(&self) -> MutexGuard<'_, ProxyContextInner> {
        self.shared.lock().await
    }

    async fn get(name: &str, as_sink: bool) -> Option<Self> {
        let mut proxy_ctxs = PROXY_CONTEXTS.lock().await;

        let mut proxy_ctx = None;
        if let Some(shared_weak) = proxy_ctxs.get(name) {
            if let Some(shared) = shared_weak.upgrade() {
                {
                    let shared = shared.lock().await;
                    if (shared.have_sink && as_sink) || (shared.have_src && !as_sink) {
                        return None;
                    }
                }

                proxy_ctx = Some({
                    let proxy_ctx = ProxyContext { shared, as_sink };
                    {
                        let mut shared = proxy_ctx.lock_shared().await;
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
            let (shared_src_tx, shared_src_rx) = oneshot::channel();
            let (shared_sink_tx, shared_sink_rx) = oneshot::channel();
            let shared = Arc::new(Mutex::new(ProxyContextInner {
                name: name.into(),
                dataqueue: None,
                last_res: Err(gst::FlowError::Flushing),
                pending_queue: None,
                have_sink: as_sink,
                have_src: !as_sink,
                shared_src_tx: Some(shared_src_tx),
                shared_src_rx: Some(shared_src_rx),
                shared_sink_tx: Some(shared_sink_tx),
                shared_sink_rx: Some(shared_sink_rx),
            }));

            proxy_ctxs.insert(name.into(), Arc::downgrade(&shared));

            proxy_ctx = Some(ProxyContext { shared, as_sink });
        }

        proxy_ctx
    }

    async fn unprepare(&self) {
        let mut shared_ctx = self.lock_shared().await;
        if self.as_sink {
            assert!(shared_ctx.have_sink);
            shared_ctx.have_sink = false;
            let _ = shared_ctx.pending_queue.take();
        } else {
            assert!(shared_ctx.have_src);
            shared_ctx.have_src = false;
            let _ = shared_ctx.dataqueue.take();
        }
        shared_ctx.unprepare().await;
    }
}

#[derive(Debug)]
struct ProxySinkPadHandlerInner {
    flush_join_handle: sync::Mutex<Option<JoinHandle<Result<(), ()>>>>,
    src_pad: PadSrcWeak,
}

#[derive(Clone, Debug)]
struct ProxySinkPadHandler(Arc<ProxySinkPadHandlerInner>);

impl ProxySinkPadHandler {
    fn new(src_pad: PadSrcWeak) -> Self {
        ProxySinkPadHandler(Arc::new(ProxySinkPadHandlerInner {
            flush_join_handle: sync::Mutex::new(None),
            src_pad,
        }))
    }
}

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
        let element = element.clone();

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
        let element = element.clone();
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
        _proxysink: &ProxySink,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        use gst::EventView;

        if event.is_serialized() {
            let pad_weak = pad.downgrade();
            let element = element.clone();
            let inner_weak = Arc::downgrade(&self.0);

            Either::Right(
                async move {
                    let pad = pad_weak.upgrade().expect("PadSink no longer exists");
                    gst_log!(SINK_CAT, obj: pad.gst_pad(), "Handling {:?}", event);

                    let proxysink = ProxySink::from_instance(&element);

                    match event.view() {
                        EventView::Eos(..) => {
                            let _ = element
                                .post_message(&gst::Message::new_eos().src(Some(&element)).build());
                        }
                        EventView::FlushStop(..) => {
                            let inner = inner_weak.upgrade().unwrap();

                            let flush_join_handle = inner.flush_join_handle.lock().unwrap().take();
                            if let Some(flush_join_handle) = flush_join_handle {
                                gst_debug!(SINK_CAT, obj: pad.gst_pad(), "Waiting for FlushStart to complete");
                                if let Ok(Ok(())) = flush_join_handle.await {
                                    let _ = proxysink.start(&element).await;
                                } else {
                                    gst_debug!(SINK_CAT, obj: pad.gst_pad(), "FlushStop ignored: FlushStart failed to complete");
                                }
                            } else {
                                gst_debug!(SINK_CAT, obj: pad.gst_pad(), "FlushStop ignored: no Flush in progress");
                            }
                        }
                        _ => (),
                    }

                    gst_log!(SINK_CAT, obj: pad.gst_pad(), "Queuing {:?}", event);
                    let _ = proxysink
                        .enqueue_item(&element, DataQueueItem::Event(event))
                        .await;

                    true
                }
                .boxed(),
            )
        } else {
            let src_pad = self
                .0
                .src_pad
                .upgrade()
                .expect("PadSrc no longer available");

            if let EventView::FlushStart(..) = event.view() {
                let mut flush_join_handle = self.0.flush_join_handle.lock().unwrap();
                if flush_join_handle.is_none() {
                    gst_log!(SINK_CAT, obj: pad.gst_pad(), "Handling {:?}", event);
                    let element = element.clone();
                    *flush_join_handle = Some(src_pad.spawn(async move {
                        ProxySink::from_instance(&element).stop(&element).await
                    }));
                } else {
                    gst_debug!(SINK_CAT, obj: pad.gst_pad(), "FlushStart ignored: previous Flush in progress");
                }
            }

            gst_log!(SINK_CAT, obj: pad.gst_pad(), "Fowarding non-serialized {:?}", event);
            Either::Left(src_pad.gst_pad().push_event(event))
        }
    }
}

#[derive(Debug)]
struct StateSink {
    proxy_ctx: Option<ProxyContext>,
}

impl StateSink {
    #[inline]
    fn proxy_ctx(&self) -> &ProxyContext {
        self.proxy_ctx.as_ref().unwrap()
    }
}

impl Default for StateSink {
    fn default() -> Self {
        StateSink { proxy_ctx: None }
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

                    let proxy_ctx = state.proxy_ctx.as_ref();
                    if proxy_ctx.is_none() {
                        return;
                    }
                    let mut shared_ctx = proxy_ctx.unwrap().lock_shared().await;

                    gst_log!(SINK_CAT, obj: &element, "Trying to empty pending queue");

                    let ProxyContextInner {
                        pending_queue: ref mut pq,
                        ref dataqueue,
                        ..
                    } = *shared_ctx;

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

            let proxy_ctx = state.proxy_ctx.as_ref().ok_or(gst::FlowError::Error)?;
            let mut shared_ctx = proxy_ctx.lock_shared().await;

            let item = {
                let ProxyContextInner {
                    ref mut pending_queue,
                    ref dataqueue,
                    ..
                } = *shared_ctx;

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

        let state = self.state.lock().await;
        let shared_ctx = state.proxy_ctx().lock_shared().await;
        shared_ctx.last_res
    }

    async fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().await;
        gst_debug!(SINK_CAT, obj: element, "Preparing");

        let proxy_ctx = ProxyContext::get(&self.settings.lock().await.proxy_context, true)
            .await
            .ok_or_else(|| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to create or get ProxyContext"]
                )
            })?;

        {
            let mut shared_ctx = proxy_ctx.lock_shared().await;
            assert!(shared_ctx.shared_src_rx.is_some());
            shared_ctx
                .shared_sink_tx
                .take()
                .unwrap()
                .send(SharedSink {
                    sink_pad: self.sink_pad.downgrade(),
                })
                .map_err(|err| {
                    gst_error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to send SharedSink: {:?}", err]
                    )
                })?;
        }

        state.proxy_ctx = Some(proxy_ctx);

        gst_debug!(SINK_CAT, obj: element, "Prepared");

        Ok(())
    }

    async fn complete_preparation(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let state = self.state.lock().await;

        let shared_src_rx = state.proxy_ctx().lock_shared().await.shared_src_rx.take();
        if shared_src_rx.is_none() {
            gst_log!(SINK_CAT, obj: element, "Preparation already completed");
            return Ok(());
        }

        gst_debug!(SINK_CAT, obj: element, "Completing preparation");

        let SharedSrc { src_pad } = shared_src_rx.unwrap().await.map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to receive SharedSrc: {:?}", err]
            )
        })?;

        self.sink_pad
            .prepare(&ProxySinkPadHandler::new(src_pad))
            .await;

        gst_debug!(SINK_CAT, obj: element, "Preparation completed");

        Ok(())
    }

    async fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().await;
        gst_debug!(SINK_CAT, obj: element, "Unpreparing");

        let proxy_ctx = state.proxy_ctx.take().unwrap();
        proxy_ctx.unprepare().await;

        self.sink_pad.unprepare().await;

        *state = StateSink::default();

        gst_debug!(SINK_CAT, obj: element, "Unprepared");
        Ok(())
    }

    async fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let state = self.state.lock().await;
        gst_debug!(SINK_CAT, obj: element, "Starting");

        let mut shared_ctx = state.proxy_ctx().lock_shared().await;
        shared_ctx.last_res = Ok(gst::FlowSuccess::Ok);

        gst_debug!(SINK_CAT, obj: element, "Started");

        Ok(())
    }

    async fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        let state = self.state.lock().await;
        gst_debug!(SINK_CAT, obj: element, "Stopping");

        let mut shared_ctx = state.proxy_ctx().lock_shared().await;
        let _ = shared_ctx.pending_queue.take();
        shared_ctx.last_res = Err(gst::FlowError::Flushing);

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

        let mut settings = runtime::executor::block_on(self.settings.lock());
        match *prop {
            subclass::Property("proxy-context", ..) => {
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

        let settings = runtime::executor::block_on(self.settings.lock());
        match *prop {
            subclass::Property("proxy-context", ..) => Ok(settings.proxy_context.to_value()),
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
                runtime::executor::block_on(self.prepare(element)).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::ReadyToPaused => {
                runtime::executor::block_on(self.complete_preparation(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PausedToReady => {
                runtime::executor::block_on(self.stop(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                runtime::executor::block_on(self.unprepare(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let success = self.parent_change_state(element, transition)?;

        if transition == gst::StateChange::ReadyToPaused {
            runtime::executor::block_on(self.start(element)).map_err(|_| gst::StateChangeError)?;
        }

        Ok(success)
    }
}

#[derive(Debug)]
struct ProxySrcPadHandlerInner {
    flush_join_handle: sync::Mutex<Option<JoinHandle<Result<(), ()>>>>,
    sink_pad: PadSinkWeak,
}

#[derive(Clone, Debug)]
struct ProxySrcPadHandler(Arc<ProxySrcPadHandlerInner>);

impl ProxySrcPadHandler {
    fn new(sink_pad: PadSinkWeak) -> Self {
        ProxySrcPadHandler(Arc::new(ProxySrcPadHandlerInner {
            flush_join_handle: sync::Mutex::new(None),
            sink_pad,
        }))
    }

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
            let mut shared_ctx = state.proxy_ctx().lock_shared().await;
            if let Some(ref mut pending_queue) = shared_ctx.pending_queue {
                pending_queue.notify_more_queue_space();
            }
        }

        let res = match item {
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
        };

        match res {
            Ok(_) => {
                gst_log!(SRC_CAT, obj: pad.gst_pad(), "Successfully pushed item");
                let state = proxysrc.state.lock().await;
                let mut shared_ctx = state.proxy_ctx().lock_shared().await;
                shared_ctx.last_res = Ok(gst::FlowSuccess::Ok);
            }
            Err(gst::FlowError::Flushing) => {
                gst_debug!(SRC_CAT, obj: pad.gst_pad(), "Flushing");
                let state = proxysrc.state.lock().await;
                pad.pause_task().await;
                let mut shared_ctx = state.proxy_ctx().lock_shared().await;
                shared_ctx.last_res = Err(gst::FlowError::Flushing);
            }
            Err(gst::FlowError::Eos) => {
                gst_debug!(SRC_CAT, obj: pad.gst_pad(), "EOS");
                let state = proxysrc.state.lock().await;
                pad.pause_task().await;
                let mut shared_ctx = state.proxy_ctx().lock_shared().await;
                shared_ctx.last_res = Err(gst::FlowError::Eos);
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
                let mut shared_ctx = state.proxy_ctx().lock_shared().await;
                shared_ctx.last_res = Err(err);
            }
        }
    }
}

impl PadSrcHandler for ProxySrcPadHandler {
    type ElementImpl = ProxySrc;

    fn src_event(
        &self,
        pad: &PadSrcRef,
        _proxysrc: &ProxySrc,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        use gst::EventView;

        gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        if event.is_serialized() {
            let element = element.clone();
            let inner_weak = Arc::downgrade(&self.0);
            let src_pad_weak = pad.downgrade();
            let sink_pad_weak = self.0.sink_pad.clone();

            Either::Right(
                async move {
                    let ret = if let EventView::FlushStop(..) = event.view() {
                        let mut ret = false;

                        let src_pad = src_pad_weak.upgrade().unwrap();
                        let inner_weak = inner_weak.upgrade().unwrap();
                        let flush_join_handle = inner_weak.flush_join_handle.lock().unwrap().take();
                        if let Some(flush_join_handle) = flush_join_handle {
                            gst_debug!(SRC_CAT, obj: src_pad.gst_pad(), "Waiting for FlushStart to complete");
                            if let Ok(Ok(())) = flush_join_handle.await {
                                ret = ProxySrc::from_instance(&element)
                                    .start(&element)
                                    .await
                                    .is_ok();
                                gst_log!(SRC_CAT, obj: src_pad.gst_pad(), "FlushStop complete");
                            } else {
                                gst_debug!(SRC_CAT, obj: src_pad.gst_pad(), "FlushStop aborted: FlushStart failed");
                            }
                        } else {
                            gst_debug!(SRC_CAT, obj: src_pad.gst_pad(), "FlushStop ignored: no Flush in progress");
                        }

                        ret
                    } else {
                        true
                    };

                    if ret {
                        let src_pad = src_pad_weak.upgrade().expect("PadSrc no longer exists");
                        gst_log!(SRC_CAT, obj: src_pad.gst_pad(), "Forwarding serialized {:?}", event);
                        sink_pad_weak.upgrade().expect("PadSink no longer available").gst_pad().push_event(event)
                    } else {
                        false
                    }
                }
                .boxed(),
            )
        } else {
            if let EventView::FlushStart(..) = event.view() {
                let mut flush_join_handle = self.0.flush_join_handle.lock().unwrap();
                if flush_join_handle.is_none() {
                    let element = element.clone();
                    let pad_weak = pad.downgrade();

                    *flush_join_handle = Some(pad.spawn(async move {
                        let res = ProxySrc::from_instance(&element).pause(&element).await;
                        let pad = pad_weak.upgrade().unwrap();
                        if res.is_ok() {
                            gst_debug!(SRC_CAT, obj: pad.gst_pad(), "FlushStart complete");
                        } else {
                            gst_debug!(SRC_CAT, obj: pad.gst_pad(), "FlushStart failed");
                        }

                        res
                    }));
                } else {
                    gst_debug!(SRC_CAT, obj: pad.gst_pad(), "FlushStart ignored: previous Flush in progress");
                }
            }

            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Forwarding non-serialized {:?}", event);
            Either::Left(
                self.0
                    .sink_pad
                    .upgrade()
                    .expect("PadSink no longer available")
                    .gst_pad()
                    .push_event(event),
            )
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
                q.set(true, 0.into(), gst::CLOCK_TIME_NONE);
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
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Handled {:?}", query);
        } else {
            gst_log!(SRC_CAT, obj: pad.gst_pad(), "Didn't handle {:?}", query);
        }

        ret
    }
}

#[derive(Debug)]
struct StateSrc {
    proxy_ctx: Option<ProxyContext>,
    ts_ctx: Option<Context>,
}

impl StateSrc {
    #[inline]
    fn proxy_ctx(&self) -> &ProxyContext {
        self.proxy_ctx.as_ref().unwrap()
    }
}

impl Default for StateSrc {
    fn default() -> Self {
        StateSrc {
            proxy_ctx: None,
            ts_ctx: None,
        }
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

        let proxy_ctx = ProxyContext::get(&settings.proxy_context, false)
            .await
            .ok_or_else(|| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to create get shared_state"]
                )
            })?;

        let ts_ctx = Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
            gst_error_msg!(
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
            if settings.max_size_time == 0 {
                None
            } else {
                Some(settings.max_size_time)
            },
        );

        {
            let mut shared_ctx = proxy_ctx.lock_shared().await;
            assert!(shared_ctx.shared_sink_rx.is_some());
            shared_ctx
                .shared_src_tx
                .take()
                .unwrap()
                .send(SharedSrc {
                    src_pad: self.src_pad.downgrade(),
                })
                .map_err(|err| {
                    gst_error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to send SharedSrc: {:?}", err]
                    )
                })?;

            shared_ctx.dataqueue = Some(dataqueue);
        }

        state.ts_ctx = Some(ts_ctx);
        state.proxy_ctx = Some(proxy_ctx);

        gst_debug!(SRC_CAT, obj: element, "Prepared");

        Ok(())
    }

    async fn complete_preparation(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().await;

        let shared_sink_rx = state.proxy_ctx().lock_shared().await.shared_sink_rx.take();
        if shared_sink_rx.is_none() {
            gst_log!(SRC_CAT, obj: element, "Preparation already completed");
            return Ok(());
        }

        gst_debug!(SRC_CAT, obj: element, "Completing preparation");

        let SharedSink { sink_pad } = shared_sink_rx.unwrap().await.map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to receive SharedSink: {:?}", err]
            )
        })?;

        self.src_pad
            .prepare(
                state.ts_ctx.take().unwrap(),
                &ProxySrcPadHandler::new(sink_pad),
            )
            .await
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing src_pad: {:?}", err]
                )
            })?;

        gst_debug!(SRC_CAT, obj: element, "Preparation completed");

        Ok(())
    }

    async fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().await;
        gst_debug!(SRC_CAT, obj: element, "Unpreparing");

        self.src_pad.stop_task().await;
        let _ = self.src_pad.unprepare().await;

        if let Some(proxy_ctx) = state.proxy_ctx.take() {
            proxy_ctx.unprepare().await;
        }

        *state = StateSrc::default();

        gst_debug!(SRC_CAT, obj: element, "Unprepared");

        Ok(())
    }

    async fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let state = self.state.lock().await;
        gst_debug!(SRC_CAT, obj: element, "Starting");

        let dataqueue = state
            .proxy_ctx()
            .lock_shared()
            .await
            .dataqueue
            .as_ref()
            .unwrap()
            .clone();
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

            if let Some(dataqueue) = state.proxy_ctx().lock_shared().await.dataqueue.as_ref() {
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

        let mut settings = runtime::executor::block_on(self.settings.lock());
        match *prop {
            subclass::Property("max-size-buffers", ..) => {
                settings.max_size_buffers = value.get_some().expect("type checked upstream");
            }
            subclass::Property("max-size-bytes", ..) => {
                settings.max_size_bytes = value.get_some().expect("type checked upstream");
            }
            subclass::Property("max-size-time", ..) => {
                settings.max_size_time = value.get_some().expect("type checked upstream");
            }
            subclass::Property("context", ..) => {
                settings.context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                settings.context_wait = value.get_some().expect("type checked upstream");
            }
            subclass::Property("proxy-context", ..) => {
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

        let settings = runtime::executor::block_on(self.settings.lock());
        match *prop {
            subclass::Property("max-size-buffers", ..) => Ok(settings.max_size_buffers.to_value()),
            subclass::Property("max-size-bytes", ..) => Ok(settings.max_size_bytes.to_value()),
            subclass::Property("max-size-time", ..) => Ok(settings.max_size_time.to_value()),
            subclass::Property("context", ..) => Ok(settings.context.to_value()),
            subclass::Property("context-wait", ..) => Ok(settings.context_wait.to_value()),
            subclass::Property("proxy-context", ..) => Ok(settings.proxy_context.to_value()),
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
                runtime::executor::block_on(self.prepare(element)).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::ReadyToPaused => {
                runtime::executor::block_on(self.complete_preparation(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PlayingToPaused => {
                runtime::executor::block_on(self.pause(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                runtime::executor::block_on(self.unprepare(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => {
                runtime::executor::block_on(self.start(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
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
