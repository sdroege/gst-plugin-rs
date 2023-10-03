// Copyright (C) 2018-2020 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2019-2022 François Laignel <fengalin@free.fr>
//
// Take a look at the license at the top of the repository in the LICENSE file.

use futures::future::poll_fn;
use futures::pin_mut;

use gio::glib::clone::Downgrade;

use std::cell::RefCell;
use std::future::Future;
use std::panic;
#[cfg(feature = "tuning")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as sync_mpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::task::Poll;
use std::thread;
use std::time::{Duration, Instant};

use waker_fn::waker_fn;

use super::task::{SubTaskOutput, TaskId, TaskQueue};
use super::{CallOnDrop, JoinHandle, Reactor};
use crate::runtime::RUNTIME_CAT;

thread_local! {
    static CURRENT_SCHEDULER: RefCell<Option<HandleWeak>> = RefCell::new(None);
}

#[derive(Debug)]
pub(super) struct Scheduler {
    context_name: Arc<str>,
    max_throttling: Duration,
    tasks: TaskQueue,
    must_unpark: Mutex<bool>,
    must_unpark_cvar: Condvar,
    #[cfg(feature = "tuning")]
    parked_duration: AtomicU64,
}

impl Scheduler {
    pub const DUMMY_NAME: &'static str = "DUMMY";
    const MAX_SUCCESSIVE_TASKS: usize = 64;

    pub fn start(context_name: &str, max_throttling: Duration) -> Handle {
        // Name the thread so that it appears in panic messages.
        let thread = thread::Builder::new().name(context_name.to_string());

        let (handle_sender, handle_receiver) = sync_mpsc::channel();
        let context_name = Arc::from(context_name);
        let thread_ctx_name = Arc::clone(&context_name);
        let join = thread
            .spawn(move || {
                gst::debug!(
                    RUNTIME_CAT,
                    "Started Scheduler thread for Context {}",
                    thread_ctx_name
                );

                let handle = Scheduler::init(Arc::clone(&thread_ctx_name), max_throttling);
                let this = Arc::clone(&handle.0.scheduler);
                let must_shutdown = handle.0.must_shutdown.clone();
                let handle_weak = handle.downgrade();
                handle_sender.send(handle).unwrap();

                let shutdown_fut = poll_fn(move |_| {
                    if must_shutdown.load(Ordering::SeqCst) {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                });

                // Blocking on `shutdown_fut` which is cheap to `poll`.
                match this.block_on_priv(shutdown_fut) {
                    Ok(_) => {
                        gst::debug!(
                            RUNTIME_CAT,
                            "Scheduler thread shut down for Context {}",
                            thread_ctx_name
                        );
                    }
                    Err(e) => {
                        gst::error!(
                            RUNTIME_CAT,
                            "Scheduler thread shut down due to an error within Context {}",
                            thread_ctx_name
                        );

                        if let Some(handle) = handle_weak.upgrade() {
                            handle.self_shutdown();
                        }

                        panic::resume_unwind(e);
                    }
                }
            })
            .expect("Failed to spawn Scheduler thread");

        let handle = handle_receiver.recv().expect("Context thread init failed");
        handle.set_join_handle(join);

        handle
    }

    fn init(context_name: Arc<str>, max_throttling: Duration) -> Handle {
        let handle = CURRENT_SCHEDULER.with(|cur_scheduler| {
            let mut cur_scheduler = cur_scheduler.borrow_mut();
            if cur_scheduler.is_some() {
                panic!("Attempt to initialize an Scheduler on thread where another Scheduler is running.");
            }

            let handle = Handle::new(Arc::new(Scheduler {
                context_name: context_name.clone(),
                max_throttling,
                tasks: TaskQueue::new(context_name),
                must_unpark: Mutex::new(false),
                must_unpark_cvar: Condvar::new(),
                #[cfg(feature = "tuning")]
                parked_duration: AtomicU64::new(0),
            }));

            *cur_scheduler = Some(handle.downgrade());

            handle
        });

        Reactor::init(handle.max_throttling());

        handle
    }

    pub fn block_on<F>(future: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        assert!(
            !Scheduler::is_scheduler_thread(),
            "Attempt to block within an existing Scheduler thread."
        );

        let handle = Scheduler::init(Scheduler::DUMMY_NAME.into(), Duration::ZERO);
        let this = Arc::clone(&handle.0.scheduler);

        // Move the (only) handle for this scheduler in the main task.
        let (task_id, task) = this.tasks.add(async move {
            let res = future.await;

            let task_id = TaskId::current().unwrap();
            let _ = handle.drain_sub_tasks(task_id).await;

            res
        });

        gst::trace!(RUNTIME_CAT, "Blocking on current thread with {:?}", task_id);

        let _guard = CallOnDrop::new(|| {
            gst::trace!(
                RUNTIME_CAT,
                "Blocking on current thread with {:?} done",
                task_id,
            );
        });

        // Blocking on `task` which is cheap to `poll`.
        match this.block_on_priv(task) {
            Ok(res) => res,
            Err(e) => {
                gst::error!(
                    RUNTIME_CAT,
                    "Panic blocking on Context {}",
                    &Scheduler::DUMMY_NAME
                );

                panic::resume_unwind(e);
            }
        }
    }

    // Important: the `termination_future` MUST be cheap to poll.
    //
    // Examples of appropriate `termination_future` are:
    //
    // - an `executor::Task` returned by `self.tasks.add(..)`.
    // - a `JoinHandle` returned by `Handle::spawn`.
    // - a custom future with few cycles (ex. checking an `AtomicBool`).
    fn block_on_priv<F>(&self, termination_future: F) -> std::thread::Result<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let waker = waker_fn(|| ());
        let cx = &mut std::task::Context::from_waker(&waker);
        pin_mut!(termination_future);

        let _guard = CallOnDrop::new(|| Scheduler::close(Arc::clone(&self.context_name)));

        let mut now;
        // This is to ensure reactor invocation on the first iteration.
        let mut last_react = Instant::now().checked_sub(self.max_throttling).unwrap();
        let mut tasks_checked;
        'main: loop {
            // Only check I/O and timers every `max_throttling`.
            now = Instant::now();
            if now - last_react >= self.max_throttling {
                last_react = now;
                Reactor::with_mut(|reactor| reactor.react(now).ok());
            }

            if let Poll::Ready(t) = termination_future.as_mut().poll(cx) {
                return Ok(t);
            }

            tasks_checked = 0;
            while tasks_checked < Self::MAX_SUCCESSIVE_TASKS {
                if let Ok(runnable) = self.tasks.pop_runnable() {
                    panic::catch_unwind(|| runnable.run()).map_err(|err| {
                        gst::error!(
                            RUNTIME_CAT,
                            "A task has panicked within Context {}",
                            self.context_name
                        );

                        err
                    })?;

                    tasks_checked += 1;
                } else {
                    let mut must_unpark = self.must_unpark.lock().unwrap();
                    loop {
                        if *must_unpark {
                            *must_unpark = false;
                            continue 'main;
                        }

                        if let Some(parking_duration) =
                            self.max_throttling.checked_sub(last_react.elapsed())
                        {
                            #[cfg(feature = "tuning")]
                            self.parked_duration.fetch_add(
                                parking_duration.subsec_nanos() as u64,
                                Ordering::Relaxed,
                            );

                            let result = self
                                .must_unpark_cvar
                                .wait_timeout(must_unpark, parking_duration)
                                .unwrap();

                            must_unpark = result.0;
                        } else {
                            *must_unpark = false;
                            continue 'main;
                        }
                    }
                }
            }
        }
    }

    fn unpark(&self) {
        let mut must_unpark = self.must_unpark.lock().unwrap();
        *must_unpark = true;
        self.must_unpark_cvar.notify_one();
    }

    fn close(context_name: Arc<str>) {
        gst::trace!(
            RUNTIME_CAT,
            "Closing Scheduler for Context {}",
            context_name,
        );

        Reactor::clear();

        let _ = CURRENT_SCHEDULER.try_with(|cur_scheduler| {
            *cur_scheduler.borrow_mut() = None;
        });
    }

    pub fn is_scheduler_thread() -> bool {
        CURRENT_SCHEDULER.with(|cur_scheduler| cur_scheduler.borrow().is_some())
    }

    pub fn current() -> Option<Handle> {
        CURRENT_SCHEDULER.with(|cur_scheduler| {
            cur_scheduler
                .borrow()
                .as_ref()
                .and_then(HandleWeak::upgrade)
        })
    }

    pub fn is_current(&self) -> bool {
        CURRENT_SCHEDULER.with(|cur_scheduler| {
            cur_scheduler
                .borrow()
                .as_ref()
                .and_then(HandleWeak::upgrade)
                .map_or(false, |cur| {
                    std::ptr::eq(self, Arc::as_ptr(&cur.0.scheduler))
                })
        })
    }
}

#[derive(Debug)]
struct HandleInner {
    scheduler: Arc<Scheduler>,
    must_shutdown: Arc<AtomicBool>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}

impl HandleInner {
    fn new(scheduler: Arc<Scheduler>) -> Self {
        HandleInner {
            scheduler,
            must_shutdown: Default::default(),
            join: Default::default(),
        }
    }
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        if !self.must_shutdown.fetch_or(true, Ordering::SeqCst) {
            // Was not already shutting down.
            self.scheduler.unpark();

            gst::trace!(
                RUNTIME_CAT,
                "Shutting down Scheduler thread for Context {}",
                self.scheduler.context_name
            );

            // Don't block shutting down itself
            if !self.scheduler.is_current() {
                if let Some(join_handler) = self.join.lock().unwrap().take() {
                    gst::trace!(
                        RUNTIME_CAT,
                        "Waiting for Scheduler thread to shutdown for Context {}",
                        self.scheduler.context_name
                    );

                    let _ = join_handler.join();
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct HandleWeak(Weak<HandleInner>);

impl HandleWeak {
    pub(super) fn upgrade(&self) -> Option<Handle> {
        self.0.upgrade().map(Handle)
    }
}

#[derive(Clone, Debug)]
pub(super) struct Handle(Arc<HandleInner>);

impl Handle {
    fn new(scheduler: Arc<Scheduler>) -> Self {
        Handle(Arc::new(HandleInner::new(scheduler)))
    }

    fn set_join_handle(&self, join: thread::JoinHandle<()>) {
        *self.0.join.lock().unwrap() = Some(join);
    }

    fn self_shutdown(self) {
        self.0.must_shutdown.store(true, Ordering::SeqCst);
        *self.0.join.lock().unwrap() = None;
    }

    pub fn context_name(&self) -> &str {
        &self.0.scheduler.context_name
    }

    pub fn max_throttling(&self) -> Duration {
        self.0.scheduler.max_throttling
    }

    #[cfg(feature = "tuning")]
    pub fn parked_duration(&self) -> Duration {
        Duration::from_nanos(self.0.scheduler.parked_duration.load(Ordering::Relaxed))
    }

    /// Executes the provided function relatively to this [`Scheduler`]'s [`Reactor`].
    ///
    /// Useful to initialize i/o sources and timers from outside
    /// of a [`Scheduler`].
    ///
    /// # Panic
    ///
    /// This will block current thread and would panic if run
    /// from the [`Scheduler`].
    pub fn enter<'a, F, O>(&'a self, f: F) -> O
    where
        F: FnOnce() -> O + Send + 'a,
        O: Send + 'a,
    {
        assert!(!self.0.scheduler.is_current());

        // Safety: bounding `self` to `'a` and blocking on the task
        // ensures that the lifetime bounds satisfy the safety
        // requirements for `TaskQueue::add_sync`.
        let task = unsafe { self.0.scheduler.tasks.add_sync(f) };
        self.0.scheduler.unpark();
        futures::executor::block_on(task)
    }

    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (task_id, task) = self.0.scheduler.tasks.add(future);
        JoinHandle::new(task_id, task, self)
    }

    pub fn spawn_and_unpark<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (task_id, task) = self.0.scheduler.tasks.add(future);
        self.0.scheduler.unpark();
        JoinHandle::new(task_id, task, self)
    }

    pub(super) fn unpark(&self) {
        self.0.scheduler.unpark();
    }

    pub fn add_sub_task<T>(&self, task_id: TaskId, sub_task: T) -> Result<(), T>
    where
        T: Future<Output = SubTaskOutput> + Send + 'static,
    {
        self.0.scheduler.tasks.add_sub_task(task_id, sub_task)
    }

    pub fn downgrade(&self) -> HandleWeak {
        HandleWeak(self.0.downgrade())
    }

    pub async fn drain_sub_tasks(&self, task_id: TaskId) -> SubTaskOutput {
        let sub_tasks_fut = self.0.scheduler.tasks.drain_sub_tasks(task_id);
        sub_tasks_fut.await
    }
}

impl PartialEq for Handle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use std::time::Duration;

    #[test]
    fn block_on_task_join_handle() {
        use futures::channel::oneshot;
        use std::sync::mpsc;

        let (join_sender, join_receiver) = mpsc::channel();
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();

        std::thread::spawn(move || {
            let handle =
                Scheduler::init("block_on_task_join_handle".into(), Duration::from_millis(2));
            let join_handle = handle.spawn(async {
                timer::delay_for(Duration::from_millis(5)).await;
                42
            });

            let _ = join_sender.send(join_handle);
            let _ = handle.0.scheduler.block_on_priv(shutdown_receiver);
        });

        let task_join_handle = join_receiver.recv().unwrap();
        let res = Scheduler::block_on(task_join_handle).unwrap();

        let _ = shutdown_sender.send(());

        assert_eq!(res, 42);
    }

    #[test]
    fn block_on_timer() {
        let res = Scheduler::block_on(async {
            timer::delay_for(Duration::from_millis(5)).await;
            42
        });

        assert_eq!(res, 42);
    }

    #[test]
    fn enter_non_static() {
        let handle = Scheduler::start("enter_non_static", Duration::from_millis(2));

        let mut flag = false;
        handle.enter(|| flag = true);
        assert!(flag);
    }
}
