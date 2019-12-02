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

use futures::channel::mpsc as future_mpsc;
use futures::future::BoxFuture;
use futures::prelude::*;
use futures::ready;
use futures::stream::futures_unordered::FuturesUnordered;

use glib;
use glib::{glib_boxed_derive_traits, glib_boxed_type};

use gst;
use gst::{gst_debug, gst_log, gst_trace};

use lazy_static::lazy_static;

use std::cmp;
use std::collections::{BinaryHeap, HashMap};
use std::io;
use std::mem;
use std::pin::Pin;
use std::sync::mpsc as sync_mpsc;
use std::sync::{atomic, Arc, Mutex, Weak};
use std::task::Poll;
use std::thread;
use std::time::{Duration, Instant};

use tokio_executor::current_thread as tokio_current_thread;
use tokio_executor::park::Unpark;

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
    shutdown: Arc<atomic::AtomicBool>,
}

impl ContextThread {
    fn start(
        name: &str,
        wait: u32,
        reactor: tokio_net::driver::Reactor,
        timers: Arc<Mutex<BinaryHeap<TimerEntry>>>,
    ) -> (tokio_current_thread::Handle, ContextShutdown) {
        let handle = reactor.handle();
        let shutdown = Arc::new(atomic::AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let name_clone = name.into();

        let mut context_thread = ContextThread {
            shutdown: shutdown_clone,
            name: name_clone,
        };

        let (sender, receiver) = sync_mpsc::channel();

        let join = thread::spawn(move || {
            context_thread.spawn(wait, reactor, sender, timers);
        });

        let shutdown = ContextShutdown {
            name: name.into(),
            shutdown,
            handle,
            join: Some(join),
        };

        let thread_handle = receiver.recv().expect("Context thread init failed");

        (thread_handle, shutdown)
    }

    fn spawn(
        &mut self,
        wait: u32,
        reactor: tokio_net::driver::Reactor,
        sender: sync_mpsc::Sender<tokio_current_thread::Handle>,
        timers: Arc<Mutex<BinaryHeap<TimerEntry>>>,
    ) {
        gst_debug!(RUNTIME_CAT, "Started context thread '{}'", self.name);

        let wait = Duration::from_millis(wait as u64);

        let handle = reactor.handle();
        let timer = tokio_timer::Timer::new(reactor);
        let timer_handle = timer.handle();

        let mut current_thread = tokio_current_thread::CurrentThread::new_with_park(timer);

        sender
            .send(current_thread.handle())
            .expect("Couldn't send context thread handle");

        let _timer_guard = tokio_timer::set_default(&timer_handle);
        let _reactor_guard = tokio_net::driver::set_default(&handle);

        let mut now = Instant::now();

        loop {
            if self.shutdown.load(atomic::Ordering::SeqCst) {
                gst_debug!(RUNTIME_CAT, "Shutting down loop");
                break;
            }

            gst_trace!(RUNTIME_CAT, "Elapsed {:?} since last loop", now.elapsed());

            // Handle timers
            {
                // Trigger all timers that would be expired before the middle of the loop wait
                // time
                let timer_threshold = now + wait / 2;
                let mut timers = timers.lock().unwrap();
                while timers
                    .peek()
                    .and_then(|entry| {
                        if entry.time < timer_threshold {
                            Some(())
                        } else {
                            None
                        }
                    })
                    .is_some()
                {
                    let TimerEntry {
                        time,
                        interval,
                        sender,
                        ..
                    } = timers.pop().unwrap();

                    if sender.is_closed() {
                        continue;
                    }

                    let _ = sender.unbounded_send(());
                    if let Some(interval) = interval {
                        timers.push(TimerEntry {
                            time: time + interval,
                            id: TIMER_ENTRY_ID.fetch_add(1, atomic::Ordering::Relaxed),
                            interval: Some(interval),
                            sender,
                        });
                    }
                }
            }

            gst_trace!(RUNTIME_CAT, "Turning thread '{}'", self.name);
            while current_thread
                .turn(Some(Duration::from_millis(0)))
                .unwrap()
                .has_polled()
            {}
            gst_trace!(RUNTIME_CAT, "Turned thread '{}'", self.name);

            // We have to check again after turning in case we're supposed to shut down now
            // and already handled the unpark above
            if self.shutdown.load(atomic::Ordering::SeqCst) {
                gst_debug!(RUNTIME_CAT, "Shutting down loop");
                break;
            }

            let elapsed = now.elapsed();
            gst_trace!(RUNTIME_CAT, "Elapsed {:?} after handling futures", elapsed);

            if wait == Duration::from_millis(0) {
                let timers = timers.lock().unwrap();
                let wait = match timers.peek().map(|entry| entry.time) {
                    None => None,
                    Some(time) => Some({
                        let tmp = Instant::now();

                        if time < tmp {
                            Duration::from_millis(0)
                        } else {
                            time.duration_since(tmp)
                        }
                    }),
                };
                drop(timers);

                gst_trace!(RUNTIME_CAT, "Sleeping for up to {:?}", wait);
                current_thread.turn(wait).unwrap();
                gst_trace!(RUNTIME_CAT, "Slept for {:?}", now.elapsed());
                now = Instant::now();
            } else {
                if elapsed < wait {
                    gst_trace!(
                        RUNTIME_CAT,
                        "Waiting for {:?} before polling again",
                        wait - elapsed
                    );
                    thread::sleep(wait - elapsed);
                    gst_trace!(RUNTIME_CAT, "Slept for {:?}", now.elapsed());
                }

                now += wait;
            }
        }
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
    shutdown: Arc<atomic::AtomicBool>,
    handle: tokio_net::driver::Handle,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for ContextShutdown {
    fn drop(&mut self) {
        gst_debug!(
            RUNTIME_CAT,
            "Shutting down context thread thread '{}'",
            self.name
        );
        self.shutdown.store(true, atomic::Ordering::SeqCst);
        gst_trace!(
            RUNTIME_CAT,
            "Waiting for context thread '{}' to shutdown",
            self.name
        );
        // After being unparked, the next turn() is guaranteed to finish immediately,
        // as such there is no race condition between checking for shutdown and setting
        // shutdown.
        self.handle.unpark();
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
    thread_handle: Mutex<tokio_current_thread::Handle>,
    reactor_handle: tokio_net::driver::Handle,
    timers: Arc<Mutex<BinaryHeap<TimerEntry>>>,
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

        let reactor = tokio_net::driver::Reactor::new()?;
        let reactor_handle = reactor.handle();

        let timers = Arc::new(Mutex::new(BinaryHeap::new()));

        let (thread_handle, shutdown) =
            ContextThread::start(context_name, wait, reactor, timers.clone());

        let context = Context(Arc::new(ContextInner {
            name: context_name.into(),
            thread_handle: Mutex::new(thread_handle),
            reactor_handle,
            timers,
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

    pub fn reactor_handle(&self) -> &tokio_net::driver::Handle {
        &self.0.reactor_handle
    }

    pub fn spawn<Fut>(&self, future: Fut)
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.0.thread_handle.lock().unwrap().spawn(future).unwrap();
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

    pub fn add_timer(
        &self,
        time: Instant,
        interval: Option<Duration>,
    ) -> future_mpsc::UnboundedReceiver<()> {
        let (sender, receiver) = future_mpsc::unbounded();

        let mut timers = self.0.timers.lock().unwrap();
        let entry = TimerEntry {
            time,
            id: TIMER_ENTRY_ID.fetch_add(1, atomic::Ordering::Relaxed),
            interval,
            sender,
        };

        timers.push(entry);
        self.0.reactor_handle.unpark();

        receiver
    }

    pub fn new_interval(&self, interval: Duration) -> Interval {
        Interval::new(&self, interval)
    }

    /// Builds a `Future` to execute an `action` at [`Interval`]s.
    ///
    /// [`Interval`]: struct.Interval.html
    pub fn interval<F, Fut>(&self, interval: Duration, f: F) -> impl Future<Output = Fut::Output>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), ()>> + Send + 'static,
    {
        let f = Arc::new(f);
        self.new_interval(interval).try_for_each(move |_| {
            let f = Arc::clone(&f);
            f()
        })
    }

    pub fn new_timeout(&self, timeout: Duration) -> Timeout {
        Timeout::new(&self, timeout)
    }

    /// Builds a `Future` to execute an action after the given `delay` has elapsed.
    pub fn delay_for<F, Fut>(&self, delay: Duration, f: F) -> impl Future<Output = Fut::Output>
    where
        F: FnOnce() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.new_timeout(delay).then(move |_| f())
    }
}

static TIMER_ENTRY_ID: atomic::AtomicUsize = atomic::AtomicUsize::new(0);

// Ad-hoc interval timer implementation for our throttled event loop above
#[derive(Debug)]
struct TimerEntry {
    time: Instant,
    id: usize, // for producing a total order
    interval: Option<Duration>,
    sender: future_mpsc::UnboundedSender<()>,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.time.eq(&other.time) && self.id.eq(&other.id)
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(&other))
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        other
            .time
            .cmp(&self.time)
            .then_with(|| other.id.cmp(&self.id))
    }
}

/// A `Stream` that yields a tick at `interval`s.
#[derive(Debug)]
pub struct Interval {
    receiver: future_mpsc::UnboundedReceiver<()>,
}

impl Interval {
    fn new(context: &Context, interval: Duration) -> Self {
        Self {
            receiver: context.add_timer(Instant::now(), Some(interval)),
        }
    }
}

impl Stream for Interval {
    type Item = Result<(), ()>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> Poll<Option<Self::Item>> {
        self.receiver
            .poll_next_unpin(cx)
            .map(|item_opt| item_opt.map(Ok))
    }
}

/// A `Future` that completes after a `timeout` is elapsed.
#[derive(Debug)]
pub struct Timeout {
    receiver: future_mpsc::UnboundedReceiver<()>,
}

impl Timeout {
    fn new(context: &Context, timeout: Duration) -> Self {
        Self {
            receiver: context.add_timer(Instant::now() + timeout, None),
        }
    }
}

impl Future for Timeout {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
        match ready!(self.receiver.poll_next_unpin(cx)) {
            Some(_) => Poll::Ready(()),
            None => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::{mpsc, oneshot};
    use futures::future::Aborted;
    use futures::lock::Mutex;

    use gst;

    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::block_on;
    use crate::runtime::future::abortable_waitable;

    use super::*;

    type Item = i32;

    const SLEEP_DURATION: u32 = 2;
    const INTERVAL: Duration = Duration::from_millis(100 * SLEEP_DURATION as u64);

    #[test]
    fn user_drain_pending_tasks() {
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

        block_on!(drain).unwrap();
        assert_eq!(receiver.try_next().unwrap(), Some(0));

        add_task(1).unwrap();
        receiver.try_next().unwrap_err();
    }

    #[test]
    fn delay_for() {
        gst::init().unwrap();

        let context = Context::acquire("delay_for", SLEEP_DURATION).unwrap();

        let (sender, receiver) = oneshot::channel();

        let start = Instant::now();
        let delayed_by_fut = context.delay_for(INTERVAL, move || async {
            sender.send(42).unwrap();
        });
        context.spawn(delayed_by_fut);

        let _ = block_on!(receiver).unwrap();
        let delta = Instant::now() - start;
        assert!(delta >= INTERVAL);
        assert!(delta < INTERVAL * 2);
    }

    #[test]
    fn delay_for_abort() {
        gst::init().unwrap();

        let context = Context::acquire("delay_for_abort", SLEEP_DURATION).unwrap();

        let (sender, receiver) = oneshot::channel();

        let delay_for_fut = context.delay_for(INTERVAL, move || async {
            sender.send(42).unwrap();
        });
        let (abortable_delay_for, abort_handle) = abortable_waitable(delay_for_fut);
        context.spawn(abortable_delay_for.map(move |res| {
            if let Err(Aborted) = res {
                gst_debug!(RUNTIME_CAT, "Aborted delay_for");
            }
        }));

        block_on!(abort_handle.abort_and_wait()).unwrap();
        block_on!(receiver).unwrap_err();
    }

    #[test]
    fn interval_ok() {
        gst::init().unwrap();

        let context = Context::acquire("interval_ok", SLEEP_DURATION).unwrap();

        let (sender, mut receiver) = mpsc::channel(1);
        let sender: Arc<Mutex<mpsc::Sender<Instant>>> = Arc::new(Mutex::new(sender));

        let interval_fut = context.interval(INTERVAL, move || {
            let sender = Arc::clone(&sender);
            async move {
                let instant = Instant::now();
                sender.lock().await.send(instant).await.map_err(drop)
            }
        });
        context.spawn(interval_fut.map(drop));

        block_on!(async {
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
                    break;
                }

                idx += 1;
            }
        });
    }

    #[test]
    fn interval_err() {
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

        block_on!(async {
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
        });
    }

    #[test]
    fn interval_abort() {
        gst::init().unwrap();

        let context = Context::acquire("interval_abort", SLEEP_DURATION).unwrap();

        let (sender, mut receiver) = mpsc::channel(1);
        let sender: Arc<Mutex<mpsc::Sender<Instant>>> = Arc::new(Mutex::new(sender));

        let interval_fut = context.interval(INTERVAL, move || {
            let sender = Arc::clone(&sender);
            async move {
                let instant = Instant::now();
                sender.lock().await.send(instant).await.map_err(drop)
            }
        });
        let (abortable_interval, abort_handle) = abortable_waitable(interval_fut);
        context.spawn(abortable_interval.map(move |res| {
            if let Err(Aborted) = res {
                gst_debug!(RUNTIME_CAT, "Aborted timeout");
            }
        }));

        block_on!(async {
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
                    abort_handle.abort_and_wait().await.unwrap();
                    break;
                }

                idx += 1;
            }

            assert_eq!(receiver.next().await, None);
        });
    }
}
