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

//! The `Executor` for the `threadshare` GStreamer plugins framework.
//!
//! The [`threadshare`]'s `Executor` consists in a set of [`Context`]s. Each [`Context`] is
//! identified by a `name` and runs a loop in a dedicated `thread`. Users can use the [`Context`]
//! to spawn `Future`s. `Future`s are asynchronous processings which allow waiting for resources
//! in a non-blocking way. Examples of non-blocking operations are:
//!
//! * Waiting for an incoming packet on a Socket.
//! * Waiting for an asynchronous `Mutex` `lock` to succeed.
//! * Waiting for a `Timeout` to be elapsed.
//!
//! [`Context`]s instantiators define the minimum time between two iterations of the [`Context`]
//! loop, which acts as a throttle, saving CPU usage when no operations are to be executed.
//!
//! `Element` implementations should use [`PadSrc`] & [`PadSink`] which provides high-level features.
//!
//! [`threadshare`]: ../index.html
//! [`Context`]: struct.Context.html
//! [`PadSrc`]: struct.PadSrc.html
//! [`PadSink`]: struct.PadSink.html

use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::futures_unordered::FuturesUnordered;

use glib;
use glib::{glib_boxed_derive_traits, glib_boxed_type};

use gst;
use gst::{gst_debug, gst_log, gst_trace};

use lazy_static::lazy_static;

use std::collections::HashMap;
use std::io;
use std::mem;
use std::sync::mpsc as sync_mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

use super::RUNTIME_CAT;

// We are bound to using `sync` for the `runtime` `Mutex`es. Attempts to use `async` `Mutex`es
// lead to the following issues:
//
// * `CONTEXTS`: can't `spawn` a `Future` when called from a `Context` thread via `ffi`.
// * `timers`: can't automatically `remove` the timer from `BinaryHeap` because `async drop`
//    is not available.
// * `task_queues`: can't `add` a pending task when called from a `Context` thread via `ffi`.
//
// Also, we want to be able to `acquire` a `Context` outside of an `async` context.
// These `Mutex`es must be `lock`ed for a short period.
lazy_static! {
    static ref CONTEXTS: Mutex<HashMap<String, Weak<ContextInner>>> = Mutex::new(HashMap::new());
}

struct ContextThread {
    name: String,
}

impl ContextThread {
    fn start(name: &str, wait: u32) -> (tokio::runtime::Handle, ContextShutdown) {
        let name_clone = name.into();

        let mut context_thread = ContextThread { name: name_clone };

        let (handle_sender, handle_receiver) = sync_mpsc::channel();
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();

        let join = thread::spawn(move || {
            context_thread.spawn(wait, handle_sender, shutdown_receiver);
        });

        let handle = handle_receiver.recv().expect("Context thread init failed");

        let shutdown = ContextShutdown {
            name: name.into(),
            shutdown: Some(shutdown_sender),
            join: Some(join),
        };

        (handle, shutdown)
    }

    fn spawn(
        &mut self,
        wait: u32,
        handle_sender: sync_mpsc::Sender<tokio::runtime::Handle>,
        shutdown_receiver: oneshot::Receiver<()>,
    ) {
        gst_debug!(RUNTIME_CAT, "Started context thread '{}'", self.name);

        let mut runtime = tokio::runtime::Builder::new()
            .basic_scheduler()
            .enable_all()
            .max_throttling(Duration::from_millis(wait as u64))
            .build()
            .expect("Couldn't build the runtime");

        handle_sender
            .send(runtime.handle().clone())
            .expect("Couldn't send context thread handle");

        let _ = runtime.block_on(shutdown_receiver);
    }
}

impl Drop for ContextThread {
    fn drop(&mut self) {
        gst_debug!(RUNTIME_CAT, "Terminated: context thread '{}'", self.name);
    }
}

#[derive(Debug)]
struct ContextShutdown {
    name: String,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for ContextShutdown {
    fn drop(&mut self) {
        gst_debug!(
            RUNTIME_CAT,
            "Shutting down context thread thread '{}'",
            self.name
        );
        self.shutdown.take().unwrap();

        gst_trace!(
            RUNTIME_CAT,
            "Waiting for context thread '{}' to shutdown",
            self.name
        );
        let _ = self.join.take().unwrap().join();
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
pub struct TaskQueueId(u64);

impl glib::subclass::boxed::BoxedType for TaskQueueId {
    const NAME: &'static str = "TsTaskQueueId";

    glib_boxed_type!();
}

glib_boxed_derive_traits!(TaskQueueId);

pub type TaskOutput = Result<(), gst::FlowError>;
type TaskQueue = FuturesUnordered<BoxFuture<'static, TaskOutput>>;

#[derive(Debug)]
struct ContextInner {
    name: String,
    handle: Mutex<tokio::runtime::Handle>,
    // Only used for dropping
    _shutdown: ContextShutdown,
    task_queues: Mutex<(u64, HashMap<u64, TaskQueue>)>,
}

impl Drop for ContextInner {
    fn drop(&mut self) {
        let mut contexts = CONTEXTS.lock().unwrap();
        gst_debug!(RUNTIME_CAT, "Finalizing context '{}'", self.name);
        contexts.remove(&self.name);
    }
}

#[derive(Clone, Debug)]
pub struct ContextWeak(Weak<ContextInner>);

impl ContextWeak {
    pub fn upgrade(&self) -> Option<Context> {
        self.0.upgrade().map(Context)
    }
}

/// A `threadshare` `runtime` `Context`.
///
/// The `Context` provides low-level asynchronous processing features to
/// multiplex task execution on a single thread.
///
/// `Element` implementations should use [`PadSrc`] and [`PadSink`] which
///  provide high-level features.
///
/// See the [module-level documentation](index.html) for more.
///
/// [`PadSrc`]: ../struct.PadSrc.html
/// [`PadSink`]: ../struct.PadSink.html
#[derive(Clone, Debug)]
pub struct Context(Arc<ContextInner>);

impl Context {
    pub fn acquire(context_name: &str, wait: u32) -> Result<Self, io::Error> {
        let mut contexts = CONTEXTS.lock().unwrap();

        if let Some(inner_weak) = contexts.get(context_name) {
            if let Some(inner_strong) = inner_weak.upgrade() {
                gst_debug!(RUNTIME_CAT, "Joining Context '{}'", inner_strong.name);
                return Ok(Context(inner_strong));
            }
        }

        let (handle, shutdown) = ContextThread::start(context_name, wait);

        let context = Context(Arc::new(ContextInner {
            name: context_name.into(),
            handle: Mutex::new(handle),
            _shutdown: shutdown,
            task_queues: Mutex::new((0, HashMap::new())),
        }));
        contexts.insert(context_name.into(), Arc::downgrade(&context.0));

        gst_debug!(RUNTIME_CAT, "New Context '{}'", context.0.name);
        Ok(context)
    }

    pub fn downgrade(&self) -> ContextWeak {
        ContextWeak(Arc::downgrade(&self.0))
    }

    pub fn acquire_task_queue_id(&self) -> TaskQueueId {
        let mut task_queues = self.0.task_queues.lock().unwrap();
        let id = task_queues.0;
        task_queues.0 += 1;
        task_queues.1.insert(id, FuturesUnordered::new());

        TaskQueueId(id)
    }

    pub fn name(&self) -> &str {
        self.0.name.as_str()
    }

    pub fn spawn<Fut>(&self, future: Fut) -> tokio::task::JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.0.handle.lock().unwrap().spawn(future)
    }

    pub fn release_task_queue(&self, id: TaskQueueId) -> Option<TaskQueue> {
        let mut task_queues = self.0.task_queues.lock().unwrap();
        task_queues.1.remove(&id.0)
    }

    pub fn add_task<T>(&self, id: TaskQueueId, task: T) -> Result<(), ()>
    where
        T: Future<Output = TaskOutput> + Send + 'static,
    {
        let mut task_queues = self.0.task_queues.lock().unwrap();
        match task_queues.1.get_mut(&id.0) {
            Some(task_queue) => {
                task_queue.push(task.boxed());
                Ok(())
            }
            None => Err(()),
        }
    }

    pub fn clear_task_queue(&self, id: TaskQueueId) {
        let mut task_queues = self.0.task_queues.lock().unwrap();
        let task_queue = task_queues.1.get_mut(&id.0).unwrap();

        *task_queue = FuturesUnordered::new();
    }

    pub fn drain_task_queue(&self, id: TaskQueueId) -> Option<impl Future<Output = TaskOutput>> {
        let task_queue = {
            let mut task_queues = self.0.task_queues.lock().unwrap();
            let task_queue = task_queues.1.get_mut(&id.0).unwrap();

            mem::replace(task_queue, FuturesUnordered::new())
        };

        if !task_queue.is_empty() {
            gst_log!(
                RUNTIME_CAT,
                "Scheduling {} tasks from {:?} on '{}'",
                task_queue.len(),
                id,
                self.0.name,
            );

            Some(task_queue.try_for_each(|_| future::ok(())))
        } else {
            None
        }
    }

    /// Builds a `Future` to execute an `action` at [`Interval`]s.
    ///
    /// [`Interval`]: struct.Interval.html
    pub fn interval<F, E, Fut>(&self, interval: Duration, f: F) -> impl Future<Output = Fut::Output>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        E: Send + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
    {
        async move {
            let mut interval = tokio::time::interval(interval);
            loop {
                interval.tick().await;
                if let Err(err) = f().await {
                    break Err(err);
                }
            }
        }
    }

    /// Builds a `Future` to execute an action after the given `delay` has elapsed.
    pub fn delay_for<F, Fut>(&self, delay: Duration, f: F) -> impl Future<Output = Fut::Output>
    where
        F: FnOnce() -> Fut + Send + Sync + 'static,
        Fut: Future + Send + 'static,
    {
        async move {
            tokio::time::delay_for(delay).await;
            f().await
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::mpsc;
    use futures::future::abortable;
    use futures::lock::Mutex;

    use gst;

    use std::sync::Arc;
    use std::time::Instant;

    use super::*;

    type Item = i32;

    const SLEEP_DURATION: u32 = 2;
    const INTERVAL: Duration = std::time::Duration::from_millis(100 * SLEEP_DURATION as u64);

    #[tokio::test]
    async fn user_drain_pending_tasks() {
        // Setup
        gst::init().unwrap();

        let context = Context::acquire("user_drain_task_queue", SLEEP_DURATION).unwrap();
        let queue_id = context.acquire_task_queue_id();

        let (sender, mut receiver) = mpsc::channel(1);
        let sender: Arc<Mutex<mpsc::Sender<Item>>> = Arc::new(Mutex::new(sender));

        let ctx_weak = context.downgrade();
        let queue_id_clone = queue_id.clone();
        let add_task = move |item| {
            let sender_task = Arc::clone(&sender);
            let context = ctx_weak.upgrade().unwrap();
            context.add_task(queue_id_clone, async move {
                sender_task
                    .lock()
                    .await
                    .send(item)
                    .await
                    .map_err(|_| gst::FlowError::Error)
            })
        };

        // Tests
        assert!(context.drain_task_queue(queue_id).is_none());

        add_task(0).unwrap();
        receiver.try_next().unwrap_err();

        let drain = context.drain_task_queue(queue_id).unwrap();

        // User triggered drain
        receiver.try_next().unwrap_err();

        drain.await.unwrap();
        assert_eq!(receiver.try_next().unwrap(), Some(0));

        add_task(1).unwrap();
        receiver.try_next().unwrap_err();
    }

    #[tokio::test]
    async fn delay_for() {
        gst::init().unwrap();

        let context = Context::acquire("delay_for", SLEEP_DURATION).unwrap();

        let (sender, receiver) = oneshot::channel();

        let start = Instant::now();
        let delayed_by_fut = context.delay_for(INTERVAL, move || {
            async {
                sender.send(42).unwrap();
            }
        });
        context.spawn(delayed_by_fut);

        let _ = receiver.await.unwrap();
        let delta = Instant::now() - start;
        assert!(delta >= INTERVAL);
        assert!(delta < INTERVAL * 2);
    }

    #[tokio::test]
    async fn interval_ok() {
        gst::init().unwrap();

        let context = Context::acquire("interval_ok", SLEEP_DURATION).unwrap();

        let (sender, mut receiver) = mpsc::channel(1);
        let sender: Arc<Mutex<mpsc::Sender<Instant>>> = Arc::new(Mutex::new(sender));

        let (interval_fut, handle) = abortable(context.interval(INTERVAL, move || {
            let sender = Arc::clone(&sender);
            async move {
                let instant = Instant::now();
                sender.lock().await.send(instant).await.map_err(drop)
            }
        }));
        context.spawn(interval_fut.map(drop));

        let mut idx: u32 = 0;
        let mut first = Instant::now();
        while let Some(instant) = receiver.next().await {
            if idx > 0 {
                let delta = instant - first;
                assert!(delta > INTERVAL * (idx - 1));
                assert!(delta < INTERVAL * (idx + 1));
            } else {
                first = instant;
            }
            if idx == 3 {
                handle.abort();
                break;
            }

            idx += 1;
        }
    }

    #[tokio::test]
    async fn interval_err() {
        gst::init().unwrap();

        let context = Context::acquire("interval_err", SLEEP_DURATION).unwrap();

        let (sender, mut receiver) = mpsc::channel(1);
        let sender: Arc<Mutex<mpsc::Sender<Instant>>> = Arc::new(Mutex::new(sender));
        let interval_idx: Arc<Mutex<Item>> = Arc::new(Mutex::new(0));

        let interval_fut = context.interval(INTERVAL, move || {
            let sender = Arc::clone(&sender);
            let interval_idx = Arc::clone(&interval_idx);
            async move {
                let instant = Instant::now();
                let mut idx = interval_idx.lock().await;
                sender.lock().await.send(instant).await.unwrap();
                *idx += 1;
                if *idx < 3 {
                    Ok(())
                } else {
                    Err(())
                }
            }
        });
        context.spawn(interval_fut.map(drop));

        let mut idx: u32 = 0;
        let mut first = Instant::now();
        while let Some(instant) = receiver.next().await {
            if idx > 0 {
                let delta = instant - first;
                assert!(delta > INTERVAL * (idx - 1));
                assert!(delta < INTERVAL * (idx + 1));
            } else {
                first = instant;
            }

            idx += 1;
        }

        assert_eq!(idx, 3);
    }
}
