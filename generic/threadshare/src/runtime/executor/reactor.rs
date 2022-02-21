// This is based on https://github.com/smol-rs/async-io
// with adaptations by:
//
// Copyright (C) 2021 François Laignel <fengalin@free.fr>
//
// Take a look at the license at the top of the repository in the LICENSE file.

use concurrent_queue::ConcurrentQueue;
use futures::ready;
use polling::{Event, Poller};
use slab::Slab;

use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::mem;
#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawSocket;
use std::panic;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::runtime::{Async, RUNTIME_CAT};

const READ: usize = 0;
const WRITE: usize = 1;

thread_local! {
    static CURRENT_REACTOR: RefCell<Option<Reactor>> = RefCell::new(None);
}

#[derive(Debug)]
pub(super) struct Reactor {
    /// Portable bindings to epoll/kqueue/event ports/wepoll.
    ///
    /// This is where I/O is polled, producing I/O events.
    poller: Poller,

    /// Ticker bumped before polling.
    ///
    /// This is useful for checking what is the current "round" of `ReactorLock::react()` when
    /// synchronizing things in `Source::readable()` and `Source::writable()`. Both of those
    /// methods must make sure they don't receive stale I/O events - they only accept events from a
    /// fresh "round" of `ReactorLock::react()`.
    ticker: AtomicUsize,

    /// Half max throttling duration, needed to fire timers.
    half_max_throttling: Duration,

    /// Registered sources.
    sources: Slab<Arc<Source>>,

    /// Temporary storage for I/O events when polling the reactor.
    ///
    /// Holding a lock on this event list implies the exclusive right to poll I/O.
    events: Vec<Event>,

    /// An ordered map of registered timers.
    ///
    /// Timers are in the order in which they fire. The `usize` in this type is a timer ID used to
    /// distinguish timers that fire at the same time. The `Waker` represents the task awaiting the
    /// timer.
    timers: BTreeMap<(Instant, usize), Waker>,

    /// A queue of timer operations (insert and remove).
    ///
    /// When inserting or removing a timer, we don't process it immediately - we just push it into
    /// this queue. Timers actually get processed when the queue fills up or the reactor is polled.
    timer_ops: ConcurrentQueue<TimerOp>,
}

impl Reactor {
    fn new(max_throttling: Duration) -> Self {
        Reactor {
            poller: Poller::new().expect("cannot initialize I/O event notification"),
            ticker: AtomicUsize::new(0),
            half_max_throttling: max_throttling / 2 + Duration::from_nanos(1),
            sources: Slab::new(),
            events: Vec::new(),
            timers: BTreeMap::new(),
            timer_ops: ConcurrentQueue::bounded(1000),
        }
    }

    /// Initializes the reactor for current thread.
    pub fn init(max_throttling: Duration) {
        CURRENT_REACTOR.with(|cur| {
            let mut cur = cur.borrow_mut();
            if cur.is_none() {
                *cur = Some(Reactor::new(max_throttling));
            }
        })
    }

    pub fn close() {
        let _ = CURRENT_REACTOR.try_with(|cur_reactor| {
            *cur_reactor.borrow_mut() = None;
        });
    }

    /// Executes the function with current thread's reactor as ref.
    ///
    /// # Panics
    ///
    /// Panics if:
    ///
    /// - The Reactor is not initialized, i.e. if
    ///   current thread is not a [`Context`] thread.
    /// - The Reactor is already mutably borrowed.
    ///
    /// Use [`Context::enter`] to register i/o sources
    /// or timers from a different thread.
    #[track_caller]
    pub fn with<F, R>(f: F) -> R
    where
        F: FnOnce(&Reactor) -> R,
    {
        CURRENT_REACTOR.with(|reactor| {
            f(reactor
                .borrow()
                .as_ref()
                .expect("Not running in a Context."))
        })
    }

    /// Executes the function with current thread's reactor as mutable.
    ///
    /// # Panics
    ///
    /// Panics if:
    ///
    /// - The Reactor is not initialized, i.e. if
    ///   current thread is not a [`Context`] thread.
    /// - The Reactor is already mutably borrowed.
    ///
    /// Use [`Context::enter`] to register i/o sources
    /// or timers from a different thread.
    #[track_caller]
    pub fn with_mut<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Reactor) -> R,
    {
        CURRENT_REACTOR.with(|reactor| {
            f(reactor
                .borrow_mut()
                .as_mut()
                .expect("Not running in a Context."))
        })
    }

    /// Returns the current ticker.
    pub fn ticker(&self) -> usize {
        self.ticker.load(Ordering::SeqCst)
    }

    pub fn half_max_throttling(&self) -> Duration {
        self.half_max_throttling
    }

    /// Registers an I/O source in the reactor.
    pub fn insert_io(
        &mut self,
        #[cfg(unix)] raw: RawFd,
        #[cfg(windows)] raw: RawSocket,
    ) -> io::Result<Arc<Source>> {
        // Create an I/O source for this file descriptor.
        let source = {
            let key = self.sources.vacant_entry().key();
            let source = Arc::new(Source {
                raw,
                key,
                state: Default::default(),
            });
            self.sources.insert(source.clone());
            source
        };

        // Register the file descriptor.
        if let Err(err) = self.poller.add(raw, Event::none(source.key)) {
            self.sources.remove(source.key);
            return Err(err);
        }

        Ok(source)
    }

    /// Deregisters an I/O source from the reactor.
    pub fn remove_io(&mut self, source: &Source) -> io::Result<()> {
        self.sources.remove(source.key);
        self.poller.delete(source.raw)
    }

    /// Registers a timer in the reactor.
    ///
    /// Returns the inserted timer's ID.
    pub fn insert_timer(&mut self, when: Instant, waker: &Waker) -> usize {
        // Generate a new timer ID.
        static ID_GENERATOR: AtomicUsize = AtomicUsize::new(1);
        let id = ID_GENERATOR.fetch_add(1, Ordering::Relaxed);

        // Push an insert operation.
        while self
            .timer_ops
            .push(TimerOp::Insert(when, id, waker.clone()))
            .is_err()
        {
            // If the queue is full, drain it and try again.
            gst::warning!(RUNTIME_CAT, "react: timer_ops is full");
            self.process_timer_ops();
        }

        id
    }

    /// Deregisters a timer from the reactor.
    pub fn remove_timer(&mut self, when: Instant, id: usize) {
        // Push a remove operation.
        while self.timer_ops.push(TimerOp::Remove(when, id)).is_err() {
            gst::warning!(RUNTIME_CAT, "react: timer_ops is full");
            // If the queue is full, drain it and try again.
            self.process_timer_ops();
        }
    }

    /// Processes ready timers and extends the list of wakers to wake.
    ///
    /// Returns the duration until the next timer before this method was called.
    fn process_timers(&mut self, wakers: &mut Vec<Waker>) {
        self.process_timer_ops();

        let now = Instant::now();

        // Split timers into ready and pending timers.
        //
        // Careful to split just *after* `now`, so that a timer set for exactly `now` is considered
        // ready.
        let pending = self.timers.split_off(&(now + self.half_max_throttling, 0));
        let ready = mem::replace(&mut self.timers, pending);

        // Add wakers to the list.
        if !ready.is_empty() {
            gst::trace!(RUNTIME_CAT, "process_timers: {} ready wakers", ready.len());

            for (_, waker) in ready {
                wakers.push(waker);
            }
        }
    }

    /// Processes queued timer operations.
    fn process_timer_ops(&mut self) {
        // Process only as much as fits into the queue, or else this loop could in theory run
        // forever.
        for _ in 0..self.timer_ops.capacity().unwrap() {
            match self.timer_ops.pop() {
                Ok(TimerOp::Insert(when, id, waker)) => {
                    self.timers.insert((when, id), waker);
                }
                Ok(TimerOp::Remove(when, id)) => {
                    self.timers.remove(&(when, id));
                }
                Err(_) => break,
            }
        }
    }

    /// Processes new events.
    pub fn react(&mut self) -> io::Result<()> {
        let mut wakers = Vec::new();

        // Process ready timers.
        self.process_timers(&mut wakers);

        // Bump the ticker before polling I/O.
        let tick = self.ticker.fetch_add(1, Ordering::SeqCst).wrapping_add(1);

        self.events.clear();

        // Block on I/O events.
        let res = match self.poller.wait(&mut self.events, Some(Duration::ZERO)) {
            // No I/O events occurred.
            Ok(0) => Ok(()),
            // At least one I/O event occurred.
            Ok(_) => {
                for ev in self.events.iter() {
                    // Check if there is a source in the table with this key.
                    if let Some(source) = self.sources.get(ev.key) {
                        let mut state = source.state.lock().unwrap();

                        // Collect wakers if a writability event was emitted.
                        for &(dir, emitted) in &[(WRITE, ev.writable), (READ, ev.readable)] {
                            if emitted {
                                state[dir].tick = tick;
                                state[dir].drain_into(&mut wakers);
                            }
                        }

                        // Re-register if there are still writers or readers. The can happen if
                        // e.g. we were previously interested in both readability and writability,
                        // but only one of them was emitted.
                        if !state[READ].is_empty() || !state[WRITE].is_empty() {
                            self.poller.modify(
                                source.raw,
                                Event {
                                    key: source.key,
                                    readable: !state[READ].is_empty(),
                                    writable: !state[WRITE].is_empty(),
                                },
                            )?;
                        }
                    }
                }

                Ok(())
            }

            // The syscall was interrupted.
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(()),

            // An actual error occureed.
            Err(err) => Err(err),
        };

        // Wake up ready tasks.
        if !wakers.is_empty() {
            gst::trace!(RUNTIME_CAT, "react: {} ready wakers", wakers.len());

            for waker in wakers {
                // Don't let a panicking waker blow everything up.
                panic::catch_unwind(|| waker.wake()).ok();
            }
        }

        res
    }
}

/// A single timer operation.
enum TimerOp {
    Insert(Instant, usize, Waker),
    Remove(Instant, usize),
}

/// A registered source of I/O events.
#[derive(Debug)]
pub(super) struct Source {
    /// Raw file descriptor on Unix platforms.
    #[cfg(unix)]
    pub(super) raw: RawFd,

    /// Raw socket handle on Windows.
    #[cfg(windows)]
    pub(super) raw: RawSocket,

    /// The key of this source obtained during registration.
    key: usize,

    /// Inner state with registered wakers.
    state: Mutex<[Direction; 2]>,
}

/// A read or write direction.
#[derive(Debug, Default)]
struct Direction {
    /// Last reactor tick that delivered an event.
    tick: usize,

    /// Ticks remembered by `Async::poll_readable()` or `Async::poll_writable()`.
    ticks: Option<(usize, usize)>,

    /// Waker stored by `Async::poll_readable()` or `Async::poll_writable()`.
    waker: Option<Waker>,

    /// Wakers of tasks waiting for the next event.
    ///
    /// Registered by `Async::readable()` and `Async::writable()`.
    wakers: Slab<Option<Waker>>,
}

impl Direction {
    /// Returns `true` if there are no wakers interested in this direction.
    fn is_empty(&self) -> bool {
        self.waker.is_none() && self.wakers.iter().all(|(_, opt)| opt.is_none())
    }

    /// Moves all wakers into a `Vec`.
    fn drain_into(&mut self, dst: &mut Vec<Waker>) {
        if let Some(w) = self.waker.take() {
            dst.push(w);
        }
        for (_, opt) in self.wakers.iter_mut() {
            if let Some(w) = opt.take() {
                dst.push(w);
            }
        }
    }
}

impl Source {
    /// Polls the I/O source for readability.
    pub fn poll_readable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_ready(READ, cx)
    }

    /// Polls the I/O source for writability.
    pub fn poll_writable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_ready(WRITE, cx)
    }

    /// Registers a waker from `poll_readable()` or `poll_writable()`.
    ///
    /// If a different waker is already registered, it gets replaced and woken.
    fn poll_ready(&self, dir: usize, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.state.lock().unwrap();

        // Check if the reactor has delivered an event.
        if let Some((a, b)) = state[dir].ticks {
            // If `state[dir].tick` has changed to a value other than the old reactor tick,
            // that means a newer reactor tick has delivered an event.
            if state[dir].tick != a && state[dir].tick != b {
                state[dir].ticks = None;
                return Poll::Ready(Ok(()));
            }
        }

        let was_empty = state[dir].is_empty();

        // Register the current task's waker.
        if let Some(w) = state[dir].waker.take() {
            if w.will_wake(cx.waker()) {
                state[dir].waker = Some(w);
                return Poll::Pending;
            }
            // Wake the previous waker because it's going to get replaced.
            panic::catch_unwind(|| w.wake()).ok();
        }

        Reactor::with(|reactor| {
            state[dir].waker = Some(cx.waker().clone());
            state[dir].ticks = Some((reactor.ticker(), state[dir].tick));

            // Update interest in this I/O handle.
            if was_empty {
                reactor.poller.modify(
                    self.raw,
                    Event {
                        key: self.key,
                        readable: !state[READ].is_empty(),
                        writable: !state[WRITE].is_empty(),
                    },
                )?;
            }

            Poll::Pending
        })
    }

    /// Waits until the I/O source is readable.
    pub fn readable<T>(handle: &Async<T>) -> Readable<'_, T> {
        Readable(Self::ready(handle, READ))
    }

    /// Waits until the I/O source is readable.
    pub fn readable_owned<T>(handle: Arc<Async<T>>) -> ReadableOwned<T> {
        ReadableOwned(Self::ready(handle, READ))
    }

    /// Waits until the I/O source is writable.
    pub fn writable<T>(handle: &Async<T>) -> Writable<'_, T> {
        Writable(Self::ready(handle, WRITE))
    }

    /// Waits until the I/O source is writable.
    pub fn writable_owned<T>(handle: Arc<Async<T>>) -> WritableOwned<T> {
        WritableOwned(Self::ready(handle, WRITE))
    }

    /// Waits until the I/O source is readable or writable.
    fn ready<H: Borrow<Async<T>> + Clone, T>(handle: H, dir: usize) -> Ready<H, T> {
        Ready {
            handle,
            dir,
            ticks: None,
            index: None,
            _guard: None,
        }
    }
}

/// Future for [`Async::readable`](crate::runtime::Async::readable).
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Readable<'a, T>(Ready<&'a Async<T>, T>);

impl<T> Future for Readable<'_, T> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        ready!(Pin::new(&mut self.0).poll(cx))?;
        gst::trace!(RUNTIME_CAT, "readable: fd={}", self.0.handle.source.raw);
        Poll::Ready(Ok(()))
    }
}

impl<T> fmt::Debug for Readable<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Readable").finish()
    }
}

/// Future for [`Async::readable_owned`](crate::runtime::Async::readable_owned).
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct ReadableOwned<T>(Ready<Arc<Async<T>>, T>);

impl<T> Future for ReadableOwned<T> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        ready!(Pin::new(&mut self.0).poll(cx))?;
        gst::trace!(
            RUNTIME_CAT,
            "readable_owned: fd={}",
            self.0.handle.source.raw
        );
        Poll::Ready(Ok(()))
    }
}

impl<T> fmt::Debug for ReadableOwned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadableOwned").finish()
    }
}

/// Future for [`Async::writable`](crate::runtime::Async::writable).
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Writable<'a, T>(Ready<&'a Async<T>, T>);

impl<T> Future for Writable<'_, T> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        ready!(Pin::new(&mut self.0).poll(cx))?;
        gst::trace!(RUNTIME_CAT, "writable: fd={}", self.0.handle.source.raw);
        Poll::Ready(Ok(()))
    }
}

impl<T> fmt::Debug for Writable<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Writable").finish()
    }
}

/// Future for [`Async::writable_owned`](crate::runtime::Async::writable_owned).
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct WritableOwned<T>(Ready<Arc<Async<T>>, T>);

impl<T> Future for WritableOwned<T> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        ready!(Pin::new(&mut self.0).poll(cx))?;
        gst::trace!(
            RUNTIME_CAT,
            "writable_owned: fd={}",
            self.0.handle.source.raw
        );
        Poll::Ready(Ok(()))
    }
}

impl<T> fmt::Debug for WritableOwned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WritableOwned").finish()
    }
}

struct Ready<H: Borrow<Async<T>>, T> {
    handle: H,
    dir: usize,
    ticks: Option<(usize, usize)>,
    index: Option<usize>,
    _guard: Option<RemoveOnDrop<H, T>>,
}

impl<H: Borrow<Async<T>>, T> Unpin for Ready<H, T> {}

impl<H: Borrow<Async<T>> + Clone, T> Future for Ready<H, T> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Self {
            ref handle,
            dir,
            ticks,
            index,
            _guard,
            ..
        } = &mut *self;

        let mut state = handle.borrow().source.state.lock().unwrap();

        // Check if the reactor has delivered an event.
        if let Some((a, b)) = *ticks {
            // If `state[dir].tick` has changed to a value other than the old reactor tick,
            // that means a newer reactor tick has delivered an event.
            if state[*dir].tick != a && state[*dir].tick != b {
                return Poll::Ready(Ok(()));
            }
        }

        let was_empty = state[*dir].is_empty();
        Reactor::with(|reactor| {
            // Register the current task's waker.
            let i = match *index {
                Some(i) => i,
                None => {
                    let i = state[*dir].wakers.insert(None);
                    *_guard = Some(RemoveOnDrop {
                        handle: handle.clone(),
                        dir: *dir,
                        key: i,
                        _marker: PhantomData,
                    });
                    *index = Some(i);
                    *ticks = Some((reactor.ticker(), state[*dir].tick));
                    i
                }
            };
            state[*dir].wakers[i] = Some(cx.waker().clone());

            // Update interest in this I/O handle.
            if was_empty {
                reactor.poller.modify(
                    handle.borrow().source.raw,
                    Event {
                        key: handle.borrow().source.key,
                        readable: !state[READ].is_empty(),
                        writable: !state[WRITE].is_empty(),
                    },
                )?;
            }

            Poll::Pending
        })
    }
}

/// Remove waker when dropped.
struct RemoveOnDrop<H: Borrow<Async<T>>, T> {
    handle: H,
    dir: usize,
    key: usize,
    _marker: PhantomData<fn() -> T>,
}

impl<H: Borrow<Async<T>>, T> Drop for RemoveOnDrop<H, T> {
    fn drop(&mut self) {
        let mut state = self.handle.borrow().source.state.lock().unwrap();
        let wakers = &mut state[self.dir].wakers;
        if wakers.contains(self.key) {
            wakers.remove(self.key);
        }
    }
}
