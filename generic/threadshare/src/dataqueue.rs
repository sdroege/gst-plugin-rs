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
//
// SPDX-License-Identifier: LGPL-2.1-or-later

use futures::future::{self, abortable, AbortHandle};

use gst::prelude::*;

use std::sync::LazyLock;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

static DATA_QUEUE_CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ts-dataqueue",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing queue"),
    )
});

#[derive(Debug, Clone)]
pub enum DataQueueItem {
    Buffer(gst::Buffer),
    BufferList(gst::BufferList),
    Event(gst::Event),
}

impl DataQueueItem {
    fn size(&self) -> (u32, u32) {
        match *self {
            DataQueueItem::Buffer(ref buffer) => (1, buffer.size() as u32),
            DataQueueItem::BufferList(ref list) => (
                list.len() as u32,
                list.iter().map(|b| b.size() as u32).sum::<u32>(),
            ),
            DataQueueItem::Event(_) => (0, 0),
        }
    }

    fn timestamp(&self) -> Option<gst::ClockTime> {
        match *self {
            DataQueueItem::Buffer(ref buffer) => buffer.dts_or_pts(),
            DataQueueItem::BufferList(ref list) => list.iter().find_map(|b| b.dts_or_pts()),
            DataQueueItem::Event(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataQueueState {
    Started,
    Stopped,
}

#[derive(Clone, Debug)]
pub struct DataQueue(Arc<StdMutex<DataQueueInner>>);

#[derive(Debug)]
struct DataQueueInner {
    element: gst::Element,
    src_pad: gst::Pad,

    state: DataQueueState,
    queue: VecDeque<DataQueueItem>,

    cur_level_buffers: u32,
    cur_level_bytes: u32,
    cur_level_time: gst::ClockTime,
    max_size_buffers: Option<u32>,
    max_size_bytes: Option<u32>,
    max_size_time: Option<gst::ClockTime>,

    pending_handle: Option<AbortHandle>,
}

impl DataQueueInner {
    fn wake(&mut self) {
        if let Some(pending_handle) = self.pending_handle.take() {
            pending_handle.abort();
        }
    }
}

impl DataQueue {
    pub fn new(
        element: &gst::Element,
        src_pad: &gst::Pad,
        max_size_buffers: Option<u32>,
        max_size_bytes: Option<u32>,
        max_size_time: impl Into<Option<gst::ClockTime>>,
    ) -> DataQueue {
        DataQueue(Arc::new(StdMutex::new(DataQueueInner {
            element: element.clone(),
            src_pad: src_pad.clone(),
            state: DataQueueState::Stopped,
            queue: VecDeque::new(),
            cur_level_buffers: 0,
            cur_level_bytes: 0,
            cur_level_time: gst::ClockTime::ZERO,
            max_size_buffers,
            max_size_bytes,
            max_size_time: max_size_time.into(),
            pending_handle: None,
        })))
    }

    pub fn state(&self) -> DataQueueState {
        self.0.lock().unwrap().state
    }

    pub fn cur_level_buffers(&self) -> u32 {
        self.0.lock().unwrap().cur_level_buffers
    }

    pub fn cur_level_bytes(&self) -> u32 {
        self.0.lock().unwrap().cur_level_bytes
    }

    pub fn cur_level_time(&self) -> gst::ClockTime {
        self.0.lock().unwrap().cur_level_time
    }

    pub fn start(&self) {
        let mut inner = self.0.lock().unwrap();
        if inner.state == DataQueueState::Started {
            gst::debug!(
                DATA_QUEUE_CAT,
                obj = inner.element,
                "Data queue already Started"
            );
            return;
        }
        gst::debug!(DATA_QUEUE_CAT, obj = inner.element, "Starting data queue");
        inner.state = DataQueueState::Started;
        inner.wake();
    }

    pub fn stop(&self) {
        let mut inner = self.0.lock().unwrap();
        if inner.state == DataQueueState::Stopped {
            gst::debug!(
                DATA_QUEUE_CAT,
                obj = inner.element,
                "Data queue already Stopped"
            );
            return;
        }
        gst::debug!(DATA_QUEUE_CAT, obj = inner.element, "Stopping data queue");
        inner.state = DataQueueState::Stopped;
        inner.wake();
    }

    pub fn clear(&self) {
        let mut inner = self.0.lock().unwrap();

        gst::debug!(DATA_QUEUE_CAT, obj = inner.element, "Clearing data queue");

        let src_pad = inner.src_pad.clone();
        for item in inner.queue.drain(..) {
            if let DataQueueItem::Event(event) = item {
                if event.is_sticky()
                    && event.type_() != gst::EventType::Segment
                    && event.type_() != gst::EventType::Eos
                {
                    let _ = src_pad.store_sticky_event(&event);
                }
            }
        }

        inner.cur_level_buffers = 0;
        inner.cur_level_bytes = 0;
        inner.cur_level_time = gst::ClockTime::ZERO;

        gst::debug!(DATA_QUEUE_CAT, obj = inner.element, "Data queue cleared");
    }

    pub fn push(&self, item: DataQueueItem) -> Result<(), DataQueueItem> {
        let mut inner = self.0.lock().unwrap();

        if inner.state == DataQueueState::Stopped {
            gst::debug!(
                DATA_QUEUE_CAT,
                obj = inner.element,
                "Rejecting item {:?} in state {:?}",
                item,
                inner.state
            );
            return Err(item);
        }

        gst::debug!(
            DATA_QUEUE_CAT,
            obj = inner.element,
            "Pushing item {:?}",
            item
        );

        let (count, bytes) = item.size();
        let queue_ts = inner.queue.iter().find_map(|i| i.timestamp());
        let ts = item.timestamp();

        if let Some(max) = inner.max_size_buffers {
            if max <= inner.cur_level_buffers {
                gst::debug!(
                    DATA_QUEUE_CAT,
                    obj = inner.element,
                    "Queue is full (buffers): {} <= {}",
                    max,
                    inner.cur_level_buffers
                );
                return Err(item);
            }
        }

        if let Some(max) = inner.max_size_bytes {
            if max <= inner.cur_level_bytes {
                gst::debug!(
                    DATA_QUEUE_CAT,
                    obj = inner.element,
                    "Queue is full (bytes): {} <= {}",
                    max,
                    inner.cur_level_bytes
                );
                return Err(item);
            }
        }

        // FIXME: Use running time
        let level = if let (Some(queue_ts), Some(ts)) = (queue_ts, ts) {
            let level = if queue_ts > ts {
                queue_ts - ts
            } else {
                ts - queue_ts
            };

            if inner.max_size_time.opt_le(level).unwrap_or(false) {
                gst::debug!(
                    DATA_QUEUE_CAT,
                    obj = inner.element,
                    "Queue is full (time): {} <= {}",
                    inner.max_size_time.display(),
                    level
                );
                return Err(item);
            }

            level
        } else {
            gst::ClockTime::ZERO
        };

        inner.queue.push_back(item);
        inner.cur_level_buffers += count;
        inner.cur_level_bytes += bytes;
        inner.cur_level_time = level;

        inner.wake();

        Ok(())
    }

    // TODO: implement as a Stream now that we use a StdMutex
    #[allow(clippy::should_implement_trait)]
    pub async fn next(&mut self) -> Option<DataQueueItem> {
        loop {
            let pending_fut = {
                let mut inner = self.0.lock().unwrap();
                match inner.state {
                    DataQueueState::Started => match inner.queue.pop_front() {
                        None => {
                            gst::debug!(DATA_QUEUE_CAT, obj = inner.element, "Data queue is empty");
                        }
                        Some(item) => {
                            gst::debug!(
                                DATA_QUEUE_CAT,
                                obj = inner.element,
                                "Popped item {:?}",
                                item
                            );

                            let (count, bytes) = item.size();
                            inner.cur_level_buffers -= count;
                            inner.cur_level_bytes -= bytes;

                            return Some(item);
                        }
                    },
                    DataQueueState::Stopped => {
                        gst::debug!(DATA_QUEUE_CAT, obj = inner.element, "Data queue Stopped");
                        return None;
                    }
                }

                let (pending_fut, abort_handle) = abortable(future::pending::<()>());
                inner.pending_handle = Some(abort_handle);

                pending_fut
            };

            let _ = pending_fut.await;
        }
    }
}
