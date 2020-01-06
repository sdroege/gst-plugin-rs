// Copyright (C) 2018-2019 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2019-2020 François Laignel <fengalin@free.fr>
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
//! * Waiting for a time related `Future`.
//!
//! `Element` implementations should use [`PadSrc`] & [`PadSink`] which provides high-level features.
//!
//! [`threadshare`]: ../../index.html
//! [`Context`]: struct.Context.html
//! [`PadSrc`]: ../pad/struct.PadSrc.html
//! [`PadSink`]: ../pad/struct.PadSink.html

use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::futures_unordered::FuturesUnordered;

use glib;
use glib::{glib_boxed_derive_traits, glib_boxed_type};

use gst;
use gst::{gst_debug, gst_log, gst_trace};

use lazy_static::lazy_static;

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::mem;
use std::pin::Pin;
use std::sync::mpsc as sync_mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::task::Poll;
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

thread_local!(static CURRENT_THREAD_CONTEXT: RefCell<Option<ContextWeak>> = RefCell::new(None));

/// Blocks on `future`.
///
/// IO & time related `Future`s must be handled within their own [`Context`].
/// Wait for the result using a [`JoinHandle`] or a `channel`.
///
/// This function must NOT be called within a [`Context`] thread.
/// The reason is this would prevent any task operating on the
/// [`Context`] from making progress.
///
/// # Panics
///
/// This function panics if called within a [`Context`] thread.
///
/// [`Context`]: struct.Context.html
/// [`JoinHandle`]: enum.JoinHandle.html
pub fn block_on<Fut: Future>(future: Fut) -> Fut::Output {
    if Context::is_context_thread() {
        panic!("Attempt to `block_on` within a `Context` thread");
    }

    // Not running in a Context thread so we can block
    futures::executor::block_on(future)
}

struct ContextThread {
    name: String,
}

impl ContextThread {
    fn start(name: &str, wait: u32) -> Context {
        let context_thread = ContextThread { name: name.into() };
        let (context_sender, context_receiver) = sync_mpsc::channel();
        let join = thread::spawn(move || {
            context_thread.spawn(wait, context_sender);
        });

        let context = context_receiver.recv().expect("Context thread init failed");
        *context.0.shutdown.join.lock().unwrap() = Some(join);

        context
    }

    fn spawn(&self, wait: u32, context_sender: sync_mpsc::Sender<Context>) {
        gst_debug!(RUNTIME_CAT, "Started context thread '{}'", self.name);

        let mut runtime = tokio::runtime::Builder::new()
            .basic_scheduler()
            .thread_name(self.name.clone())
            .enable_all()
            .max_throttling(Duration::from_millis(wait as u64))
            .build()
            .expect("Couldn't build the runtime");

        let (shutdown_sender, shutdown_receiver) = oneshot::channel();

        let shutdown = ContextShutdown {
            name: self.name.clone(),
            shutdown: Some(shutdown_sender),
            join: Mutex::new(None),
        };

        let context = Context(Arc::new(ContextInner {
            name: self.name.clone(),
            handle: Mutex::new(runtime.handle().clone()),
            shutdown,
            task_queues: Mutex::new((0, HashMap::new())),
        }));

        CURRENT_THREAD_CONTEXT.with(|cur_ctx| {
            *cur_ctx.borrow_mut() = Some(context.downgrade());
        });

        context_sender.send(context).unwrap();

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
    join: Mutex<Option<thread::JoinHandle<()>>>,
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
        let join_handle = self.join.lock().unwrap().take().unwrap();
        let _ = join_handle.join();
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

pub struct JoinError(tokio::task::JoinError);

impl fmt::Display for JoinError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, fmt)
    }
}

impl fmt::Debug for JoinError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, fmt)
    }
}

impl std::error::Error for JoinError {}

impl From<tokio::task::JoinError> for JoinError {
    fn from(src: tokio::task::JoinError) -> Self {
        JoinError(src)
    }
}

/// Wrapper for the underlying runtime JoinHandle implementation.
pub struct JoinHandle<T>(tokio::task::JoinHandle<T>);

unsafe impl<T: Send> Send for JoinHandle<T> {}
unsafe impl<T: Send> Sync for JoinHandle<T> {}

impl<T> From<tokio::task::JoinHandle<T>> for JoinHandle<T> {
    fn from(src: tokio::task::JoinHandle<T>) -> Self {
        JoinHandle(src)
    }
}

impl<T> Unpin for JoinHandle<T> {}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        self.as_mut().0.poll_unpin(cx).map_err(JoinError::from)
    }
}

impl<T> fmt::Debug for JoinHandle<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("JoinHandle").finish()
    }
}

#[derive(Debug)]
struct ContextInner {
    name: String,
    handle: Mutex<tokio::runtime::Handle>,
    // Only used for dropping
    shutdown: ContextShutdown,
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
/// [`PadSrc`]: ../pad/struct.PadSrc.html
/// [`PadSink`]: ../pad/struct.PadSink.html
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

        let context = ContextThread::start(context_name, wait);
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

    /// Returns `true` if a `Context` is running on current thread.
    pub fn is_context_thread() -> bool {
        CURRENT_THREAD_CONTEXT.with(|cur_ctx| cur_ctx.borrow().is_some())
    }

    /// Returns the `Context` running on current thread, if any.
    pub fn current() -> Option<Context> {
        CURRENT_THREAD_CONTEXT.with(|cur_ctx| {
            cur_ctx
                .borrow()
                .as_ref()
                .and_then(|ctx_weak| ctx_weak.upgrade())
        })
    }

    pub fn enter<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.0.handle.lock().unwrap().enter(f)
    }

    pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.0.handle.lock().unwrap().spawn(future).into()
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
}

#[cfg(test)]
mod tests {
    use futures;
    use futures::channel::mpsc;
    use futures::lock::Mutex;
    use futures::prelude::*;

    use gst;

    use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::Context;

    type Item = i32;

    const SLEEP_DURATION_MS: u32 = 2;
    const SLEEP_DURATION: Duration = Duration::from_millis(SLEEP_DURATION_MS as u64);
    const DELAY: Duration = Duration::from_millis(SLEEP_DURATION_MS as u64 * 10);

    #[tokio::test]
    async fn user_drain_pending_tasks() {
        // Setup
        gst::init().unwrap();

        let context = Context::acquire("user_drain_task_queue", SLEEP_DURATION_MS).unwrap();
        let queue_id = context.acquire_task_queue_id();

        let (sender, mut receiver) = mpsc::channel(1);
        let sender: Arc<Mutex<mpsc::Sender<Item>>> = Arc::new(Mutex::new(sender));

        let ctx_weak = context.downgrade();
        let add_task = move |item| {
            let sender_task = Arc::clone(&sender);
            let context = ctx_weak.upgrade().unwrap();
            context.add_task(queue_id, async move {
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
    async fn block_on_within_tokio() {
        let context = Context::acquire("block_on_within_tokio", SLEEP_DURATION_MS).unwrap();

        let bytes_sent = crate::runtime::executor::block_on(context.spawn(async {
            let saddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5000);
            let socket = UdpSocket::bind(saddr).unwrap();
            let mut socket = tokio::net::UdpSocket::from_std(socket).unwrap();
            let saddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4000);
            socket.send_to(&[0; 10], saddr).await.unwrap()
        }))
        .unwrap();
        assert_eq!(bytes_sent, 10);

        let elapsed = crate::runtime::executor::block_on(context.spawn(async {
            let now = Instant::now();
            crate::runtime::time::delay_for(DELAY).await;
            now.elapsed()
        }))
        .unwrap();
        // Due to throttling, `Delay` may be fired earlier
        assert!(elapsed + SLEEP_DURATION / 2 >= DELAY);
    }

    #[test]
    fn block_on_from_sync() {
        let context = Context::acquire("block_on_from_sync", SLEEP_DURATION_MS).unwrap();

        let bytes_sent = crate::runtime::executor::block_on(context.spawn(async {
            let saddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5001);
            let socket = UdpSocket::bind(saddr).unwrap();
            let mut socket = tokio::net::UdpSocket::from_std(socket).unwrap();
            let saddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4000);
            socket.send_to(&[0; 10], saddr).await.unwrap()
        }))
        .unwrap();
        assert_eq!(bytes_sent, 10);

        let elapsed = crate::runtime::executor::block_on(context.spawn(async {
            let now = Instant::now();
            crate::runtime::time::delay_for(DELAY).await;
            now.elapsed()
        }))
        .unwrap();
        // Due to throttling, `Delay` may be fired earlier
        assert!(elapsed + SLEEP_DURATION / 2 >= DELAY);
    }

    #[test]
    fn block_on_from_context() {
        gst::init().unwrap();

        let context = Context::acquire("block_on_from_context", SLEEP_DURATION_MS).unwrap();
        let join_handle = context.spawn(async {
            crate::runtime::executor::block_on(async {
                crate::runtime::time::delay_for(DELAY).await;
            });
        });
        // Panic: attempt to `runtime::executor::block_on` within a `Context` thread
        futures::executor::block_on(join_handle).unwrap_err();
    }

    #[tokio::test]
    async fn enter_context_from_tokio() {
        gst::init().unwrap();

        let context = Context::acquire("enter_context_from_tokio", SLEEP_DURATION_MS).unwrap();
        let mut socket = context
            .enter(|| {
                let saddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5002);
                let socket = UdpSocket::bind(saddr).unwrap();
                tokio::net::UdpSocket::from_std(socket)
            })
            .unwrap();

        let saddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4000);
        let bytes_sent = socket.send_to(&[0; 10], saddr).await.unwrap();
        assert_eq!(bytes_sent, 10);

        let elapsed = context.enter(|| {
            futures::executor::block_on(async {
                let now = Instant::now();
                crate::runtime::time::delay_for(DELAY).await;
                now.elapsed()
            })
        });
        // Due to throttling, `Delay` may be fired earlier
        assert!(elapsed + SLEEP_DURATION / 2 >= DELAY);
    }

    #[test]
    fn enter_context_from_sync() {
        gst::init().unwrap();

        let context = Context::acquire("enter_context_from_sync", SLEEP_DURATION_MS).unwrap();
        let mut socket = context
            .enter(|| {
                let saddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5003);
                let socket = UdpSocket::bind(saddr).unwrap();
                tokio::net::UdpSocket::from_std(socket)
            })
            .unwrap();

        let saddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4000);
        let bytes_sent = futures::executor::block_on(socket.send_to(&[0; 10], saddr)).unwrap();
        assert_eq!(bytes_sent, 10);

        let elapsed = context.enter(|| {
            futures::executor::block_on(async {
                let now = Instant::now();
                crate::runtime::time::delay_for(DELAY).await;
                now.elapsed()
            })
        });
        // Due to throttling, `Delay` may be fired earlier
        assert!(elapsed + SLEEP_DURATION / 2 >= DELAY);
    }
}
