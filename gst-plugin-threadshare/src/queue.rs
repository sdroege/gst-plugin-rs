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

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::collections::VecDeque;
use std::sync::Mutex;
use std::{u32, u64};

use futures;
use futures::future;
use futures::task;
use futures::{Async, Future};

use tokio::executor;

use dataqueue::*;
use iocontext::*;

const DEFAULT_MAX_SIZE_BUFFERS: u32 = 200;
const DEFAULT_MAX_SIZE_BYTES: u32 = 1024 * 1024;
const DEFAULT_MAX_SIZE_TIME: u64 = gst::SECOND_VAL;
const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;

#[derive(Debug, Clone)]
struct Settings {
    max_size_buffers: u32,
    max_size_bytes: u32,
    max_size_time: u64,
    context: String,
    context_wait: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            max_size_buffers: DEFAULT_MAX_SIZE_BUFFERS,
            max_size_bytes: DEFAULT_MAX_SIZE_BYTES,
            max_size_time: DEFAULT_MAX_SIZE_TIME,
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
        }
    }
}

static PROPERTIES: [subclass::Property; 5] = [
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
];

struct PendingQueue {
    task: Option<task::Task>,
    scheduled: bool,
    items: VecDeque<DataQueueItem>,
}

struct State {
    io_context: Option<IOContext>,
    pending_future_id: Option<PendingFutureId>,
    io_context_in: Option<IOContext>,
    pending_future_id_in: Option<PendingFutureId>,
    queue: Option<DataQueue>,
    pending_queue: Option<PendingQueue>,
    last_res: Result<gst::FlowSuccess, gst::FlowError>,
    pending_future_cancel: Option<futures::sync::oneshot::Sender<()>>,
}

impl Default for State {
    fn default() -> State {
        State {
            io_context: None,
            pending_future_id: None,
            io_context_in: None,
            pending_future_id_in: None,
            queue: None,
            pending_queue: None,
            last_res: Ok(gst::FlowSuccess::Ok),
            pending_future_cancel: None,
        }
    }
}

struct Queue {
    cat: gst::DebugCategory,
    sink_pad: gst::Pad,
    src_pad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl Queue {
    fn create_io_context_event(state: &State) -> Option<gst::Event> {
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

    /* Try transfering all the items from the pending queue to the DataQueue, then
     * the current item. Errors out if the DataQueue was full, or the pending queue
     * is already scheduled, in which case the current item should be added to the
     * pending queue */
    fn queue_until_full(
        &self,
        queue: &DataQueue,
        pending_queue: &mut Option<PendingQueue>,
        item: DataQueueItem,
    ) -> Result<(), DataQueueItem> {
        match pending_queue {
            None => queue.push(item),
            Some(PendingQueue {
                scheduled: false,
                ref mut items,
                ..
            }) => {
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
    }

    /* Schedules emptying of the pending queue. If there is an upstream
     * io context, the new pending future is added to it, it is otherwise
     * returned, for the caller to block on */
    fn schedule_pending_queue(
        &self,
        element: &gst::Element,
        state: &mut State,
    ) -> Option<impl Future<Item = (), Error = ()>> {
        gst_log!(self.cat, obj: element, "Scheduling pending queue now");

        let State {
            ref mut pending_queue,
            ref io_context_in,
            pending_future_id_in,
            ..
        } = *state;

        pending_queue.as_mut().unwrap().scheduled = true;

        let element_clone = element.clone();
        let future = future::poll_fn(move || {
            let queue = Self::from_instance(&element_clone);
            let mut state = queue.state.lock().unwrap();

            let State {
                queue: ref dq,
                ref mut pending_queue,
                ..
            } = *state;

            if dq.is_none() {
                return Ok(Async::Ready(()));
            }

            gst_log!(
                queue.cat,
                obj: &element_clone,
                "Trying to empty pending queue"
            );

            let res = if let Some(PendingQueue {
                ref mut task,
                ref mut items,
                ..
            }) = *pending_queue
            {
                let mut failed_item = None;
                while let Some(item) = items.pop_front() {
                    if let Err(item) = dq.as_ref().unwrap().push(item) {
                        failed_item = Some(item);
                    }
                }

                if let Some(failed_item) = failed_item {
                    items.push_front(failed_item);
                    *task = Some(task::current());
                    gst_log!(
                        queue.cat,
                        obj: &element_clone,
                        "Waiting for more queue space"
                    );
                    Ok(Async::NotReady)
                } else {
                    gst_log!(queue.cat, obj: &element_clone, "Pending queue is empty now");
                    Ok(Async::Ready(()))
                }
            } else {
                gst_log!(
                    queue.cat,
                    obj: &element_clone,
                    "Flushing, dropping pending queue"
                );
                Ok(Async::Ready(()))
            };

            if res == Ok(Async::Ready(())) {
                *pending_queue = None;
            }

            res
        });

        if let (Some(io_context_in), Some(pending_future_id_in)) =
            (io_context_in.as_ref(), pending_future_id_in.as_ref())
        {
            io_context_in.add_pending_future(*pending_future_id_in, future);
            None
        } else {
            Some(future)
        }
    }

    fn enqueue_item(
        &self,
        _pad: &gst::Pad,
        element: &gst::Element,
        item: DataQueueItem,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let wait_future = {
            let mut state = self.state.lock().unwrap();
            let State {
                ref queue,
                ref mut pending_queue,
                ref io_context_in,
                ..
            } = *state;

            let queue = queue.as_ref().ok_or(gst::FlowError::Error)?;

            if let Err(item) = self.queue_until_full(queue, pending_queue, item) {
                if pending_queue
                    .as_ref()
                    .map(|pq| !pq.scheduled)
                    .unwrap_or(true)
                {
                    if pending_queue.is_none() {
                        *pending_queue = Some(PendingQueue {
                            task: None,
                            scheduled: false,
                            items: VecDeque::new(),
                        });
                    }

                    let schedule_now = match item {
                        DataQueueItem::Event(ref ev) if ev.get_type() != gst::EventType::Eos => {
                            false
                        }
                        _ => true,
                    };

                    pending_queue.as_mut().unwrap().items.push_back(item);

                    gst_log!(
                        self.cat,
                        obj: element,
                        "Queue is full - Pushing first item on pending queue"
                    );

                    if schedule_now {
                        self.schedule_pending_queue(element, &mut state)
                    } else {
                        gst_log!(self.cat, obj: element, "Scheduling pending queue later");

                        None
                    }
                } else {
                    assert!(io_context_in.is_some());
                    pending_queue.as_mut().unwrap().items.push_back(item);

                    None
                }
            } else {
                None
            }
        };

        if let Some(wait_future) = wait_future {
            gst_log!(
                self.cat,
                obj: element,
                "Blocking until queue has space again"
            );
            executor::current_thread::block_on_all(wait_future).map_err(|_| {
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["failed to wait for queue to have space again"]
                );
                gst::FlowError::Error
            })?;
        }

        self.state.lock().unwrap().last_res
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(self.cat, obj: pad, "Handling buffer {:?}", buffer);
        self.enqueue_item(pad, element, DataQueueItem::Buffer(buffer))
    }

    fn sink_chain_list(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
        list: gst::BufferList,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(self.cat, obj: pad, "Handling buffer list {:?}", list);
        self.enqueue_item(pad, element, DataQueueItem::BufferList(list))
    }

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, mut event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        let mut new_event = None;
        match event.view() {
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
                        .expect("signal arg")
                        .expect("missing signal arg");
                    let pending_future_id = s
                        .get::<&PendingFutureId>("pending-future-id")
                        .expect("signal arg")
                        .expect("missing signal arg");

                    gst_debug!(
                        self.cat,
                        obj: element,
                        "Got upstream pending future id {:?}",
                        pending_future_id
                    );

                    state.io_context_in = Some(io_context.clone());
                    state.pending_future_id_in = Some(*pending_future_id);

                    new_event = Self::create_io_context_event(&state);

                    // Get rid of reconfigure flag
                    self.src_pad.check_reconfigure();
                }
            }
            _ => (),
        };

        if let Some(new_event) = new_event {
            event = new_event;
        }

        if event.is_serialized() {
            gst_log!(self.cat, obj: pad, "Queuing event {:?}", event);
            let _ = self.enqueue_item(pad, element, DataQueueItem::Event(event));
            true
        } else {
            gst_log!(self.cat, obj: pad, "Forwarding event {:?}", event);
            self.src_pad.push_event(event)
        }
    }

    fn sink_query(
        &self,
        pad: &gst::Pad,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(self.cat, obj: pad, "Handling query {:?}", query);

        if query.is_serialized() {
            // FIXME: How can we do this?
            gst_log!(self.cat, obj: pad, "Dropping serialized query {:?}", query);
            false
        } else {
            gst_log!(self.cat, obj: pad, "Forwarding query {:?}", query);
            self.src_pad.peer_query(query)
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::FlushStart(..) => {
                let _ = self.stop(element);
            }
            EventView::FlushStop(..) => {
                let (res, state, pending) = element.get_state(0.into());
                if res == Ok(gst::StateChangeSuccess::Success) && state == gst::State::Playing
                    || res == Ok(gst::StateChangeSuccess::Async) && pending == gst::State::Playing
                {
                    let _ = self.start(element);
                }
            }
            _ => (),
        };

        gst_log!(self.cat, obj: pad, "Forwarding event {:?}", event);
        self.sink_pad.push_event(event)
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(self.cat, obj: pad, "Handling query {:?}", query);
        #[allow(clippy::redundant_pattern_matching)]
        #[allow(clippy::single_match)]
        match query.view_mut() {
            QueryView::Scheduling(ref mut q) => {
                let mut new_query = gst::Query::new_scheduling();
                let res = self.sink_pad.peer_query(&mut new_query);
                if !res {
                    return res;
                }

                gst_log!(self.cat, obj: pad, "Upstream returned {:?}", new_query);

                let (flags, min, max, align) = new_query.get_result();
                q.set(flags, min, max, align);
                q.add_scheduling_modes(
                    &new_query
                        .get_scheduling_modes()
                        .iter()
                        .cloned()
                        .filter(|m| m != &gst::PadMode::Pull)
                        .collect::<Vec<_>>(),
                );
                gst_log!(self.cat, obj: pad, "Returning {:?}", q.get_mut_query());
                return true;
            }
            _ => (),
        };

        gst_log!(self.cat, obj: pad, "Forwarding query {:?}", query);
        self.sink_pad.peer_query(query)
    }

    fn push_item(
        &self,
        element: &gst::Element,
        item: DataQueueItem,
    ) -> future::Either<
        Box<dyn Future<Item = (), Error = gst::FlowError> + Send + 'static>,
        future::FutureResult<(), gst::FlowError>,
    > {
        let event = {
            let state = self.state.lock().unwrap();
            if let Some(PendingQueue {
                task: Some(ref task),
                ..
            }) = state.pending_queue
            {
                task.notify();
            }

            if self.src_pad.check_reconfigure() {
                Self::create_io_context_event(&state)
            } else {
                None
            }
        };

        if let Some(event) = event {
            self.src_pad.push_event(event);
        }

        let res = match item {
            DataQueueItem::Buffer(buffer) => {
                gst_log!(self.cat, obj: element, "Forwarding buffer {:?}", buffer);
                self.src_pad.push(buffer).map(|_| ())
            }
            DataQueueItem::BufferList(list) => {
                gst_log!(self.cat, obj: element, "Forwarding buffer list {:?}", list);
                self.src_pad.push_list(list).map(|_| ())
            }
            DataQueueItem::Event(event) => {
                gst_log!(self.cat, obj: element, "Forwarding event {:?}", event);
                self.src_pad.push_event(event);
                Ok(())
            }
        };

        let res = match res {
            Ok(_) => {
                gst_log!(self.cat, obj: element, "Successfully pushed item");
                let mut state = self.state.lock().unwrap();
                state.last_res = Ok(gst::FlowSuccess::Ok);
                Ok(())
            }
            Err(gst::FlowError::Flushing) => {
                gst_debug!(self.cat, obj: element, "Flushing");
                let mut state = self.state.lock().unwrap();
                if let Some(ref queue) = state.queue {
                    queue.pause();
                }
                state.last_res = Err(gst::FlowError::Flushing);
                Ok(())
            }
            Err(gst::FlowError::Eos) => {
                gst_debug!(self.cat, obj: element, "EOS");
                let mut state = self.state.lock().unwrap();
                if let Some(ref queue) = state.queue {
                    queue.pause();
                }
                state.last_res = Err(gst::FlowError::Eos);
                Ok(())
            }
            Err(err) => {
                gst_error!(self.cat, obj: element, "Got error {}", err);
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["streaming stopped, reason {}", err]
                );
                let mut state = self.state.lock().unwrap();
                state.last_res = Err(err);
                Err(gst::FlowError::CustomError)
            }
        };

        match res {
            Ok(()) => {
                let mut state = self.state.lock().unwrap();

                if let State {
                    io_context: Some(ref io_context),
                    pending_future_id: Some(ref pending_future_id),
                    ref mut pending_future_cancel,
                    ..
                } = *state
                {
                    let (cancel, future) = io_context.drain_pending_futures(*pending_future_id);
                    *pending_future_cancel = cancel;

                    future
                } else {
                    future::Either::B(future::ok(()))
                }
            }
            Err(err) => future::Either::B(future::err(err)),
        }
    }

    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(self.cat, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let mut state = self.state.lock().unwrap();

        let io_context =
            IOContext::new(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to create IO context: {}", err]
                )
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
                move |item| {
                    let queue = Self::from_instance(&element_clone);
                    queue.push_item(&element_clone, item)
                },
                move |err| {
                    let queue = Self::from_instance(&element_clone2);
                    gst_error!(queue.cat, obj: &element_clone2, "Got error {}", err);
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
            self.cat,
            obj: element,
            "Got pending future id {:?}",
            pending_future_id
        );

        state.io_context = Some(io_context);
        state.queue = Some(dataqueue);
        state.pending_future_id = Some(pending_future_id);

        gst_debug!(self.cat, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Unpreparing");

        // FIXME: The IO Context has to be alive longer than the queue,
        // otherwise the queue can't finish any remaining work
        let (mut queue, io_context) = {
            let mut state = self.state.lock().unwrap();
            if let (&Some(ref pending_future_id), &Some(ref io_context)) =
                (&state.pending_future_id, &state.io_context)
            {
                io_context.release_pending_future_id(*pending_future_id);
            }

            let queue = state.queue.take();
            let io_context = state.io_context.take();

            *state = State::default();

            (queue, io_context)
        };

        if let Some(ref queue) = queue.take() {
            queue.shutdown();
        }
        drop(io_context);

        gst_debug!(self.cat, obj: element, "Unprepared");
        Ok(())
    }

    fn start(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Starting");
        let mut state = self.state.lock().unwrap();

        if let Some(ref queue) = state.queue {
            queue.unpause();
        }
        state.last_res = Ok(gst::FlowSuccess::Ok);

        gst_debug!(self.cat, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Stopping");
        let mut state = self.state.lock().unwrap();

        if let Some(ref queue) = state.queue {
            queue.pause();
            queue.clear(&self.src_pad);
        }
        if let Some(PendingQueue {
            task: Some(task), ..
        }) = state.pending_queue.take()
        {
            task.notify();
        }
        let _ = state.pending_future_cancel.take();
        state.last_res = Err(gst::FlowError::Flushing);

        gst_debug!(self.cat, obj: element, "Stopped");

        Ok(())
    }
}

impl ObjectSubclass for Queue {
    const NAME: &'static str = "RsTsQueue";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing queue",
            "Generic",
            "Simple data queue",
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

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sink_pad = gst::Pad::new_from_template(&templ, Some("sink"));
        let templ = klass.get_pad_template("src").unwrap();
        let src_pad = gst::Pad::new_from_template(&templ, Some("src"));

        sink_pad.set_chain_function(|pad, parent, buffer| {
            Queue::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |queue, element| queue.sink_chain(pad, element, buffer),
            )
        });
        sink_pad.set_chain_list_function(|pad, parent, list| {
            Queue::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |queue, element| queue.sink_chain_list(pad, element, list),
            )
        });
        sink_pad.set_event_function(|pad, parent, event| {
            Queue::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.sink_event(pad, element, event),
            )
        });
        sink_pad.set_query_function(|pad, parent, query| {
            Queue::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.sink_query(pad, element, query),
            )
        });

        src_pad.set_event_function(|pad, parent, event| {
            Queue::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.src_event(pad, element, event),
            )
        });
        src_pad.set_query_function(|pad, parent, query| {
            Queue::catch_panic_pad_function(
                parent,
                || false,
                |queue, element| queue.src_query(pad, element, query),
            )
        });

        Self {
            cat: gst::DebugCategory::new(
                "ts-queue",
                gst::DebugColorFlags::empty(),
                Some("Thread-sharing queue"),
            ),
            sink_pad,
            src_pad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for Queue {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

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
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

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
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sink_pad).unwrap();
        element.add_pad(&self.src_pad).unwrap();
    }
}

impl ElementImpl for Queue {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(self.cat, obj: element, "Changing state {:?}", transition);

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

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(Some(plugin), "ts-queue", gst::Rank::None, Queue::get_type())
}
