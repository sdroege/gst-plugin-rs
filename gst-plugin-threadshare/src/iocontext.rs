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

use futures::channel::{mpsc, oneshot};
use futures::future::{AbortHandle, Abortable, BoxFuture};
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
use std::sync::atomic;
use std::sync::{Arc, Mutex, Weak};
use std::task::{self, Poll};
use std::thread;
use std::time;

use tokio_executor::current_thread as tokio_current_thread;

lazy_static! {
    static ref CONTEXTS: Mutex<HashMap<String, Weak<IOContextInner>>> = Mutex::new(HashMap::new());
    static ref CONTEXT_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-context",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing Context"),
    );
}

// Our own simplified implementation of reactor::Background to allow hooking into its internals
const RUNNING: usize = 0;
const SHUTDOWN_NOW: usize = 1;

struct IOContextRunner {
    name: String,
    shutdown: Arc<atomic::AtomicUsize>,
}

impl IOContextRunner {
    fn start(
        name: &str,
        wait: u32,
        reactor: tokio_net::driver::Reactor,
        timers: Arc<Mutex<BinaryHeap<TimerEntry>>>,
    ) -> (tokio_current_thread::Handle, IOContextShutdown) {
        let handle = reactor.handle().clone();
        let shutdown = Arc::new(atomic::AtomicUsize::new(RUNNING));
        let shutdown_clone = shutdown.clone();
        let name_clone = name.into();

        let mut runner = IOContextRunner {
            shutdown: shutdown_clone,
            name: name_clone,
        };

        let (sender, receiver) = oneshot::channel();

        let join = thread::spawn(move || {
            runner.run(wait, reactor, sender, timers);
        });

        let shutdown = IOContextShutdown {
            name: name.into(),
            shutdown,
            handle,
            join: Some(join),
        };

        let runtime_handle =
            tokio_current_thread::block_on_all(receiver).expect("Runtime init failed");

        (runtime_handle, shutdown)
    }

    fn run(
        &mut self,
        wait: u32,
        reactor: tokio_net::driver::Reactor,
        sender: oneshot::Sender<tokio_current_thread::Handle>,
        timers: Arc<Mutex<BinaryHeap<TimerEntry>>>,
    ) {
        use std::time::{Duration, Instant};

        gst_debug!(CONTEXT_CAT, "Started reactor thread '{}'", self.name);

        let wait = Duration::from_millis(wait as u64);

        let handle = reactor.handle();
        let timer = tokio_timer::Timer::new(reactor);
        let timer_handle = timer.handle();

        let mut current_thread = tokio_current_thread::CurrentThread::new_with_park(timer);

        sender
            .send(current_thread.handle())
            .expect("Couldn't send Runtime handle");

        let _timer_guard = tokio_timer::set_default(&timer_handle);
        let _reactor_guard = tokio_net::driver::set_default(&handle);

        let mut now = Instant::now();

        loop {
            if self.shutdown.load(atomic::Ordering::SeqCst) > RUNNING {
                break;
            }

            gst_trace!(CONTEXT_CAT, "Elapsed {:?} since last loop", now.elapsed());

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

            gst_trace!(CONTEXT_CAT, "Turning current thread '{}'", self.name);
            while current_thread
                .turn(Some(time::Duration::from_millis(0)))
                .unwrap()
                .has_polled()
            {}
            gst_trace!(CONTEXT_CAT, "Turned current thread '{}'", self.name);

            // We have to check again after turning in case we're supposed to shut down now
            // and already handled the unpark above
            if self.shutdown.load(atomic::Ordering::SeqCst) > RUNNING {
                gst_debug!(CONTEXT_CAT, "Shutting down loop");
                break;
            }

            let elapsed = now.elapsed();
            gst_trace!(CONTEXT_CAT, "Elapsed {:?} after handling futures", elapsed);

            if wait == time::Duration::from_millis(0) {
                let timers = timers.lock().unwrap();
                let wait = match timers.peek().map(|entry| entry.time) {
                    None => None,
                    Some(time) => Some({
                        let tmp = time::Instant::now();

                        if time < tmp {
                            time::Duration::from_millis(0)
                        } else {
                            time.duration_since(tmp)
                        }
                    }),
                };
                drop(timers);

                gst_trace!(CONTEXT_CAT, "Sleeping for up to {:?}", wait);
                current_thread.turn(wait).unwrap();
                gst_trace!(CONTEXT_CAT, "Slept for {:?}", now.elapsed());
                now = time::Instant::now();
            } else {
                if elapsed < wait {
                    gst_trace!(
                        CONTEXT_CAT,
                        "Waiting for {:?} before polling again",
                        wait - elapsed
                    );
                    thread::sleep(wait - elapsed);
                    gst_trace!(CONTEXT_CAT, "Slept for {:?}", now.elapsed());
                }

                now += wait;
            }
        }
    }
}

impl Drop for IOContextRunner {
    fn drop(&mut self) {
        gst_debug!(CONTEXT_CAT, "Shut down reactor thread '{}'", self.name);
    }
}

struct IOContextShutdown {
    name: String,
    shutdown: Arc<atomic::AtomicUsize>,
    handle: tokio_net::driver::Handle,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for IOContextShutdown {
    fn drop(&mut self) {
        use tokio_executor::park::Unpark;

        gst_debug!(CONTEXT_CAT, "Shutting down reactor thread '{}'", self.name);
        self.shutdown.store(SHUTDOWN_NOW, atomic::Ordering::SeqCst);
        gst_trace!(CONTEXT_CAT, "Waiting for reactor '{}' shutdown", self.name);
        // After being unparked, the next turn() is guaranteed to finish immediately,
        // as such there is no race condition between checking for shutdown and setting
        // shutdown.
        self.handle.unpark();
        let _ = self.join.take().unwrap().join();
    }
}

#[derive(Clone)]
pub struct IOContext(Arc<IOContextInner>);

impl glib::subclass::boxed::BoxedType for IOContext {
    const NAME: &'static str = "TsIOContext";

    glib_boxed_type!();
}

glib_boxed_derive_traits!(IOContext);

pub type PendingFuturesOutput = Result<(), gst::FlowError>;
type PendingFutureQueue = FuturesUnordered<BoxFuture<'static, PendingFuturesOutput>>;

struct IOContextInner {
    name: String,
    runtime_handle: Mutex<tokio_current_thread::Handle>,
    reactor_handle: tokio_net::driver::Handle,
    timers: Arc<Mutex<BinaryHeap<TimerEntry>>>,
    // Only used for dropping
    _shutdown: IOContextShutdown,
    pending_futures: Mutex<(u64, HashMap<u64, PendingFutureQueue>)>,
}

impl Drop for IOContextInner {
    fn drop(&mut self) {
        let mut contexts = CONTEXTS.lock().unwrap();
        gst_debug!(CONTEXT_CAT, "Finalizing context '{}'", self.name);
        contexts.remove(&self.name);
    }
}

impl IOContext {
    pub fn new(name: &str, wait: u32) -> Result<Self, io::Error> {
        let mut contexts = CONTEXTS.lock().unwrap();
        if let Some(context) = contexts.get(name) {
            if let Some(context) = context.upgrade() {
                gst_debug!(CONTEXT_CAT, "Reusing existing context '{}'", name);
                return Ok(IOContext(context));
            }
        }

        let reactor = tokio_net::driver::Reactor::new()?;
        let reactor_handle = reactor.handle().clone();

        let timers = Arc::new(Mutex::new(BinaryHeap::new()));

        let (runtime_handle, shutdown) =
            IOContextRunner::start(name, wait, reactor, timers.clone());

        let context = Arc::new(IOContextInner {
            name: name.into(),
            runtime_handle: Mutex::new(runtime_handle),
            reactor_handle,
            timers,
            _shutdown: shutdown,
            pending_futures: Mutex::new((0, HashMap::new())),
        });
        contexts.insert(name.into(), Arc::downgrade(&context));

        gst_debug!(CONTEXT_CAT, "Created new context '{}'", name);
        Ok(IOContext(context))
    }

    pub fn spawn<Fut>(&self, future: Fut)
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.0.runtime_handle.lock().unwrap().spawn(future).unwrap();
    }

    pub fn reactor_handle(&self) -> &tokio_net::driver::Handle {
        &self.0.reactor_handle
    }

    pub fn acquire_pending_future_id(&self) -> PendingFutureId {
        let mut pending_futures = self.0.pending_futures.lock().unwrap();
        let id = pending_futures.0;
        pending_futures.0 += 1;
        pending_futures.1.insert(id, FuturesUnordered::new());

        PendingFutureId(id)
    }

    pub fn release_pending_future_id(&self, id: PendingFutureId) {
        let mut pending_futures = self.0.pending_futures.lock().unwrap();
        if let Some(fs) = pending_futures.1.remove(&id.0) {
            self.spawn(fs.try_for_each(|_| future::ok(())).map(|_| ()));
        }
    }

    pub fn add_pending_future<F>(&self, id: PendingFutureId, future: F)
    where
        F: Future<Output = PendingFuturesOutput> + Send + 'static,
    {
        let mut pending_futures = self.0.pending_futures.lock().unwrap();
        let fs = pending_futures.1.get_mut(&id.0).unwrap();
        fs.push(future.boxed())
    }

    pub fn drain_pending_futures(
        &self,
        id: PendingFutureId,
    ) -> (
        Option<AbortHandle>,
        future::Either<
            BoxFuture<'static, PendingFuturesOutput>,
            future::Ready<PendingFuturesOutput>,
        >,
    ) {
        let mut pending_futures = self.0.pending_futures.lock().unwrap();
        let fs = pending_futures.1.get_mut(&id.0).unwrap();

        let pending_futures = mem::replace(fs, FuturesUnordered::new());

        if !pending_futures.is_empty() {
            gst_log!(
                CONTEXT_CAT,
                "Scheduling {} pending futures for context '{}' with pending future id {:?}",
                pending_futures.len(),
                self.0.name,
                id,
            );

            let (abort_handle, abort_registration) = AbortHandle::new_pair();

            let abortable = Abortable::new(
                pending_futures.try_for_each(|_| future::ok(())),
                abort_registration,
            )
            .map(|res| {
                res.unwrap_or_else(|_| {
                    gst_trace!(CONTEXT_CAT, "Aborting");

                    Err(gst::FlowError::Flushing)
                })
            })
            .boxed()
            .left_future();

            (Some(abort_handle), abortable)
        } else {
            (None, future::ok(()).right_future())
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
pub struct PendingFutureId(u64);

impl glib::subclass::boxed::BoxedType for PendingFutureId {
    const NAME: &'static str = "TsPendingFutureId";

    glib_boxed_type!();
}

glib_boxed_derive_traits!(PendingFutureId);

static TIMER_ENTRY_ID: atomic::AtomicUsize = atomic::AtomicUsize::new(0);

// Ad-hoc interval timer implementation for our throttled event loop above
pub struct TimerEntry {
    time: time::Instant,
    id: usize, // for producing a total order
    interval: Option<time::Duration>,
    sender: mpsc::UnboundedSender<()>,
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

#[allow(unused)]
pub struct Interval {
    receiver: mpsc::UnboundedReceiver<()>,
}

impl Interval {
    #[allow(unused)]
    pub fn new(context: &IOContext, interval: time::Duration) -> Self {
        use tokio_executor::park::Unpark;

        let (sender, receiver) = mpsc::unbounded();

        let mut timers = context.0.timers.lock().unwrap();
        let entry = TimerEntry {
            time: time::Instant::now(),
            id: TIMER_ENTRY_ID.fetch_add(1, atomic::Ordering::Relaxed),
            interval: Some(interval),
            sender,
        };
        timers.push(entry);
        context.reactor_handle().unpark();

        Self { receiver }
    }
}

impl Stream for Interval {
    type Item = ();

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_next_unpin(cx)
    }
}

pub struct Timeout {
    receiver: mpsc::UnboundedReceiver<()>,
}

impl Timeout {
    pub fn new(context: &IOContext, timeout: time::Duration) -> Self {
        let (sender, receiver) = mpsc::unbounded();

        let mut timers = context.0.timers.lock().unwrap();
        let entry = TimerEntry {
            time: time::Instant::now() + timeout,
            id: TIMER_ENTRY_ID.fetch_add(1, atomic::Ordering::Relaxed),
            interval: None,
            sender,
        };
        timers.push(entry);

        Self { receiver }
    }
}

impl Future for Timeout {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        match ready!(self.receiver.poll_next_unpin(cx)) {
            Some(_) => Poll::Ready(()),
            None => unreachable!(),
        }
    }
}
