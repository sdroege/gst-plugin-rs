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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::sync::atomic;
use std::thread;
use std::io;

use futures::Future;
use tokio::executor::thread_pool;
use tokio::reactor;

use gst;

use either::Either;

lazy_static!{
    static ref CONTEXTS: Mutex<HashMap<String, Weak<IOContextInner>>> = Mutex::new(HashMap::new());
    static ref CONTEXT_CAT: gst::DebugCategory = gst::DebugCategory::new(
                "ts-context",
                gst::DebugColorFlags::empty(),
                "Thread-sharing Context",
            );
}

// Our own simplified implementation of reactor::Background to allow hooking into its internals
const RUNNING: usize = 0;
const SHUTDOWN_NOW: usize = 1;

struct IOContextRunner {
    name: String,
    shutdown: Arc<atomic::AtomicUsize>,
    pending_futures: Option<Arc<Mutex<Vec<Box<Future<Item = (), Error = ()> + Send + 'static>>>>>,
}

impl IOContextRunner {
    fn start_single_threaded(
        name: &str,
        wait: u32,
        reactor: reactor::Reactor,
    ) -> (IOContextExecutor, IOContextShutdown) {
        let handle = reactor.handle().clone();
        let handle2 = reactor.handle().clone();
        let shutdown = Arc::new(atomic::AtomicUsize::new(RUNNING));
        let shutdown_clone = shutdown.clone();
        let name_clone = name.into();

        let pending_futures = Arc::new(Mutex::new(Vec::new()));
        let pending_futures_clone = pending_futures.clone();

        let mut runner = IOContextRunner {
            shutdown: shutdown_clone,
            name: name_clone,
            pending_futures: Some(pending_futures),
        };
        let join = thread::spawn(move || {
            runner.run(wait, reactor);
        });

        let executor = IOContextExecutor {
            handle: handle,
            pending_futures: pending_futures_clone,
        };

        let shutdown = IOContextShutdown {
            name: name.into(),
            shutdown: shutdown,
            handle: handle2,
            join: Some(join),
        };

        (executor, shutdown)
    }

    fn start(name: &str, wait: u32, reactor: reactor::Reactor) -> IOContextShutdown {
        let handle = reactor.handle().clone();
        let shutdown = Arc::new(atomic::AtomicUsize::new(RUNNING));
        let shutdown_clone = shutdown.clone();
        let name_clone = name.into();

        let mut runner = IOContextRunner {
            shutdown: shutdown_clone,
            name: name_clone,
            pending_futures: None,
        };
        let join = thread::spawn(move || {
            runner.run(wait, reactor);
        });

        let shutdown = IOContextShutdown {
            name: name.into(),
            shutdown: shutdown,
            handle: handle,
            join: Some(join),
        };

        shutdown
    }

    fn run(&mut self, wait: u32, reactor: reactor::Reactor) {
        use std::time;
        let wait = time::Duration::from_millis(wait as u64);

        gst_debug!(CONTEXT_CAT, "Started reactor thread '{}'", self.name);

        if let Some(ref pending_futures) = self.pending_futures {
            use tokio::executor::current_thread;

            reactor.set_fallback().unwrap();
            let handle = reactor.handle();
            let mut enter = ::tokio_executor::enter().unwrap();
            let mut current_thread = current_thread::CurrentThread::new_with_park(reactor);

            ::tokio_reactor::with_default(&handle, &mut enter, |enter| loop {
                let now = time::Instant::now();

                if self.shutdown.load(atomic::Ordering::SeqCst) > RUNNING {
                    break;
                }

                {
                    let mut pending_futures = pending_futures.lock().unwrap();
                    while let Some(future) = pending_futures.pop() {
                        current_thread.spawn(future);
                    }
                }

                gst_trace!(CONTEXT_CAT, "Turning current thread '{}'", self.name);
                current_thread.enter(enter).turn(None).unwrap();
                gst_trace!(CONTEXT_CAT, "Turned current thread '{}'", self.name);

                let elapsed = now.elapsed();
                if elapsed < wait {
                    gst_trace!(
                        CONTEXT_CAT,
                        "Waiting for {:?} before polling again",
                        wait - elapsed
                    );
                    thread::sleep(wait - elapsed);
                }
            });
        } else {
            let mut reactor = reactor;

            loop {
                let now = time::Instant::now();

                if self.shutdown.load(atomic::Ordering::SeqCst) > RUNNING {
                    break;
                }

                gst_trace!(CONTEXT_CAT, "Turning reactor '{}'", self.name);
                reactor.turn(None).unwrap();
                gst_trace!(CONTEXT_CAT, "Turned reactor '{}'", self.name);

                let elapsed = now.elapsed();
                if elapsed < wait {
                    gst_trace!(
                        CONTEXT_CAT,
                        "Waiting for {:?} before polling again",
                        wait - elapsed
                    );
                    thread::sleep(wait - elapsed);
                }
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
    handle: reactor::Handle,
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

struct IOContextExecutor {
    handle: reactor::Handle,
    pending_futures: Arc<Mutex<Vec<Box<Future<Item = (), Error = ()> + Send + 'static>>>>,
}

impl IOContextExecutor {
    fn spawn<F>(&self, future: F)
    where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        use tokio_executor::park::Unpark;

        self.pending_futures.lock().unwrap().push(Box::new(future));
        self.handle.unpark();
    }
}

#[derive(Clone)]
pub struct IOContext(Arc<IOContextInner>);

struct IOContextInner {
    name: String,
    pool: Either<thread_pool::ThreadPool, IOContextExecutor>,
    // Only used for dropping
    _shutdown: IOContextShutdown,
}

impl Drop for IOContextInner {
    fn drop(&mut self) {
        let mut contexts = CONTEXTS.lock().unwrap();
        gst_debug!(CONTEXT_CAT, "Finalizing context '{}'", self.name);
        contexts.remove(&self.name);
    }
}

impl IOContext {
    pub fn new(name: &str, n_threads: isize, wait: u32) -> Result<Self, io::Error> {
        let mut contexts = CONTEXTS.lock().unwrap();
        if let Some(context) = contexts.get(name) {
            if let Some(context) = context.upgrade() {
                gst_debug!(CONTEXT_CAT, "Reusing existing context '{}'", name);
                return Ok(IOContext(context));
            }
        }

        let reactor = reactor::Reactor::new()?;

        let (pool, shutdown) = if n_threads >= 0 {
            let handle = reactor.handle().clone();

            let shutdown = IOContextRunner::start(name, wait, reactor);

            let mut pool_builder = thread_pool::Builder::new();
            pool_builder.around_worker(move |w, enter| {
                ::tokio_reactor::with_default(&handle, enter, |_| {
                    w.run();
                });
            });

            if n_threads > 0 {
                pool_builder.pool_size(n_threads as usize);
            }
            (Either::Left(pool_builder.build()), shutdown)
        } else {
            let (executor, shutdown) = IOContextRunner::start_single_threaded(name, wait, reactor);

            (Either::Right(executor), shutdown)
        };

        let context = Arc::new(IOContextInner {
            name: name.into(),
            pool,
            _shutdown: shutdown,
        });
        contexts.insert(name.into(), Arc::downgrade(&context));

        gst_debug!(CONTEXT_CAT, "Created new context '{}'", name);
        Ok(IOContext(context))
    }

    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        match self.0.pool {
            Either::Left(ref pool) => pool.spawn(future),
            Either::Right(ref pool) => pool.spawn(future),
        }
    }
}
