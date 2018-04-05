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
use gst;
use gst::prelude::*;

use gst_plugin::element::*;
use gst_plugin::object::*;
use gst_plugin::properties::*;

use std::collections::VecDeque;
use std::sync::Mutex;
use std::{u16, u32, u64};

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
const DEFAULT_CONTEXT: &'static str = "";
const DEFAULT_CONTEXT_THREADS: i32 = 0;
const DEFAULT_CONTEXT_WAIT: u32 = 0;

#[derive(Debug, Clone)]
struct Settings {
    max_size_buffers: u32,
    max_size_bytes: u32,
    max_size_time: u64,
    context: String,
    context_threads: i32,
    context_wait: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            max_size_buffers: DEFAULT_MAX_SIZE_BUFFERS,
            max_size_bytes: DEFAULT_MAX_SIZE_BYTES,
            max_size_time: DEFAULT_MAX_SIZE_TIME,
            context: DEFAULT_CONTEXT.into(),
            context_threads: DEFAULT_CONTEXT_THREADS,
            context_wait: DEFAULT_CONTEXT_WAIT,
        }
    }
}

static PROPERTIES: [Property; 6] = [
    Property::UInt(
        "max-size-buffers",
        "Max Size Buffers",
        "Maximum number of buffers to queue (0=unlimited)",
        (0, u32::MAX),
        DEFAULT_MAX_SIZE_BUFFERS,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "max-size-bytes",
        "Max Size Bytes",
        "Maximum number of bytes to queue (0=unlimited)",
        (0, u32::MAX),
        DEFAULT_MAX_SIZE_BYTES,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt64(
        "max-size-time",
        "Max Size Time",
        "Maximum number of nanoseconds to queue (0=unlimited)",
        (0, u64::MAX - 1),
        DEFAULT_MAX_SIZE_TIME,
        PropertyMutability::ReadWrite,
    ),
    Property::String(
        "context",
        "Context",
        "Context name to share threads with",
        Some(DEFAULT_CONTEXT),
        PropertyMutability::ReadWrite,
    ),
    Property::Int(
        "context-threads",
        "Context Threads",
        "Number of threads for the context thread-pool if we create it",
        (-1, u16::MAX as i32),
        DEFAULT_CONTEXT_THREADS,
        PropertyMutability::ReadWrite,
    ),
    Property::UInt(
        "context-wait",
        "Context Wait",
        "Throttle poll loop to run at most once every this many ms",
        (0, 1000),
        DEFAULT_CONTEXT_WAIT,
        PropertyMutability::ReadWrite,
    ),
];

struct State {
    io_context: Option<IOContext>,
    pending_future_id: Option<PendingFutureId>,
    io_context_in: Option<IOContext>,
    pending_future_id_in: Option<PendingFutureId>,
    queue: Option<DataQueue>,
    pending_queue: Option<(Option<task::Task>, VecDeque<DataQueueItem>)>,
    last_ret: gst::FlowReturn,
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
            last_ret: gst::FlowReturn::Ok,
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
    fn class_init(klass: &mut ElementClass) {
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
        );
        klass.add_pad_template(sink_pad_template);

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        );
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }

    fn init(element: &Element) -> Box<ElementImpl<Element>> {
        let templ = element.get_pad_template("sink").unwrap();
        let sink_pad = gst::Pad::new_from_template(&templ, "sink");
        let templ = element.get_pad_template("src").unwrap();
        let src_pad = gst::Pad::new_from_template(&templ, "src");

        sink_pad.set_chain_function(|pad, parent, buffer| {
            Queue::catch_panic_pad_function(
                parent,
                || gst::FlowReturn::Error,
                |queue, element| queue.sink_chain(pad, element, buffer),
            )
        });
        sink_pad.set_chain_list_function(|pad, parent, list| {
            Queue::catch_panic_pad_function(
                parent,
                || gst::FlowReturn::Error,
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
        element.add_pad(&sink_pad).unwrap();
        element.add_pad(&src_pad).unwrap();

        Box::new(Self {
            cat: gst::DebugCategory::new(
                "ts-queue",
                gst::DebugColorFlags::empty(),
                "Thread-sharing queue",
            ),
            sink_pad: sink_pad,
            src_pad: src_pad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        })
    }

    fn create_io_context_event(state: &State) -> Option<gst::Event> {
        if let (&Some(ref pending_future_id), &Some(ref io_context)) =
            (&state.pending_future_id, &state.io_context)
        {
            let s = gst::Structure::new(
                "ts-io-context",
                &[
                    ("io-context", &glib::AnySendValue::new(io_context.clone())),
                    (
                        "pending-future-id",
                        &glib::AnySendValue::new(*pending_future_id),
                    ),
                ],
            );
            Some(gst::Event::new_custom_downstream_sticky(s).build())
        } else {
            None
        }
    }

    fn enqueue_item(
        &self,
        _pad: &gst::Pad,
        element: &Element,
        item: DataQueueItem,
    ) -> gst::FlowReturn {
        let wait_future = {
            let mut state = self.state.lock().unwrap();
            let State {
                ref queue,
                ref mut pending_queue,
                ref io_context_in,
                pending_future_id_in,
                ..
            } = *state;
            let queue = match *queue {
                None => return gst::FlowReturn::Error,
                Some(ref queue) => queue,
            };

            let item = if pending_queue.is_none() {
                queue.push(item)
            } else {
                Err(item)
            };
            if let Err(item) = item {
                if pending_queue.is_none() {
                    *pending_queue = Some((None, VecDeque::new()));
                    pending_queue.as_mut().unwrap().1.push_back(item);

                    gst_log!(
                        self.cat,
                        obj: element,
                        "Queue is full - Pushing first item on pending queue"
                    );

                    let element_clone = element.clone();
                    let future = future::poll_fn(move || {
                        let queue = element_clone.get_impl().downcast_ref::<Queue>().unwrap();
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
                        let res = if let Some((ref mut task, ref mut items)) = *pending_queue {
                            let mut failed_item = None;
                            for item in items.drain(..) {
                                if let Err(item) = dq.as_ref().unwrap().push(item) {
                                    failed_item = Some(item);
                                    break;
                                }
                            }

                            if let Some(item) = failed_item {
                                items.push_front(item);
                                *task = Some(task::current());
                                gst_log!(
                                    queue.cat,
                                    obj: &element_clone,
                                    "Waiting for more queue space"
                                );
                                Ok(Async::NotReady)
                            } else {
                                gst_log!(
                                    queue.cat,
                                    obj: &element_clone,
                                    "Pending queue is empty now"
                                );
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
                } else {
                    assert!(io_context_in.is_some());
                    pending_queue.as_mut().unwrap().1.push_back(item);

                    None
                }
            } else {
                None
            }
        };

        if let Some(wait_future) = wait_future {
            gst_log!(self.cat, obj: element, "Blocking until queue becomes empty");
            match executor::current_thread::block_on_all(wait_future) {
                Err(_) => {
                    gst_element_error!(
                        element,
                        gst::StreamError::Failed,
                        ["failed to wait for queue to become empty again"]
                    );
                    return gst::FlowReturn::Error;
                }
                Ok(_) => (),
            }
        }

        self.state.lock().unwrap().last_ret
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &Element,
        buffer: gst::Buffer,
    ) -> gst::FlowReturn {
        gst_log!(self.cat, obj: pad, "Handling buffer {:?}", buffer);
        self.enqueue_item(pad, element, DataQueueItem::Buffer(buffer))
    }

    fn sink_chain_list(
        &self,
        pad: &gst::Pad,
        element: &Element,
        list: gst::BufferList,
    ) -> gst::FlowReturn {
        gst_log!(self.cat, obj: pad, "Handling buffer list {:?}", list);
        self.enqueue_item(pad, element, DataQueueItem::BufferList(list))
    }

    fn sink_event(&self, pad: &gst::Pad, element: &Element, mut event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        let mut new_event = None;
        match event.view() {
            EventView::FlushStart(..) => {
                let _ = self.stop(element);
            }
            EventView::FlushStop(..) => {
                let (ret, state, pending) = element.get_state(0.into());
                if ret == gst::StateChangeReturn::Success && state == gst::State::Paused
                    || ret == gst::StateChangeReturn::Async && pending == gst::State::Paused
                {
                    let _ = self.start(element);
                }
            }
            EventView::CustomDownstreamSticky(e) => {
                let s = e.get_structure().unwrap();
                if s.get_name() == "ts-io-context" {
                    let mut state = self.state.lock().unwrap();
                    let io_context = s.get::<&glib::AnySendValue>("io-context").unwrap();
                    let io_context = io_context.downcast_ref::<IOContext>().unwrap();
                    let pending_future_id =
                        s.get::<&glib::AnySendValue>("pending-future-id").unwrap();
                    let pending_future_id =
                        pending_future_id.downcast_ref::<PendingFutureId>().unwrap();

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

    fn sink_query(&self, pad: &gst::Pad, _element: &Element, query: &mut gst::QueryRef) -> bool {
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

    fn src_event(&self, pad: &gst::Pad, element: &Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::FlushStart(..) => {
                let _ = self.stop(element);
            }
            EventView::FlushStop(..) => {
                let (ret, state, pending) = element.get_state(0.into());
                if ret == gst::StateChangeReturn::Success && state == gst::State::Playing
                    || ret == gst::StateChangeReturn::Async && pending == gst::State::Playing
                {
                    let _ = self.start(element);
                }
            }
            _ => (),
        };

        gst_log!(self.cat, obj: pad, "Forwarding event {:?}", event);
        self.sink_pad.push_event(event)
    }

    fn src_query(&self, pad: &gst::Pad, _element: &Element, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        gst_log!(self.cat, obj: pad, "Handling query {:?}", query);
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
                q.add_scheduling_modes(&new_query
                    .get_scheduling_modes()
                    .iter()
                    .cloned()
                    .filter(|m| m != &gst::PadMode::Pull)
                    .collect::<Vec<_>>());
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
        element: &Element,
        item: DataQueueItem,
    ) -> future::Either<
        Box<Future<Item = (), Error = gst::FlowError> + Send + 'static>,
        future::FutureResult<(), gst::FlowError>,
    > {
        let event = {
            let state = self.state.lock().unwrap();
            if let Some((Some(ref task), _)) = state.pending_queue {
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
                self.src_pad.push(buffer).into_result().map(|_| ())
            }
            DataQueueItem::BufferList(list) => {
                gst_log!(self.cat, obj: element, "Forwarding buffer list {:?}", list);
                self.src_pad.push_list(list).into_result().map(|_| ())
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
                state.last_ret = gst::FlowReturn::Ok;
                Ok(())
            }
            Err(gst::FlowError::Flushing) => {
                gst_debug!(self.cat, obj: element, "Flushing");
                let mut state = self.state.lock().unwrap();
                if let Some(ref queue) = state.queue {
                    queue.pause();
                }
                state.last_ret = gst::FlowReturn::Flushing;
                Ok(())
            }
            Err(gst::FlowError::Eos) => {
                gst_debug!(self.cat, obj: element, "EOS");
                let mut state = self.state.lock().unwrap();
                if let Some(ref queue) = state.queue {
                    queue.pause();
                }
                state.last_ret = gst::FlowReturn::Eos;
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
                state.last_ret = gst::FlowReturn::from_error(err);
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

    fn prepare(&self, element: &Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(self.cat, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();

        let mut state = self.state.lock().unwrap();

        let io_context = IOContext::new(
            &settings.context,
            settings.context_threads as isize,
            settings.context_wait,
        ).map_err(|err| {
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
                    let queue = element_clone.get_impl().downcast_ref::<Queue>().unwrap();
                    queue.push_item(&element_clone, item)
                },
                move |err| {
                    let queue = element_clone2.get_impl().downcast_ref::<Queue>().unwrap();
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
            })?;;

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

    fn unprepare(&self, element: &Element) -> Result<(), ()> {
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

    fn start(&self, element: &Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Starting");
        let mut state = self.state.lock().unwrap();

        if let Some(ref queue) = state.queue {
            queue.unpause();
        }
        state.last_ret = gst::FlowReturn::Ok;

        gst_debug!(self.cat, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &Element) -> Result<(), ()> {
        gst_debug!(self.cat, obj: element, "Stopping");
        let mut state = self.state.lock().unwrap();

        if let Some(ref queue) = state.queue {
            queue.pause();
            queue.clear(&self.src_pad);
        }
        if let Some((Some(task), _)) = state.pending_queue.take() {
            task.notify();
        }
        let _ = state.pending_future_cancel.take();
        state.last_ret = gst::FlowReturn::Flushing;

        gst_debug!(self.cat, obj: element, "Stopped");

        Ok(())
    }
}

impl ObjectImpl<Element> for Queue {
    fn set_property(&self, _obj: &glib::Object, id: u32, value: &glib::Value) {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::UInt("max-size-buffers", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_size_buffers = value.get().unwrap();
            }
            Property::UInt("max-size-bytes", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_size_bytes = value.get().unwrap();
            }
            Property::UInt64("max-size-time", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_size_time = value.get().unwrap();
            }
            Property::String("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context = value.get().unwrap_or_else(|| "".into());
            }
            Property::Int("context-threads", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_threads = value.get().unwrap();
            }
            Property::UInt("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_wait = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::UInt("max-size-buffers", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.max_size_buffers.to_value())
            }
            Property::UInt("max-size-bytes", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.max_size_bytes.to_value())
            }
            Property::UInt64("max-size-time", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.max_size_time.to_value())
            }
            Property::String("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context.to_value())
            }
            Property::Int("context-threads", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context_threads.to_value())
            }
            Property::UInt("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                Ok(settings.context_wait.to_value())
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl<Element> for Queue {
    fn change_state(
        &self,
        element: &Element,
        transition: gst::StateChange,
    ) -> gst::StateChangeReturn {
        gst_trace!(self.cat, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => match self.prepare(element) {
                Err(err) => {
                    element.post_error_message(&err);
                    return gst::StateChangeReturn::Failure;
                }
                Ok(_) => (),
            },
            gst::StateChange::PausedToReady => match self.stop(element) {
                Err(_) => return gst::StateChangeReturn::Failure,
                Ok(_) => (),
            },
            gst::StateChange::ReadyToNull => match self.unprepare(element) {
                Err(_) => return gst::StateChangeReturn::Failure,
                Ok(_) => (),
            },
            _ => (),
        }

        let ret = element.parent_change_state(transition);
        if ret == gst::StateChangeReturn::Failure {
            return ret;
        }

        match transition {
            gst::StateChange::ReadyToPaused => match self.start(element) {
                Err(_) => return gst::StateChangeReturn::Failure,
                Ok(_) => (),
            },
            _ => (),
        }

        ret
    }
}

struct QueueStatic;

impl ImplTypeStatic<Element> for QueueStatic {
    fn get_name(&self) -> &str {
        "Queue"
    }

    fn new(&self, element: &Element) -> Box<ElementImpl<Element>> {
        Queue::init(element)
    }

    fn class_init(&self, klass: &mut ElementClass) {
        Queue::class_init(klass);
    }
}

pub fn register(plugin: &gst::Plugin) {
    let queue_static = QueueStatic;
    let type_ = register_type(queue_static);
    gst::Element::register(plugin, "ts-queue", 0, type_);
}
