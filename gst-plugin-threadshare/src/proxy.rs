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
use std::sync::{Arc, Mutex, Weak};
use std::task::{self, Poll};
use std::{u32, u64};

use tokio_executor::current_thread as tokio_current_thread;

use super::{dataqueue::*, iocontext::*};

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

struct SharedQueue(Arc<Mutex<SharedQueueInner>>, bool);

impl SharedQueue {
    fn get(name: &str, as_sink: bool) -> Option<Self> {
        let mut contexts = CONTEXTS.lock().unwrap();

        let mut queue = None;
        if let Some(context) = contexts.get(name) {
            if let Some(context) = context.upgrade() {
                {
                    let inner = context.lock().unwrap();
                    if (inner.have_sink && as_sink) || (inner.have_src && !as_sink) {
                        return None;
                    }
                }

                let context = SharedQueue(context, as_sink);
                {
                    let mut inner = context.0.lock().unwrap();
                    if as_sink {
                        inner.have_sink = true;
                    } else {
                        inner.have_src = true;
                    }
                }

                queue = Some(context);
            }
        }

        if queue.is_none() {
            let inner = Arc::new(Mutex::new(SharedQueueInner {
                name: name.into(),
                queue: None,
                last_res: Err(gst::FlowError::Flushing),
                pending_queue: None,
                pending_future_abort_handle: None,
                have_sink: as_sink,
                have_src: !as_sink,
            }));

            contexts.insert(name.into(), Arc::downgrade(&inner));

            queue = Some(SharedQueue(inner, as_sink));
        }

        queue
    }
}

impl Drop for SharedQueue {
    fn drop(&mut self) {
        if self.1 {
            let mut inner = self.0.lock().unwrap();
            assert!(inner.have_sink);
            inner.have_sink = false;
            if let Some((Some(waker), _, _)) = inner.pending_queue.take() {
                waker.wake();
            }
        } else {
            let mut inner = self.0.lock().unwrap();
            assert!(inner.have_src);
            inner.have_src = false;
            let _ = inner.queue.take();
            if let Some(abort_handle) = inner.pending_future_abort_handle.take() {
                abort_handle.abort();
            }
        }
    }
}

struct SharedQueueInner {
    name: String,
    queue: Option<DataQueue>,
    last_res: Result<gst::FlowSuccess, gst::FlowError>,
    pending_queue: Option<(Option<task::Waker>, bool, VecDeque<DataQueueItem>)>,
    pending_future_abort_handle: Option<future::AbortHandle>,
    have_sink: bool,
    have_src: bool,
}

impl Drop for SharedQueueInner {
    fn drop(&mut self) {
        let mut contexts = CONTEXTS.lock().unwrap();
        contexts.remove(&self.name);
    }
}

struct StateSrc {
    io_context: Option<IOContext>,
    pending_future_id: Option<PendingFutureId>,
    queue: Option<SharedQueue>,
}

impl Default for StateSrc {
    fn default() -> StateSrc {
        StateSrc {
            io_context: None,
            pending_future_id: None,
            queue: None,
        }
    }
}

struct StateSink {
    io_context: Option<IOContext>,
    pending_future_id: Option<PendingFutureId>,
    queue: Option<SharedQueue>,
}

impl Default for StateSink {
    fn default() -> StateSink {
        StateSink {
            io_context: None,
            pending_future_id: None,
            queue: None,
        }
    }
}

struct ProxySink {
    #[allow(unused)]
    sink_pad: gst::Pad,
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
    fn enqueue_item(
        &self,
        _pad: &gst::Pad,
        element: &gst::Element,
        item: DataQueueItem,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let wait_future = {
            let state = self.state.lock().unwrap();
            let StateSink {
                ref queue,
                ref io_context,
                pending_future_id,
                ..
            } = *state;
            let queue = queue.as_ref().ok_or(gst::FlowError::Error)?;

            let mut queue = queue.0.lock().unwrap();

            let item = {
                let SharedQueueInner {
                    ref mut pending_queue,
                    ref queue,
                    ..
                } = *queue;

                match (pending_queue, queue) {
                    (None, Some(ref queue)) => queue.push(item),
                    (Some((_, false, ref mut items)), Some(ref queue)) => {
                        let mut failed_item = None;
                        while let Some(item) = items.pop_front() {
                            if let Err(item) = queue.push(item) {
                                failed_item = Some(item);
                            }
                        }

                        if let Some(failed_item) = failed_item {
                            items.push_front(failed_item);

                            Err(item)
                        } else {
                            queue.push(item)
                        }
                    }
                    _ => Err(item),
                }
            };

            if let Err(item) = item {
                if queue
                    .pending_queue
                    .as_ref()
                    .map(|(_, scheduled, _)| !scheduled)
                    .unwrap_or(true)
                {
                    if queue.pending_queue.is_none() {
                        queue.pending_queue = Some((None, false, VecDeque::new()));
                    }

                    let schedule_now = match item {
                        DataQueueItem::Event(ref ev) if ev.get_type() != gst::EventType::Eos => {
                            false
                        }
                        _ => true,
                    };

                    queue.pending_queue.as_mut().unwrap().2.push_back(item);

                    gst_log!(
                        SINK_CAT,
                        obj: element,
                        "Proxy is full - Pushing first item on pending queue"
                    );

                    if schedule_now {
                        gst_log!(SINK_CAT, obj: element, "Scheduling pending queue now");

                        queue.pending_queue.as_mut().unwrap().1 = true;

                        let element_clone = element.clone();
                        let future = future::poll_fn(move |cx| {
                            let sink = Self::from_instance(&element_clone);
                            let state = sink.state.lock().unwrap();

                            gst_log!(
                                SINK_CAT,
                                obj: &element_clone,
                                "Trying to empty pending queue"
                            );

                            let mut queue = match state.queue {
                                Some(ref queue) => queue.0.lock().unwrap(),
                                None => {
                                    return Poll::Ready(Ok(()));
                                }
                            };

                            let SharedQueueInner {
                                ref mut pending_queue,
                                ref queue,
                                ..
                            } = *queue;

                            let res =
                                if let Some((ref mut waker, _, ref mut items)) = *pending_queue {
                                    if let Some(ref queue) = queue {
                                        let mut failed_item = None;
                                        while let Some(item) = items.pop_front() {
                                            if let Err(item) = queue.push(item) {
                                                failed_item = Some(item);
                                            }
                                        }

                                        if let Some(failed_item) = failed_item {
                                            items.push_front(failed_item);
                                            *waker = Some(cx.waker().clone());
                                            gst_log!(
                                                SINK_CAT,
                                                obj: &element_clone,
                                                "Waiting for more queue space"
                                            );
                                            Poll::Pending
                                        } else {
                                            gst_log!(
                                                SINK_CAT,
                                                obj: &element_clone,
                                                "Pending queue is empty now"
                                            );
                                            Poll::Ready(Ok(()))
                                        }
                                    } else {
                                        gst_log!(
                                            SINK_CAT,
                                            obj: &element_clone,
                                            "Waiting for queue to be allocated"
                                        );
                                        Poll::Pending
                                    }
                                } else {
                                    gst_log!(
                                        SINK_CAT,
                                        obj: &element_clone,
                                        "Flushing, dropping pending queue"
                                    );
                                    Poll::Ready(Ok(()))
                                };

                            if res == Poll::Ready(Ok(())) {
                                *pending_queue = None;
                            }

                            res
                        });

                        if let (Some(io_context), Some(pending_future_id)) =
                            (io_context.as_ref(), pending_future_id.as_ref())
                        {
                            io_context.add_pending_future(*pending_future_id, future);
                            None
                        } else {
                            Some(future)
                        }
                    } else {
                        gst_log!(SINK_CAT, obj: element, "Scheduling pending queue later");

                        None
                    }
                } else {
                    queue.pending_queue.as_mut().unwrap().2.push_back(item);

                    None
                }
            } else {
                None
            }
        };

        if let Some(wait_future) = wait_future {
            gst_log!(SINK_CAT, obj: element, "Blocking until queue becomes empty");
            tokio_current_thread::block_on_all(wait_future).map_err(|_| {
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["failed to wait for queue to become empty again"]
                );
                gst::FlowError::Error
            })?;
        }

        let state = self.state.lock().unwrap();
        let inner_queue = state.queue.as_ref().unwrap().0.lock().unwrap();
        inner_queue.last_res
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(SINK_CAT, obj: pad, "Handling buffer {:?}", buffer);
        self.enqueue_item(pad, element, DataQueueItem::Buffer(buffer))
    }

    fn sink_chain_list(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        list: gst::BufferList,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(SINK_CAT, obj: pad, "Handling buffer list {:?}", list);
        self.enqueue_item(pad, element, DataQueueItem::BufferList(list))
    }

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(SINK_CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Eos(..) => {
                let _ = element.post_message(&gst::Message::new_eos().src(Some(element)).build());
            }
            EventView::FlushStart(..) => {
                let _ = self.stop(element);
            }
            EventView::FlushStop(..) => {
                let (res, state, pending) = element.get_state(0.into());
                if res == Ok(gst::StateChangeSuccess::Success) && state == gst::State::Paused
                    || res == Ok(gst::StateChangeSuccess::Async) && pending == gst::State::Paused
                {
                    let _ = self.start(element);
                }
            }
            EventView::CustomDownstreamSticky(e) => {
                let s = e.get_structure().unwrap();
                if s.get_name() == "ts-io-context" {
                    let mut state = self.state.lock().unwrap();
                    let io_context = s
                        .get::<&IOContext>("io-context")
                        .expect("event field")
                        .expect("missing event field");
                    let pending_future_id = s
                        .get::<&PendingFutureId>("pending-future-id")
                        .expect("event field")
                        .expect("missing event field");

                    gst_debug!(
                        SINK_CAT,
                        obj: element,
                        "Got upstream pending future id {:?}",
                        pending_future_id
                    );

                    state.io_context = Some(io_context.clone());
                    state.pending_future_id = Some(*pending_future_id);
                }
            }
            _ => (),
        };

        gst_log!(SINK_CAT, obj: pad, "Queuing event {:?}", event);
        let _ = self.enqueue_item(pad, element, DataQueueItem::Event(event));
        true
    }

    fn sink_query(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(SINK_CAT, obj: pad, "Handling query {:?}", query);

        pad.query_default(Some(element), query)
    }

    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(SINK_CAT, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let mut state = self.state.lock().unwrap();

        state.queue = match SharedQueue::get(&settings.proxy_context, true) {
            Some(queue) => Some(queue),
            None => {
                return Err(gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to create get queue"]
                ));
            }
        };

        gst_debug!(SINK_CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(SINK_CAT, obj: element, "Unpreparing");

        let mut state = self.state.lock().unwrap();
        *state = StateSink::default();

        gst_debug!(SINK_CAT, obj: element, "Unprepared");
        Ok(())
    }

    fn start(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(SINK_CAT, obj: element, "Starting");
        let state = self.state.lock().unwrap();

        let mut queue = state.queue.as_ref().unwrap().0.lock().unwrap();
        queue.last_res = Ok(gst::FlowSuccess::Ok);

        gst_debug!(SINK_CAT, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(SINK_CAT, obj: element, "Stopping");
        let mut state = self.state.lock().unwrap();

        state.io_context = None;
        state.pending_future_id = None;

        let mut queue = state.queue.as_ref().unwrap().0.lock().unwrap();

        if let Some((Some(waker), _, _)) = queue.pending_queue.take() {
            waker.wake();
        }
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
        let sink_pad = gst::Pad::new_from_template(&templ, Some("sink"));

        sink_pad.set_chain_function(|pad, parent, buffer| {
            ProxySink::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |queue, element| queue.sink_chain(pad, element, buffer),
            )
        });
        sink_pad.set_chain_list_function(|pad, parent, list| {
            ProxySink::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |queue, element| queue.sink_chain_list(pad, element, list),
            )
        });
        sink_pad.set_event_function(|pad, parent, event| {
            ProxySink::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.sink_event(pad, element, event),
            )
        });
        sink_pad.set_query_function(|pad, parent, query| {
            ProxySink::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.sink_query(pad, element, query),
            )
        });

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
                let mut settings = self.settings.lock().unwrap();
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
                let settings = self.settings.lock().unwrap();
                Ok(settings.proxy_context.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sink_pad).unwrap();

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

struct ProxySrc {
    src_pad: gst::Pad,
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
    fn create_io_context_event(state: &StateSrc) -> Option<gst::Event> {
        if let (&Some(ref pending_future_id), &Some(ref io_context)) =
            (&state.pending_future_id, &state.io_context)
        {
            let s = gst::Structure::new(
                "ts-io-context",
                &[
                    ("io-context", &io_context),
                    ("pending-future-id", &*pending_future_id),
                ],
            );
            Some(gst::Event::new_custom_downstream_sticky(s).build())
        } else {
            None
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(SRC_CAT, obj: pad, "Handling event {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                let _ = self.stop(element);
                true
            }
            EventView::FlushStop(..) => {
                let (res, state, pending) = element.get_state(0.into());
                if res == Ok(gst::StateChangeSuccess::Success) && state == gst::State::Playing
                    || res == Ok(gst::StateChangeSuccess::Async) && pending == gst::State::Playing
                {
                    let _ = self.start(element);
                }
                true
            }
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst_log!(SRC_CAT, obj: pad, "Handled event {:?}", event);
        } else {
            gst_log!(SRC_CAT, obj: pad, "Didn't handle event {:?}", event);
        }

        ret
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(SRC_CAT, obj: pad, "Handling query {:?}", query);
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
                let caps = if let Some(ref caps) = self.src_pad.get_current_caps() {
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
            gst_log!(SRC_CAT, obj: pad, "Handled query {:?}", query);
        } else {
            gst_log!(SRC_CAT, obj: pad, "Didn't handle query {:?}", query);
        }
        ret
    }

    async fn push_item(element: gst::Element, item: DataQueueItem) -> Result<(), gst::FlowError> {
        let src = Self::from_instance(&element);

        let event = {
            let state = src.state.lock().unwrap();
            let queue = state.queue.as_ref().unwrap().0.lock().unwrap();
            if let Some((Some(ref waker), _, _)) = queue.pending_queue {
                waker.wake_by_ref();
            }

            if src.src_pad.check_reconfigure() {
                Self::create_io_context_event(&state)
            } else {
                None
            }
        };

        if let Some(event) = event {
            src.src_pad.push_event(event);
        }

        let res = match item {
            DataQueueItem::Buffer(buffer) => {
                gst_log!(SRC_CAT, obj: &element, "Forwarding buffer {:?}", buffer);
                src.src_pad.push(buffer).map(|_| ())
            }
            DataQueueItem::BufferList(list) => {
                gst_log!(SRC_CAT, obj: &element, "Forwarding buffer list {:?}", list);
                src.src_pad.push_list(list).map(|_| ())
            }
            DataQueueItem::Event(event) => {
                use gst::EventView;

                let mut new_event = None;
                match event.view() {
                    EventView::CustomDownstreamSticky(e) => {
                        let s = e.get_structure().unwrap();
                        if s.get_name() == "ts-io-context" {
                            let state = src.state.lock().unwrap();
                            new_event = Self::create_io_context_event(&state);
                        }
                    }
                    _ => (),
                }

                match new_event {
                    Some(event) => {
                        gst_log!(SRC_CAT, obj: &element, "Forwarding new event {:?}", event);
                        src.src_pad.push_event(event);
                    }
                    None => {
                        gst_log!(SRC_CAT, obj: &element, "Forwarding event {:?}", event);
                        src.src_pad.push_event(event);
                    }
                }
                Ok(())
            }
        };

        match res {
            Ok(_) => {
                gst_log!(SRC_CAT, obj: &element, "Successfully pushed item");
                let state = src.state.lock().unwrap();
                let mut queue = state.queue.as_ref().unwrap().0.lock().unwrap();
                queue.last_res = Ok(gst::FlowSuccess::Ok);
            }
            Err(gst::FlowError::Flushing) => {
                gst_debug!(SRC_CAT, obj: &element, "Flushing");
                let state = src.state.lock().unwrap();
                let mut queue = state.queue.as_ref().unwrap().0.lock().unwrap();
                if let Some(ref queue) = queue.queue {
                    queue.pause();
                }
                queue.last_res = Err(gst::FlowError::Flushing);
            }
            Err(gst::FlowError::Eos) => {
                gst_debug!(SRC_CAT, obj: &element, "EOS");
                let state = src.state.lock().unwrap();
                let mut queue = state.queue.as_ref().unwrap().0.lock().unwrap();
                if let Some(ref queue) = queue.queue {
                    queue.pause();
                }
                queue.last_res = Err(gst::FlowError::Eos);
            }
            Err(err) => {
                gst_error!(SRC_CAT, obj: &element, "Got error {}", err);
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["streaming stopped, reason {}", err]
                );
                let state = src.state.lock().unwrap();
                let mut queue = state.queue.as_ref().unwrap().0.lock().unwrap();
                queue.last_res = Err(err);
                return Err(gst::FlowError::CustomError);
            }
        }

        let abortable_drain = {
            let state = src.state.lock().unwrap();

            if let StateSrc {
                io_context: Some(ref io_context),
                pending_future_id: Some(ref pending_future_id),
                queue: Some(ref queue),
                ..
            } = *state
            {
                let (abort_handle, abortable_drain) =
                    io_context.drain_pending_futures(*pending_future_id);
                queue.0.lock().unwrap().pending_future_abort_handle = abort_handle;

                abortable_drain
            } else {
                return Ok(());
            }
        };

        abortable_drain.await
    }

    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(SRC_CAT, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let mut state = self.state.lock().unwrap();

        let io_context =
            IOContext::new(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to create IO context: {}", err]
                )
            })?;

        let queue = SharedQueue::get(&settings.proxy_context, false).ok_or_else(|| {
            gst_error_msg!(gst::ResourceError::OpenRead, ["Failed to create get queue"])
        })?;

        let dataqueue = DataQueue::new(
            &element.clone().upcast(),
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

        let element_clone = element.clone();
        let element_clone2 = element.clone();
        dataqueue
            .schedule(
                &io_context,
                move |item| Self::push_item(element_clone.clone(), item),
                move |err| {
                    gst_error!(SRC_CAT, obj: &element_clone2, "Got error {}", err);
                    match err {
                        gst::FlowError::CustomError => (),
                        err => {
                            gst_element_error!(
                                element_clone2,
                                gst::StreamError::Failed,
                                ("Internal data stream error"),
                                ["streaming stopped, reason {}", err]
                            );
                        }
                    }
                },
            )
            .map_err(|_| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to schedule data queue"]
                )
            })?;

        let pending_future_id = io_context.acquire_pending_future_id();
        gst_debug!(
            SRC_CAT,
            obj: element,
            "Got pending future id {:?}",
            pending_future_id
        );

        queue.0.lock().unwrap().queue = Some(dataqueue);

        state.io_context = Some(io_context);
        state.pending_future_id = Some(pending_future_id);
        state.queue = Some(queue);

        gst_debug!(SRC_CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(SRC_CAT, obj: element, "Unpreparing");

        // FIXME: The IO Context has to be alive longer than the queue,
        // otherwise the queue can't finish any remaining work
        let (mut queue, io_context) = {
            let mut state = self.state.lock().unwrap();

            if let (&Some(ref pending_future_id), &Some(ref io_context)) =
                (&state.pending_future_id, &state.io_context)
            {
                io_context.release_pending_future_id(*pending_future_id);
            }

            let queue = if let Some(ref queue) = state.queue.take() {
                let mut queue = queue.0.lock().unwrap();
                queue.queue.take()
            } else {
                None
            };

            let io_context = state.io_context.take();

            *state = StateSrc::default();

            (queue, io_context)
        };

        if let Some(ref queue) = queue.take() {
            queue.shutdown();
        }
        drop(io_context);

        gst_debug!(SRC_CAT, obj: element, "Unprepared");

        Ok(())
    }

    fn start(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(SRC_CAT, obj: element, "Starting");
        let state = self.state.lock().unwrap();
        let queue = state.queue.as_ref().unwrap().0.lock().unwrap();

        if let Some(ref queue) = queue.queue {
            queue.unpause();
        }

        gst_debug!(SRC_CAT, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(SRC_CAT, obj: element, "Stopping");
        let state = self.state.lock().unwrap();
        let mut queue = state.queue.as_ref().unwrap().0.lock().unwrap();

        if let Some(ref queue) = queue.queue {
            queue.pause();
            queue.clear(&self.src_pad);
        }
        if let Some(abort_handle) = queue.pending_future_abort_handle.take() {
            abort_handle.abort();
        }

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
        let src_pad = gst::Pad::new_from_template(&templ, Some("src"));

        src_pad.set_event_function(|pad, parent, event| {
            ProxySrc::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.src_event(pad, element, event),
            )
        });
        src_pad.set_query_function(|pad, parent, query| {
            ProxySrc::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.src_query(pad, element, query),
            )
        });

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
                let mut settings = self.settings.lock().unwrap();
                settings.max_size_buffers = value.get_some().expect("type checked upstream");
            }
            subclass::Property("max-size-bytes", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_size_bytes = value.get_some().expect("type checked upstream");
            }
            subclass::Property("max-size-time", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_size_time = value.get_some().expect("type checked upstream");
            }
            subclass::Property("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_wait = value.get_some().expect("type checked upstream");
            }
            subclass::Property("proxy-context", ..) => {
                let mut settings = self.settings.lock().unwrap();
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
                let settings = self.settings.lock().unwrap();
                Ok(settings.max_size_buffers.to_value())
            }
            subclass::Property("max-size-bytes", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.max_size_bytes.to_value())
            }
            subclass::Property("max-size-time", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.max_size_time.to_value())
            }
            subclass::Property("context", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.context.to_value())
            }
            subclass::Property("context-wait", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.context_wait.to_value())
            }
            subclass::Property("proxy-context", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.proxy_context.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.src_pad).unwrap();

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
                self.stop(element).map_err(|_| gst::StateChangeError)?;
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
