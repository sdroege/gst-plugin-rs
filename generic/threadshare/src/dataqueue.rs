// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use futures::future::{self, AbortHandle, abortable};

use gst::prelude::*;

use std::sync::LazyLock;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use crate::runtime::Context;

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
    fn sizes(&self) -> (u32, u32, Option<gst::ClockTime>) {
        match *self {
            DataQueueItem::Buffer(ref buffer) => (1, buffer.size() as u32, buffer.dts_or_pts()),
            DataQueueItem::BufferList(ref list) => {
                let (size, ts) = list
                    .iter()
                    .fold((0, gst::ClockTime::NONE), |(size, first_ts), buf| {
                        (size + buf.size(), first_ts.or(buf.dts_or_pts()))
                    });
                (list.len() as u32, size as u32, ts)
            }
            DataQueueItem::Event(_) => (0, 0, None),
        }
    }

    fn timestamp(&self) -> Option<gst::ClockTime> {
        match *self {
            DataQueueItem::Buffer(ref buffer) => buffer.dts_or_pts(),
            DataQueueItem::BufferList(ref list) => list.iter().find_map(|b| b.dts_or_pts()),
            DataQueueItem::Event(_) => None,
        }
    }

    fn last_timestamp(&self) -> Option<gst::ClockTime> {
        match *self {
            DataQueueItem::Buffer(ref buffer) => buffer.dts_or_pts(),
            DataQueueItem::BufferList(ref list) => list.iter().rev().find_map(|b| b.dts_or_pts()),
            DataQueueItem::Event(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataQueueState {
    Started,
    Stopped,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PushedStatus {
    FirstBuffer,
    GotBuffers,
    #[default]
    PendingBuffers,
}

impl PushedStatus {
    pub fn is_first_buffer(self) -> bool {
        matches!(self, PushedStatus::FirstBuffer)
    }
}

#[derive(Clone, Debug)]
pub struct DataQueue(Arc<Mutex<DataQueueInner>>);

#[derive(Debug)]
struct DataQueueInner {
    element: gst::Element,
    upstream_ctx: Option<Context>,
    src_pad: gst::Pad,

    state: DataQueueState,
    queue: VecDeque<DataQueueItem>,

    pushed_status: PushedStatus,

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

    fn update_cur_time_level(&mut self) {
        if let Some((first_ts, last_ts)) = Option::zip(
            self.queue.iter().find_map(|i| i.timestamp()),
            self.queue.iter().rev().find_map(|i| i.last_timestamp()),
        ) {
            self.cur_level_time = if last_ts >= first_ts {
                last_ts - first_ts
            } else {
                first_ts - last_ts
            };
        } else {
            self.cur_level_time = gst::ClockTime::ZERO;
        }
    }
}

impl DataQueue {
    pub fn builder(element: &gst::Element, src_pad: &gst::Pad) -> DataQueueBuilder {
        DataQueueBuilder::new(element, src_pad)
    }

    pub fn state(&self) -> DataQueueState {
        self.0.lock().unwrap().state
    }

    pub fn upstream_context(&self) -> Option<Context> {
        self.0.lock().unwrap().upstream_ctx.clone()
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

    pub fn is_empty(&self) -> bool {
        self.0.lock().unwrap().queue.is_empty()
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
            if let DataQueueItem::Event(event) = item
                && event.is_sticky()
                && event.type_() != gst::EventType::Segment
                && event.type_() != gst::EventType::Eos
            {
                let _ = src_pad.store_sticky_event(&event);
            }
        }

        inner.pushed_status = PushedStatus::default();

        inner.cur_level_buffers = 0;
        inner.cur_level_bytes = 0;
        inner.cur_level_time = gst::ClockTime::ZERO;

        gst::debug!(DATA_QUEUE_CAT, obj = inner.element, "Data queue cleared");
    }

    pub fn push(
        &self,
        obj: &gst::glib::Object,
        item: DataQueueItem,
    ) -> Result<PushedStatus, DataQueueItem> {
        let mut inner = self.0.lock().unwrap();

        if inner.state == DataQueueState::Stopped {
            gst::debug!(
                DATA_QUEUE_CAT,
                obj = obj,
                "Rejecting item {item:?} in state {:?}",
                inner.state
            );
            return Err(item);
        }

        let (buffer_count, bytes, ts) = item.sizes();

        if buffer_count > 0 {
            if let Some(max) = inner.max_size_buffers
                && max <= inner.cur_level_buffers
            {
                gst::debug!(
                    DATA_QUEUE_CAT,
                    obj = obj,
                    "Queue is full (buffers): {max} <= {}, {item:?}",
                    inner.cur_level_buffers,
                );
                return Err(item);
            }

            if let Some(max) = inner.max_size_bytes
                && max <= inner.cur_level_bytes
            {
                gst::debug!(
                    DATA_QUEUE_CAT,
                    obj = obj,
                    "Queue is full (bytes): {max} <= {}, {item:?}",
                    inner.cur_level_bytes,
                );
                return Err(item);
            }

            if ts.is_some() {
                // FIXME: Use running time
                if let Some(max) = inner.max_size_time
                    && max <= inner.cur_level_time
                {
                    gst::debug!(
                        DATA_QUEUE_CAT,
                        obj = obj,
                        "Queue is full (time): {max} <= {}, {item:?}",
                        inner.cur_level_time,
                    );

                    return Err(item);
                }
            }
        }

        gst::debug!(DATA_QUEUE_CAT, obj = obj, "Pushing item {item:?}");

        use PushedStatus::*;
        match inner.pushed_status {
            GotBuffers => (),
            FirstBuffer => inner.pushed_status = GotBuffers,
            PendingBuffers => {
                if let DataQueueItem::Buffer(_) | DataQueueItem::BufferList(_) = item {
                    inner.pushed_status = PushedStatus::FirstBuffer;
                    inner.upstream_ctx = Context::current();
                }
            }
        }

        inner.queue.push_back(item);

        inner.cur_level_buffers += buffer_count;
        inner.cur_level_bytes += bytes;
        if ts.is_some() {
            inner.update_cur_time_level();
        }

        inner.wake();

        Ok(inner.pushed_status)
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

                            let (buffer_count, bytes, ts) = item.sizes();
                            if buffer_count > 0 {
                                inner.cur_level_buffers -= buffer_count;
                                inner.cur_level_bytes -= bytes;

                                if ts.is_some() {
                                    inner.update_cur_time_level();
                                }
                            }

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

#[derive(Debug)]
pub struct DataQueueBuilder(DataQueueInner);

impl DataQueueBuilder {
    fn new(element: &gst::Element, src_pad: &gst::Pad) -> DataQueueBuilder {
        DataQueueBuilder(DataQueueInner {
            element: element.clone(),
            upstream_ctx: None,
            src_pad: src_pad.clone(),
            state: DataQueueState::Stopped,
            queue: VecDeque::new(),
            pushed_status: PushedStatus::default(),
            cur_level_buffers: 0,
            cur_level_bytes: 0,
            cur_level_time: gst::ClockTime::ZERO,
            max_size_buffers: None,
            max_size_bytes: None,
            max_size_time: None,
            pending_handle: None,
        })
    }

    pub fn max_size_buffers(mut self, max_size_buffers: u32) -> Self {
        self.0.max_size_buffers = (max_size_buffers > 0).then_some(max_size_buffers);
        self
    }

    pub fn max_size_bytes(mut self, max_size_bytes: u32) -> Self {
        self.0.max_size_bytes = (max_size_bytes > 0).then_some(max_size_bytes);
        self
    }

    pub fn max_size_time(mut self, max_size_time: gst::ClockTime) -> Self {
        self.0.max_size_time = (!max_size_time.is_zero()).then_some(max_size_time);
        self
    }

    pub fn build(self) -> DataQueue {
        DataQueue(Arc::new(Mutex::new(self.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init(test: &str) -> (gst::Element, gst::Pad) {
        use std::sync::Once;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            gst::init().unwrap();
        });

        let elem = gst::ElementFactory::make("fakesrc")
            .name(test)
            .build()
            .unwrap();
        let src_pad = elem.static_pad("src").unwrap();

        (elem, src_pad)
    }

    #[track_caller]
    fn push_initial_events(elem: &gst::Element, dq: &DataQueue) {
        dq.push(
            elem.upcast_ref(),
            DataQueueItem::Event(
                gst::event::Caps::builder(&gst::Caps::new_empty_simple("test/raw")).build(),
            ),
        )
        .unwrap();
        dq.push(
            elem.upcast_ref(),
            DataQueueItem::Event(gst::event::StreamStart::builder(&elem.name()).build()),
        )
        .unwrap();
        dq.push(
            elem.upcast_ref(),
            DataQueueItem::Event(
                gst::event::Segment::builder(&gst::FormattedSegment::<gst::format::Time>::new())
                    .build(),
            ),
        )
        .unwrap();
    }

    #[track_caller]
    fn push_segment_done(elem: &gst::Element, dq: &DataQueue) {
        dq.push(
            elem.upcast_ref(),
            DataQueueItem::Event(gst::event::SegmentDone::builder(2.seconds()).build()),
        )
        .unwrap();
    }

    fn pop_item(dq: &mut DataQueue) -> Option<DataQueueItem> {
        futures::executor::block_on(dq.next())
    }

    #[track_caller]
    fn pop_event(dq: &mut DataQueue) -> gst::Event {
        match pop_item(dq) {
            Some(DataQueueItem::Event(evt)) => evt,
            other => panic!("Unexpected {other:?}"),
        }
    }

    #[track_caller]
    fn pop_intial_events(dq: &mut DataQueue) {
        assert_eq!(pop_event(dq).type_(), gst::EventType::Caps);
        assert_eq!(pop_event(dq).type_(), gst::EventType::StreamStart);
        assert_eq!(pop_event(dq).type_(), gst::EventType::Segment);
    }

    #[track_caller]
    fn pop_segment_done(dq: &mut DataQueue) {
        assert_eq!(pop_event(dq).type_(), gst::EventType::SegmentDone);
    }

    fn push_buffer(
        elem: &gst::Element,
        dq: &DataQueue,
        ts: gst::ClockTime,
    ) -> Result<PushedStatus, DataQueueItem> {
        let mut buf = gst::Buffer::from_slice([0u8]);
        {
            let buf_mut = buf.make_mut();
            buf_mut.set_pts(ts);
        }

        dq.push(elem.upcast_ref(), DataQueueItem::Buffer(buf))
    }

    #[track_caller]
    fn pop_buffer(dq: &mut DataQueue) -> gst::Buffer {
        match pop_item(dq) {
            Some(DataQueueItem::Buffer(buf)) => buf,
            other => panic!("Unexpected {other:?}"),
        }
    }

    fn push_buffer_list(
        elem: &gst::Element,
        dq: &DataQueue,
        ts: gst::ClockTime,
    ) -> Result<PushedStatus, DataQueueItem> {
        let mut buf1 = gst::Buffer::from_slice([0u8]);
        {
            let buf_mut = buf1.make_mut();
            buf_mut.set_pts(ts);
        }
        let mut buf2 = gst::Buffer::from_slice([0u8]);
        {
            let buf_mut = buf2.make_mut();
            buf_mut.set_pts(ts + 1.seconds());
        }

        dq.push(
            elem.upcast_ref(),
            DataQueueItem::BufferList(gst::BufferList::from([buf1, buf2])),
        )
    }

    #[track_caller]
    fn pop_buffer_list(dq: &mut DataQueue) -> gst::BufferList {
        match pop_item(dq) {
            Some(DataQueueItem::BufferList(buf_list)) => buf_list,
            other => panic!("Unexpected {other:?}"),
        }
    }

    fn test_not_leaky(elem: &gst::Element, dq: &mut DataQueue) {
        dq.start();

        // Buffers
        push_initial_events(elem, dq);
        assert_eq!(dq.cur_level_buffers(), 0);
        assert_eq!(dq.cur_level_bytes(), 0);
        assert_eq!(dq.cur_level_time(), 0.seconds());

        push_buffer(elem, dq, 0.seconds()).unwrap();
        push_buffer(elem, dq, 1.seconds()).unwrap();
        assert_eq!(dq.cur_level_buffers(), 2);
        assert_eq!(dq.cur_level_bytes(), 2);
        assert_eq!(dq.cur_level_time(), 1.seconds());
        let rejected_buf = match push_buffer(elem, dq, 2.seconds()).unwrap_err() {
            DataQueueItem::Buffer(buf) => buf,
            other => panic!("Unexpected {other:?}"),
        };
        assert_eq!(rejected_buf.pts(), Some(2.seconds()));
        assert_eq!(dq.cur_level_buffers(), 2);
        assert_eq!(dq.cur_level_bytes(), 2);
        assert_eq!(dq.cur_level_time(), 1.seconds());
        push_segment_done(elem, dq);

        pop_intial_events(dq);
        let buf = pop_buffer(dq);
        assert_eq!(buf.pts(), Some(0.seconds()));
        assert_eq!(dq.cur_level_buffers(), 1);
        assert_eq!(dq.cur_level_bytes(), 1);
        assert_eq!(dq.cur_level_time(), 0.seconds());
        let buf = pop_buffer(dq);
        assert_eq!(buf.pts(), Some(1.seconds()));
        assert_eq!(dq.cur_level_buffers(), 0);
        assert_eq!(dq.cur_level_bytes(), 0);
        assert_eq!(dq.cur_level_time(), 0.seconds());
        pop_segment_done(dq);
        assert!(dq.is_empty());

        // Buffer list
        push_initial_events(elem, dq);
        assert_eq!(dq.cur_level_buffers(), 0);
        assert_eq!(dq.cur_level_bytes(), 0);
        assert_eq!(dq.cur_level_time(), 0.seconds());

        push_buffer_list(elem, dq, 0.seconds()).unwrap();
        assert_eq!(dq.cur_level_buffers(), 2);
        assert_eq!(dq.cur_level_bytes(), 2);
        assert_eq!(dq.cur_level_time(), 1.seconds());

        let rejected_buf_list = match push_buffer_list(elem, dq, 2.seconds()).unwrap_err() {
            DataQueueItem::BufferList(buf_list) => buf_list,
            other => panic!("Unexpected {other:?}"),
        };
        assert_eq!(rejected_buf_list.len(), 2);
        let buf = rejected_buf_list.get(0).unwrap();
        assert_eq!(buf.pts(), Some(2.seconds()));

        let rejected_buf = match push_buffer(elem, dq, 2.seconds()).unwrap_err() {
            DataQueueItem::Buffer(buf) => buf,
            other => panic!("Unexpected {other:?}"),
        };
        assert_eq!(rejected_buf.pts(), Some(2.seconds()));
        assert_eq!(dq.cur_level_buffers(), 2);
        assert_eq!(dq.cur_level_bytes(), 2);
        assert_eq!(dq.cur_level_time(), 1.seconds());

        push_segment_done(elem, dq);

        pop_intial_events(dq);
        let buf_list = pop_buffer_list(dq);
        let buf = buf_list.get(0).unwrap();
        assert_eq!(buf.pts(), Some(0.seconds()));
        assert_eq!(dq.cur_level_buffers(), 0);
        assert_eq!(dq.cur_level_bytes(), 0);
        assert_eq!(dq.cur_level_time(), 0.seconds());
        pop_segment_done(dq);
        assert!(dq.is_empty());
    }

    #[test]
    fn not_leaky_max_size_buffers() {
        let (elem, src_pad) = init("not_leaky - max_size_buffers");

        let mut dq = DataQueueBuilder::new(&elem, &src_pad)
            .max_size_buffers(2)
            .max_size_bytes(0)
            .max_size_time(gst::ClockTime::ZERO)
            .build();

        test_not_leaky(&elem, &mut dq);
    }

    #[test]
    fn not_leaky_max_size_bytes() {
        let (elem, src_pad) = init("not_leaky - max_size_bytes");

        let mut dq = DataQueueBuilder::new(&elem, &src_pad)
            .max_size_buffers(0)
            .max_size_bytes(2)
            .max_size_time(gst::ClockTime::ZERO)
            .build();

        test_not_leaky(&elem, &mut dq);
    }

    #[test]
    fn not_leaky_max_size_time() {
        let (elem, src_pad) = init("not_leaky - max_size_time");

        let mut dq = DataQueueBuilder::new(&elem, &src_pad)
            .max_size_buffers(0)
            .max_size_bytes(0)
            .max_size_time(1.seconds())
            .build();

        test_not_leaky(&elem, &mut dq);
    }
}
