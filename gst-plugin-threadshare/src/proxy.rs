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
use std::sync::Mutex as StdMutex;
use std::sync::MutexGuard as StdMutexGuard;
use std::sync::{Arc, Weak};
use std::{u32, u64};

use crate::runtime::prelude::*;
use crate::runtime::{Context, PadSink, PadSinkRef, PadSinkWeak, PadSrc, PadSrcRef, PadSrcWeak};

use super::dataqueue::{DataQueue, DataQueueItem, DataQueueState};

lazy_static! {
    static ref PROXY_CONTEXTS: StdMutex<HashMap<String, Weak<StdMutex<ProxyContextInner>>>> =
        StdMutex::new(HashMap::new());
    static ref PROXY_SRC_PADS: StdMutex<HashMap<String, PadSrcWeak>> =
        StdMutex::new(HashMap::new());
    static ref PROXY_SINK_PADS: StdMutex<HashMap<String, PadSinkWeak>> =
        StdMutex::new(HashMap::new());
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

#[derive(Debug)]
struct ProxySinkPadHandlerInner {
    proxy_ctx: ProxyContext,
}

#[derive(Clone, Debug)]
struct ProxySinkPadHandler(Arc<ProxySinkPadHandlerInner>);

impl ProxySinkPadHandler {
    fn new(proxy_ctx: ProxyContext) -> Self {
        ProxySinkPadHandler(Arc::new(ProxySinkPadHandlerInner { proxy_ctx }))
    }

    fn proxy_ctx(&self) -> &ProxyContext {
        &self.0.proxy_ctx
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
        proxysink: &ProxySink,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_debug!(SINK_CAT, obj: pad.gst_pad(), "Handling non-serialized {:?}", event);

        let src_pad = PROXY_SRC_PADS
            .lock()
            .unwrap()
            .get(&self.proxy_ctx().name)
            .and_then(|src_pad| src_pad.upgrade())
            .map(|src_pad| src_pad.gst_pad().clone());

        if let EventView::FlushStart(..) = event.view() {
            proxysink.stop(&element).unwrap();
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
        let element = element.clone();
        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            let proxysink = ProxySink::from_instance(&element);

            match event.view() {
                EventView::Eos(..) => {
                    let _ =
                        element.post_message(&gst::Message::new_eos().src(Some(&element)).build());
                }
                EventView::FlushStop(..) => proxysink.start(&element).unwrap(),
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
struct ProxySink {
    sink_pad: PadSink,
    sink_pad_handler: StdMutex<Option<ProxySinkPadHandler>>,
    settings: StdMutex<SettingsSink>,
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
                    let sink_pad_handler = sink.sink_pad_handler.lock().unwrap();
                    if sink_pad_handler.is_none() {
                        return;
                    }

                    let proxy_ctx = sink_pad_handler.as_ref().unwrap().proxy_ctx();
                    let mut shared_ctx = proxy_ctx.lock_shared();

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
            let sink_pad_handler = self.sink_pad_handler.lock().unwrap();

            let proxy_ctx = sink_pad_handler
                .as_ref()
                .ok_or(gst::FlowError::Error)?
                .proxy_ctx();
            let mut shared_ctx = proxy_ctx.lock_shared();

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

        let sink_pad_handler = self.sink_pad_handler.lock().unwrap();
        let shared_ctx = sink_pad_handler.as_ref().unwrap().proxy_ctx().lock_shared();
        shared_ctx.last_res
    }

    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(SINK_CAT, obj: element, "Preparing");

        let proxy_context = self.settings.lock().unwrap().proxy_context.to_string();

        let proxy_ctx = ProxyContext::get(&proxy_context, true).ok_or_else(|| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to create or get ProxyContext"]
            )
        })?;

        {
            let mut proxy_sink_pads = PROXY_SINK_PADS.lock().unwrap();
            assert!(!proxy_sink_pads.contains_key(&proxy_context));
            proxy_sink_pads.insert(proxy_context, self.sink_pad.downgrade());
        }

        let handler = ProxySinkPadHandler::new(proxy_ctx);
        self.sink_pad.prepare(&handler);
        *self.sink_pad_handler.lock().unwrap() = Some(handler);

        gst_debug!(SINK_CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(SINK_CAT, obj: element, "Unpreparing");

        self.sink_pad.unprepare();
        *self.sink_pad_handler.lock().unwrap() = None;

        gst_debug!(SINK_CAT, obj: element, "Unprepared");
        Ok(())
    }

    fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let sink_pad_handler = self.sink_pad_handler.lock().unwrap();
        gst_debug!(SINK_CAT, obj: element, "Starting");

        {
            let settings = self.settings.lock().unwrap();
            let mut proxy_sink_pads = PROXY_SINK_PADS.lock().unwrap();
            proxy_sink_pads.remove(&settings.proxy_context);
        }

        let mut shared_ctx = sink_pad_handler.as_ref().unwrap().proxy_ctx().lock_shared();
        shared_ctx.last_res = Ok(gst::FlowSuccess::Ok);

        gst_debug!(SINK_CAT, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        let sink_pad_handler = self.sink_pad_handler.lock().unwrap();
        gst_debug!(SINK_CAT, obj: element, "Stopping");

        let mut shared_ctx = sink_pad_handler.as_ref().unwrap().proxy_ctx().lock_shared();
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
            sink_pad_handler: StdMutex::new(None),
            settings: StdMutex::new(SettingsSink::default()),
        }
    }
}

impl ObjectImpl for ProxySink {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES_SINK[id];

        let mut settings = self.settings.lock().unwrap();
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

        let settings = self.settings.lock().unwrap();
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
                self.prepare(element).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PausedToReady => {
                self.stop(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let success = self.parent_change_state(element, transition)?;

        if transition == gst::StateChange::ReadyToPaused {
            self.start(element).map_err(|_| gst::StateChangeError)?;
        }

        Ok(success)
    }
}

#[derive(Debug)]
struct ProxySrcPadHandlerInner {
    proxy_ctx: ProxyContext,
}

#[derive(Clone, Debug)]
struct ProxySrcPadHandler(Arc<ProxySrcPadHandlerInner>);

impl ProxySrcPadHandler {
    fn new(proxy_ctx: ProxyContext) -> Self {
        ProxySrcPadHandler(Arc::new(ProxySrcPadHandlerInner { proxy_ctx }))
    }

    fn proxy_ctx(&self) -> &ProxyContext {
        &self.0.proxy_ctx
    }

    fn start_task(&self, pad: PadSrcRef<'_>, element: &gst::Element, dataqueue: DataQueue) {
        let this = self.clone();
        let pad_weak = pad.downgrade();
        let element = element.clone();

        pad.start_task(move || {
            let this = this.clone();
            let pad_weak = pad_weak.clone();
            let element = element.clone();
            let mut dataqueue = dataqueue.clone();

            async move {
                let item = dataqueue.next().await;

                let pad = pad_weak.upgrade().expect("PadSrc no longer exists");
                let item = match item {
                    Some(item) => item,
                    None => {
                        gst_log!(SRC_CAT, obj: pad.gst_pad(), "DataQueue Stopped or Paused");
                        return glib::Continue(false);
                    }
                };

                match this.push_item(&pad, item).await {
                    Ok(_) => {
                        gst_log!(SRC_CAT, obj: pad.gst_pad(), "Successfully pushed item");
                        let mut shared_ctx = this.proxy_ctx().lock_shared();
                        shared_ctx.last_res = Ok(gst::FlowSuccess::Ok);
                        glib::Continue(true)
                    }
                    Err(gst::FlowError::Flushing) => {
                        gst_debug!(SRC_CAT, obj: pad.gst_pad(), "Flushing");
                        let mut shared_ctx = this.proxy_ctx().lock_shared();
                        shared_ctx.last_res = Err(gst::FlowError::Flushing);
                        glib::Continue(false)
                    }
                    Err(gst::FlowError::Eos) => {
                        gst_debug!(SRC_CAT, obj: pad.gst_pad(), "EOS");
                        let mut shared_ctx = this.proxy_ctx().lock_shared();
                        shared_ctx.last_res = Err(gst::FlowError::Eos);
                        glib::Continue(false)
                    }
                    Err(err) => {
                        gst_error!(SRC_CAT, obj: pad.gst_pad(), "Got error {}", err);
                        gst_element_error!(
                            element,
                            gst::StreamError::Failed,
                            ("Internal data stream error"),
                            ["streaming stopped, reason {}", err]
                        );
                        let mut shared_ctx = this.proxy_ctx().lock_shared();
                        shared_ctx.last_res = Err(err);
                        glib::Continue(false)
                    }
                }
            }
        });
    }

    async fn push_item(
        &self,
        pad: &PadSrcRef<'_>,
        item: DataQueueItem,
    ) -> Result<(), gst::FlowError> {
        {
            let mut shared_ctx = self.proxy_ctx().lock_shared();
            if let Some(ref mut pending_queue) = shared_ctx.pending_queue {
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

        let sink_pad = PROXY_SINK_PADS
            .lock()
            .unwrap()
            .get(&self.proxy_ctx().name)
            .and_then(|sink_pad| sink_pad.upgrade())
            .map(|sink_pad| sink_pad.gst_pad().clone());

        match event.view() {
            EventView::FlushStart(..) => proxysrc.stop(element).unwrap(),
            EventView::FlushStop(..) => proxysrc.flush_stop(element),
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
struct ProxySrc {
    src_pad: PadSrc,
    src_pad_handler: StdMutex<Option<ProxySrcPadHandler>>,
    dataqueue: StdMutex<Option<DataQueue>>,
    settings: StdMutex<SettingsSrc>,
}

lazy_static! {
    static ref SRC_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-proxysrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing proxy source"),
    );
}

impl ProxySrc {
    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(SRC_CAT, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let proxy_ctx = ProxyContext::get(&settings.proxy_context, false).ok_or_else(|| {
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
            let mut shared_ctx = proxy_ctx.lock_shared();
            shared_ctx.dataqueue = Some(dataqueue.clone());

            let mut proxy_src_pads = PROXY_SRC_PADS.lock().unwrap();
            assert!(!proxy_src_pads.contains_key(&settings.proxy_context));
            proxy_src_pads.insert(settings.proxy_context, self.src_pad.downgrade());
        }

        *self.dataqueue.lock().unwrap() = Some(dataqueue);

        let handler = ProxySrcPadHandler::new(proxy_ctx);

        self.src_pad.prepare(ts_ctx, &handler).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                ["Error preparing src_pad: {:?}", err]
            )
        })?;

        *self.src_pad_handler.lock().unwrap() = Some(handler);

        gst_debug!(SRC_CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(SRC_CAT, obj: element, "Unpreparing");

        {
            let settings = self.settings.lock().unwrap();
            let mut proxy_src_pads = PROXY_SRC_PADS.lock().unwrap();
            proxy_src_pads.remove(&settings.proxy_context);
        }

        self.src_pad.stop_task();
        let _ = self.src_pad.unprepare();
        *self.src_pad_handler.lock().unwrap() = None;

        *self.dataqueue.lock().unwrap() = None;

        gst_debug!(SRC_CAT, obj: element, "Unprepared");

        Ok(())
    }

    fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        // Keep the lock on the `dataqueue` until `stop` is complete
        // so as to prevent race conditions due to concurrent FlushStart/Stop.
        // Note that this won't deadlock as `ProxySrc::dataqueue` guards a pointer to
        // the `dataqueue` used within the `src_pad`'s `Task`.
        let dataqueue = self.dataqueue.lock().unwrap();
        gst_debug!(SRC_CAT, obj: element, "Stopping");

        // Now stop the task if it was still running, blocking
        // until this has actually happened
        self.src_pad.stop_task();

        let dataqueue = dataqueue.as_ref().unwrap();
        dataqueue.clear();
        dataqueue.stop();

        gst_debug!(SRC_CAT, obj: element, "Stopped");

        Ok(())
    }

    fn start(&self, element: &gst::Element) -> Result<(), ()> {
        let dataqueue = self.dataqueue.lock().unwrap();
        let dataqueue = dataqueue.as_ref().unwrap();
        if dataqueue.state() == DataQueueState::Started {
            gst_debug!(SRC_CAT, obj: element, "Already started");
            return Ok(());
        }

        gst_debug!(SRC_CAT, obj: element, "Starting");

        self.start_unchecked(element, dataqueue);

        gst_debug!(SRC_CAT, obj: element, "Started");

        Ok(())
    }

    fn flush_stop(&self, element: &gst::Element) {
        // Keep the lock on the `dataqueue` until `flush_stop` is complete
        // so as to prevent race conditions due to concurrent state transitions.
        // Note that this won't deadlock as `ProxySrc::dataqueue` guards a pointer to
        // the `dataqueue` used within the `src_pad`'s `Task`.
        let dataqueue = self.dataqueue.lock().unwrap();
        let dataqueue = dataqueue.as_ref().unwrap();
        if dataqueue.state() == DataQueueState::Started {
            gst_debug!(SRC_CAT, obj: element, "Already started");
            return;
        }

        gst_debug!(SRC_CAT, obj: element, "Stopping Flush");

        self.src_pad.stop_task();
        self.start_unchecked(element, dataqueue);

        gst_debug!(SRC_CAT, obj: element, "Stopped Flush");
    }

    fn start_unchecked(&self, element: &gst::Element, dataqueue: &DataQueue) {
        dataqueue.start();

        let src_pad_handler = self.src_pad_handler.lock().unwrap();
        let src_pad_handler = src_pad_handler.as_ref().unwrap();

        let mut shared_ctx = src_pad_handler.proxy_ctx().lock_shared();
        if let Some(pending_queue) = shared_ctx.pending_queue.as_mut() {
            pending_queue.notify_more_queue_space();
        }

        src_pad_handler.start_task(self.src_pad.as_ref(), element, dataqueue.clone());
    }

    fn pause(&self, element: &gst::Element) -> Result<(), ()> {
        let dataqueue = self.dataqueue.lock().unwrap();
        gst_debug!(SRC_CAT, obj: element, "Pausing");

        dataqueue.as_ref().unwrap().pause();

        self.src_pad.pause_task();

        gst_debug!(SRC_CAT, obj: element, "Paused");

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
            src_pad_handler: StdMutex::new(None),
            dataqueue: StdMutex::new(None),
            settings: StdMutex::new(SettingsSrc::default()),
        }
    }
}

impl ObjectImpl for ProxySrc {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES_SRC[id];

        let mut settings = self.settings.lock().unwrap();
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

        let settings = self.settings.lock().unwrap();
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
                self.prepare(element).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                self.pause(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element).map_err(|_| gst::StateChangeError)?;
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
