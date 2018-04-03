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

use gst;
use gst::prelude::*;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::{u32, u64};

use futures::sync::oneshot;
use futures::task;
use futures::{Async, Future, IntoFuture, Poll, Stream};

use iocontext::*;

lazy_static! {
    static ref DATA_QUEUE_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-dataqueue",
        gst::DebugColorFlags::empty(),
        "Thread-sharing queue",
    );
}

#[derive(Debug)]
pub enum DataQueueItem {
    Buffer(gst::Buffer),
    BufferList(gst::BufferList),
    Event(gst::Event),
}

impl DataQueueItem {
    fn size(&self) -> (u32, u32) {
        match *self {
            DataQueueItem::Buffer(ref buffer) => (1, buffer.get_size() as u32),
            DataQueueItem::BufferList(ref list) => (
                list.len() as u32,
                list.iter().map(|b| b.get_size() as u32).sum::<u32>(),
            ),
            DataQueueItem::Event(_) => (0, 0),
        }
    }

    fn timestamp(&self) -> Option<u64> {
        match *self {
            DataQueueItem::Buffer(ref buffer) => buffer.get_dts_or_pts().0,
            DataQueueItem::BufferList(ref list) => {
                list.iter().filter_map(|b| b.get_dts_or_pts().0).next()
            }
            DataQueueItem::Event(_) => None,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum DataQueueState {
    Unscheduled,
    Scheduled,
    Running,
    Shutdown,
}

#[derive(Clone)]
pub struct DataQueue(Arc<Mutex<DataQueueInner>>);

struct DataQueueInner {
    element: gst::Element,

    state: DataQueueState,
    queue: VecDeque<DataQueueItem>,

    cur_size_buffers: u32,
    cur_size_bytes: u32,
    max_size_buffers: Option<u32>,
    max_size_bytes: Option<u32>,
    max_size_time: Option<u64>,

    current_task: Option<task::Task>,
    shutdown_receiver: Option<oneshot::Receiver<()>>,
}

impl DataQueue {
    pub fn new(
        element: &gst::Element,
        max_size_buffers: Option<u32>,
        max_size_bytes: Option<u32>,
        max_size_time: Option<u64>,
    ) -> DataQueue {
        DataQueue(Arc::new(Mutex::new(DataQueueInner {
            element: element.clone(),
            state: DataQueueState::Unscheduled,
            queue: VecDeque::new(),
            cur_size_buffers: 0,
            cur_size_bytes: 0,
            max_size_buffers: max_size_buffers,
            max_size_bytes: max_size_bytes,
            max_size_time: max_size_time,
            current_task: None,
            shutdown_receiver: None,
        })))
    }

    pub fn schedule<U, F, G>(&self, io_context: &IOContext, func: F, err_func: G) -> Result<(), ()>
    where
        F: Fn(DataQueueItem) -> U + Send + 'static,
        U: IntoFuture<Item = (), Error = gst::FlowError> + 'static,
        <U as IntoFuture>::Future: Send + 'static,
        G: FnOnce(gst::FlowError) + Send + 'static,
    {
        // Ready->Paused
        //
        // Need to wait for a possible shutdown to finish first
        // spawn() on the reactor, change state to Scheduled

        let mut inner = self.0.lock().unwrap();
        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Scheduling data queue");
        if inner.state == DataQueueState::Scheduled {
            gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue already scheduled");
            return Ok(());
        }

        assert_eq!(inner.state, DataQueueState::Unscheduled);
        inner.state = DataQueueState::Scheduled;

        let (sender, receiver) = oneshot::channel::<()>();
        inner.shutdown_receiver = Some(receiver);

        let queue_clone = self.clone();
        let element_clone = inner.element.clone();
        io_context.spawn(queue_clone.for_each(func).then(move |res| {
            gst_debug!(
                DATA_QUEUE_CAT,
                obj: &element_clone,
                "Data queue finished: {:?}",
                res
            );

            if let Err(err) = res {
                err_func(err);
            }

            let _ = sender.send(());

            Ok(())
        }));
        Ok(())
    }

    pub fn unpause(&self) {
        // Paused->Playing
        //
        // Change state to Running and signal task
        let mut inner = self.0.lock().unwrap();
        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Unpausing data queue");
        if inner.state == DataQueueState::Running {
            gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue already unpaused");
            return;
        }

        assert_eq!(inner.state, DataQueueState::Scheduled);
        inner.state = DataQueueState::Running;

        if let Some(task) = inner.current_task.take() {
            task.notify();
        }
    }

    pub fn pause(&self) {
        // Playing->Paused
        //
        // Change state to Scheduled and signal task

        let mut inner = self.0.lock().unwrap();
        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Pausing data queue");
        if inner.state == DataQueueState::Scheduled {
            gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue already paused");
            return;
        }

        assert_eq!(inner.state, DataQueueState::Running);
        inner.state = DataQueueState::Scheduled;

        if let Some(task) = inner.current_task.take() {
            task.notify();
        }
    }

    pub fn shutdown(&self) {
        // Paused->Ready
        //
        // Change state to Shutdown and signal task, wait for our future to be finished
        // Requires scheduled function to be unblocked! Pad must be deactivated before

        let mut inner = self.0.lock().unwrap();
        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Shutting down data queue");
        if inner.state == DataQueueState::Unscheduled {
            gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue already shut down");
            return;
        }

        assert!(inner.state == DataQueueState::Scheduled || inner.state == DataQueueState::Running);
        inner.state = DataQueueState::Shutdown;

        if let Some(task) = inner.current_task.take() {
            task.notify();
        }

        let shutdown_receiver = inner.shutdown_receiver.take().unwrap();
        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Waiting for data queue to shut down");
        drop(inner);

        shutdown_receiver.wait().expect("Already shut down");

        let mut inner = self.0.lock().unwrap();
        inner.state = DataQueueState::Unscheduled;
        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue shut down");
    }

    pub fn clear(&self, src_pad: &gst::Pad) {
        let mut inner = self.0.lock().unwrap();

        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Clearing queue");
        for item in inner.queue.drain(..) {
            if let DataQueueItem::Event(event) = item {
                if event.is_sticky() && event.get_type() != gst::EventType::Segment
                    && event.get_type() != gst::EventType::Eos
                {
                    let _ = src_pad.store_sticky_event(&event);
                }
            }
        }
    }

    pub fn push(&self, item: DataQueueItem) -> Result<(), DataQueueItem> {
        let mut inner = self.0.lock().unwrap();

        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Pushing item {:?}", item);

        let (count, bytes) = item.size();
        let queue_ts = inner.queue.iter().filter_map(|i| i.timestamp()).next();
        let ts = item.timestamp();

        if let Some(max) = inner.max_size_buffers {
            if max <= inner.cur_size_buffers {
                gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Queue is full (buffers): {} <= {}", max, inner.cur_size_buffers);
                return Err(item);
            }
        }

        if let Some(max) = inner.max_size_bytes {
            if max <= inner.cur_size_bytes {
                gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Queue is full (bytes): {} <= {}", max, inner.cur_size_bytes);
                return Err(item);
            }
        }

        // FIXME: Use running time
        if let (Some(max), Some(queue_ts), Some(ts)) = (inner.max_size_time, queue_ts, ts) {
            let level = if queue_ts > ts {
                queue_ts - ts
            } else {
                ts - queue_ts
            };

            if max <= level {
                gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Queue is full (time): {} <= {}", max, level);
                return Err(item);
            }
        }

        inner.queue.push_back(item);
        inner.cur_size_buffers += count;
        inner.cur_size_bytes += bytes;

        if let Some(task) = inner.current_task.take() {
            task.notify();
        }

        Ok(())
    }
}

impl Drop for DataQueueInner {
    fn drop(&mut self) {
        assert_eq!(self.state, DataQueueState::Unscheduled);
    }
}

impl Stream for DataQueue {
    type Item = DataQueueItem;
    type Error = gst::FlowError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut inner = self.0.lock().unwrap();
        if inner.state == DataQueueState::Shutdown {
            gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue shutting down");
            return Ok(Async::Ready(None));
        } else if inner.state == DataQueueState::Scheduled {
            gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue not running");
            inner.current_task = Some(task::current());
            return Ok(Async::NotReady);
        }

        assert_eq!(inner.state, DataQueueState::Running);

        gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Trying to read data");
        match inner.queue.pop_front() {
            None => {
                gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Data queue is empty");
                inner.current_task = Some(task::current());
                Ok(Async::NotReady)
            }
            Some(item) => {
                gst_debug!(DATA_QUEUE_CAT, obj: &inner.element, "Popped item {:?}", item);

                let (count, bytes) = item.size();
                inner.cur_size_buffers -= count;
                inner.cur_size_bytes -= bytes;

                Ok(Async::Ready(Some(item)))
            }
        }
    }
}
