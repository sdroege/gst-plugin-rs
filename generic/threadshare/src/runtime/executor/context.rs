// Copyright (C) 2018-2020 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2019-2021 François Laignel <fengalin@free.fr>
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

use gst::{gst_debug, gst_trace};

use once_cell::sync::Lazy;

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{Handle, HandleWeak, JoinHandle, Scheduler, SubTaskOutput, TaskId};
use crate::runtime::RUNTIME_CAT;

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
static CONTEXTS: Lazy<Mutex<HashMap<Arc<str>, ContextWeak>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Blocks on `future` in one way or another if possible.
///
/// IO & time related `Future`s must be handled within their own [`Context`].
/// Wait for the result using a [`JoinHandle`] or a `channel`.
///
/// If there's currently an active `Context` with a task, then the future is only queued up as a
/// pending sub task for that task.
///
/// Otherwise the current thread is blocking and the passed in future is executed.
///
/// Note that you must not pass any futures here that wait for the currently active task in one way
/// or another as this would deadlock!
pub fn block_on_or_add_sub_task<Fut: Future + Send + 'static>(future: Fut) -> Option<Fut::Output> {
    if let Some((cur_context, cur_task_id)) = Context::current_task() {
        gst_debug!(
            RUNTIME_CAT,
            "Adding subtask to task {:?} on context {}",
            cur_task_id,
            cur_context.name()
        );
        let _ = Context::add_sub_task(async move {
            future.await;
            Ok(())
        });
        return None;
    }

    // Not running in a Context thread so we can block
    Some(block_on(future))
}

/// Blocks on `future`.
///
/// IO & time related `Future`s must be handled within their own [`Context`].
/// Wait for the result using a [`JoinHandle`] or a `channel`.
///
/// The current thread is blocking and the passed in future is executed.
///
/// # Panics
///
/// This function panics if called within a [`Context`] thread.
pub fn block_on<F: Future>(future: F) -> F::Output {
    assert!(!Context::is_context_thread());

    // Not running in a Context thread so we can block
    gst_debug!(RUNTIME_CAT, "Blocking on new dummy context");
    Scheduler::block_on(future)
}

/// Yields execution back to the runtime
#[inline]
pub async fn yield_now() {
    tokio::task::yield_now().await;
}

#[derive(Clone, Debug)]
pub struct ContextWeak(HandleWeak);

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
/// [`PadSrc`]: ../pad/struct.PadSrc.html
/// [`PadSink`]: ../pad/struct.PadSink.html
#[derive(Clone, Debug)]
pub struct Context(Handle);

impl PartialEq for Context {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq(&other.0)
    }
}

impl Eq for Context {}

impl Context {
    pub fn acquire(context_name: &str, wait: Duration) -> Result<Self, io::Error> {
        assert_ne!(context_name, Scheduler::DUMMY_NAME);

        let mut contexts = CONTEXTS.lock().unwrap();

        if let Some(context_weak) = contexts.get(context_name) {
            if let Some(context) = context_weak.upgrade() {
                gst_debug!(RUNTIME_CAT, "Joining Context '{}'", context.name());
                return Ok(context);
            }
        }

        let context = Context(Scheduler::start(context_name, wait));
        contexts.insert(context_name.into(), context.downgrade());

        gst_debug!(RUNTIME_CAT, "New Context '{}'", context.name());
        Ok(context)
    }

    pub fn downgrade(&self) -> ContextWeak {
        ContextWeak(self.0.downgrade())
    }

    pub fn name(&self) -> &str {
        self.0.context_name()
    }

    // FIXME this could be renamed as max_throttling
    // but then, all elements should also change their
    // wait variables and properties to max_throttling.
    pub fn wait_duration(&self) -> Duration {
        self.0.max_throttling()
    }

    /// Returns `true` if a `Context` is running on current thread.
    pub fn is_context_thread() -> bool {
        Scheduler::is_scheduler_thread()
    }

    /// Returns the `Context` running on current thread, if any.
    pub fn current() -> Option<Context> {
        Scheduler::current().map(Context)
    }

    /// Returns the `TaskId` running on current thread, if any.
    pub fn current_task() -> Option<(Context, TaskId)> {
        Scheduler::current().map(Context).zip(TaskId::current())
    }

    pub fn enter<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.0.enter(f)
    }

    pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.0.spawn(future, false)
    }

    pub fn awake_and_spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.0.spawn(future, true)
    }

    pub fn current_has_sub_tasks() -> bool {
        let (ctx, task_id) = match Context::current_task() {
            Some(task) => task,
            None => {
                gst_trace!(RUNTIME_CAT, "No current task");
                return false;
            }
        };

        ctx.0.has_sub_tasks(task_id)
    }

    pub fn add_sub_task<T>(sub_task: T) -> Result<(), T>
    where
        T: Future<Output = SubTaskOutput> + Send + 'static,
    {
        let (ctx, task_id) = match Context::current_task() {
            Some(task) => task,
            None => {
                gst_trace!(RUNTIME_CAT, "No current task");
                return Err(sub_task);
            }
        };

        ctx.0.add_sub_task(task_id, sub_task)
    }

    pub async fn drain_sub_tasks() -> SubTaskOutput {
        let (ctx, task_id) = match Context::current_task() {
            Some(task) => task,
            None => return Ok(()),
        };

        ctx.0.drain_sub_tasks(task_id).await
    }
}

impl From<Handle> for Context {
    fn from(handle: Handle) -> Self {
        Context(handle)
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::mpsc;
    use futures::lock::Mutex;
    use futures::prelude::*;

    use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::super::Scheduler;
    use super::Context;

    type Item = i32;

    const SLEEP_DURATION_MS: u64 = 2;
    const SLEEP_DURATION: Duration = Duration::from_millis(SLEEP_DURATION_MS);
    const DELAY: Duration = Duration::from_millis(SLEEP_DURATION_MS * 10);

    #[test]
    fn block_on_task_id() {
        gst::init().unwrap();

        assert!(!Context::is_context_thread());

        crate::runtime::executor::block_on(async {
            let (ctx, task_id) = Context::current_task().unwrap();
            assert_eq!(ctx.name(), Scheduler::DUMMY_NAME);
            assert_eq!(task_id, super::TaskId(0));

            let res = Context::add_sub_task(async move {
                let (_ctx, task_id) = Context::current_task().unwrap();
                assert_eq!(task_id, super::TaskId(0));
                Ok(())
            });
            assert!(res.is_ok());
            assert!(Context::is_context_thread());
        });

        assert!(!Context::is_context_thread());
    }

    #[test]
    fn block_on_timer() {
        gst::init().unwrap();

        let elapsed = crate::runtime::executor::block_on(async {
            let now = Instant::now();
            crate::runtime::time::delay_for(DELAY).await;
            now.elapsed()
        });

        assert!(elapsed >= DELAY);
    }

    #[test]
    fn context_task_id() {
        gst::init().unwrap();

        let context = Context::acquire("context_task_id", SLEEP_DURATION).unwrap();
        let join_handle = context.spawn(async {
            let (ctx, task_id) = Context::current_task().unwrap();
            assert_eq!(ctx.name(), "context_task_id");
            assert_eq!(task_id, super::TaskId(0));
        });
        futures::executor::block_on(join_handle).unwrap();

        let ctx_weak = context.downgrade();
        let join_handle = context.spawn(async move {
            let (_ctx, task_id) = Context::current_task().unwrap();
            assert_eq!(task_id, super::TaskId(1));

            let res = Context::add_sub_task(async move {
                let (_ctx, task_id) = Context::current_task().unwrap();
                assert_eq!(task_id, super::TaskId(1));
                Ok(())
            });
            assert!(res.is_ok());

            ctx_weak
                .upgrade()
                .unwrap()
                .spawn(async {
                    let (_ctx, task_id) = Context::current_task().unwrap();
                    assert_eq!(task_id, super::TaskId(2));

                    let res = Context::add_sub_task(async move {
                        let (_ctx, task_id) = Context::current_task().unwrap();
                        assert_eq!(task_id, super::TaskId(2));
                        Ok(())
                    });
                    assert!(res.is_ok());
                    assert!(Context::drain_sub_tasks().await.is_ok());

                    let (_ctx, task_id) = Context::current_task().unwrap();
                    assert_eq!(task_id, super::TaskId(2));
                })
                .await
                .unwrap();

            assert!(Context::drain_sub_tasks().await.is_ok());

            let (_ctx, task_id) = Context::current_task().unwrap();
            assert_eq!(task_id, super::TaskId(1));
        });
        futures::executor::block_on(join_handle).unwrap();
    }

    #[test]
    fn drain_sub_tasks() {
        // Setup
        gst::init().unwrap();

        let context = Context::acquire("drain_sub_tasks", SLEEP_DURATION).unwrap();

        let join_handle = context.spawn(async {
            let (sender, mut receiver) = mpsc::channel(1);
            let sender: Arc<Mutex<mpsc::Sender<Item>>> = Arc::new(Mutex::new(sender));

            let add_sub_task = move |item| {
                let sender = sender.clone();
                Context::add_sub_task(async move {
                    sender
                        .lock()
                        .await
                        .send(item)
                        .await
                        .map_err(|_| gst::FlowError::Error)
                })
            };

            // Tests

            // Drain empty queue
            let drain_fut = Context::drain_sub_tasks();
            drain_fut.await.unwrap();

            // Add a subtask
            add_sub_task(0).map_err(drop).unwrap();

            // Check that it was not executed yet
            receiver.try_next().unwrap_err();

            // Drain it now and check that it was executed
            let drain_fut = Context::drain_sub_tasks();
            drain_fut.await.unwrap();
            assert_eq!(receiver.try_next().unwrap(), Some(0));

            // Add another task and check that it's not executed yet
            add_sub_task(1).map_err(drop).unwrap();
            receiver.try_next().unwrap_err();

            // Return the receiver
            receiver
        });

        let mut receiver = futures::executor::block_on(join_handle).unwrap();

        // The last sub task should be simply dropped at this point
        match receiver.try_next() {
            Ok(None) | Err(_) => (),
            other => panic!("Unexpected {:?}", other),
        }
    }

    #[test]
    fn block_on_from_sync() {
        gst::init().unwrap();

        let context = Context::acquire("block_on_from_sync", SLEEP_DURATION).unwrap();

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

        let context = Context::acquire("block_on_from_context", SLEEP_DURATION).unwrap();
        let join_handle = context.spawn(async {
            crate::runtime::executor::block_on(async {
                crate::runtime::time::delay_for(DELAY).await;
            });
        });
        // Panic: attempt to `runtime::executor::block_on` within a `Context` thread
        futures::executor::block_on(join_handle).unwrap_err();
    }

    #[test]
    fn enter_context_from_scheduler() {
        gst::init().unwrap();

        let elapsed = crate::runtime::executor::block_on(async {
            let context = Context::acquire("enter_context_from_tokio", SLEEP_DURATION).unwrap();
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

            context.enter(|| {
                futures::executor::block_on(async {
                    let now = Instant::now();
                    crate::runtime::time::delay_for(DELAY).await;
                    now.elapsed()
                })
            })
        });

        // Due to throttling, `Delay` may be fired earlier
        assert!(elapsed + SLEEP_DURATION / 2 >= DELAY);
    }

    #[test]
    fn enter_context_from_sync() {
        gst::init().unwrap();

        let context = Context::acquire("enter_context_from_sync", SLEEP_DURATION).unwrap();
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
