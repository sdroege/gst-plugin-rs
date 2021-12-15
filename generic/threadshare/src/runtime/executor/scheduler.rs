// Copyright (C) 2018-2020 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2019-2021 François Laignel <fengalin@free.fr>
//
// Take a look at the license at the top of the repository in the LICENSE file.

use concurrent_queue::ConcurrentQueue;

use futures::channel::oneshot;
use futures::pin_mut;

use gio::glib::clone::Downgrade;
use gst::{gst_debug, gst_error, gst_trace, gst_warning};

use std::cell::RefCell;
use std::future::Future;
use std::panic;
use std::sync::mpsc as sync_mpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::task::Poll;
use std::thread;
use std::time::{Duration, Instant};

use waker_fn::waker_fn;

use super::task::{SubTaskOutput, TaskId, TaskQueue};
use super::{CallOnDrop, JoinHandle, Reactor, Source};
use crate::runtime::RUNTIME_CAT;

thread_local! {
    static CURRENT_SCHEDULER: RefCell<Option<HandleWeak>> = RefCell::new(None);
}

#[derive(Debug)]
struct CleanUpOps(Arc<Source>);

#[derive(Debug)]
pub(super) struct Scheduler {
    context_name: Arc<str>,
    max_throttling: Duration,
    tasks: TaskQueue,
    cleanup_ops: ConcurrentQueue<CleanUpOps>,
    must_awake: Mutex<bool>,
    must_awake_cvar: Condvar,
}

impl Scheduler {
    pub const DUMMY_NAME: &'static str = "DUMMY";

    pub fn start(context_name: &str, max_throttling: Duration) -> Handle {
        // Name the thread so that it appears in panic messages.
        let thread = thread::Builder::new().name(context_name.to_string());

        let (handle_sender, handle_receiver) = sync_mpsc::channel();
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let context_name = Arc::from(context_name);
        let thread_ctx_name = Arc::clone(&context_name);
        let join = thread
            .spawn(move || {
                gst_debug!(
                    RUNTIME_CAT,
                    "Started Scheduler thread for Context {}",
                    thread_ctx_name
                );

                let handle = Scheduler::init(Arc::clone(&thread_ctx_name), max_throttling);
                let this = Arc::clone(&handle.0.scheduler);
                handle_sender.send(handle.clone()).unwrap();

                match this.block_on_priv(shutdown_receiver) {
                    Ok(_) => {
                        gst_debug!(
                            RUNTIME_CAT,
                            "Scheduler thread shut down for Context {}",
                            thread_ctx_name
                        );
                    }
                    Err(e) => {
                        gst_error!(
                            RUNTIME_CAT,
                            "Scheduler thread shut down due to an error within Context {}",
                            thread_ctx_name
                        );

                        // We are shutting down on our own initiative
                        if let Ok(mut shutdown) = handle.0.shutdown.lock() {
                            shutdown.clear();
                        }

                        panic::resume_unwind(e);
                    }
                }
            })
            .expect("Failed to spawn Scheduler thread");

        let handle = handle_receiver.recv().expect("Context thread init failed");
        handle.set_shutdown(shutdown_sender, join);

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
                cleanup_ops: ConcurrentQueue::bounded(1000),
                must_awake: Mutex::new(false),
                must_awake_cvar: Condvar::new(),
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

        let (task_id, task) = this.tasks.add(async move {
            let res = future.await;

            let task_id = TaskId::current().unwrap();
            while handle.has_sub_tasks(task_id) {
                if handle.drain_sub_tasks(task_id).await.is_err() {
                    break;
                }
            }

            res
        });

        gst_trace!(RUNTIME_CAT, "Blocking on current thread with {:?}", task_id);

        let _guard = CallOnDrop::new(|| {
            gst_trace!(
                RUNTIME_CAT,
                "Blocking on current thread with {:?} done",
                task_id,
            );
        });

        match this.block_on_priv(task) {
            Ok(res) => res,
            Err(e) => {
                gst_error!(
                    RUNTIME_CAT,
                    "Panic blocking on Context {}",
                    &Scheduler::DUMMY_NAME
                );

                panic::resume_unwind(e);
            }
        }
    }

    fn block_on_priv<F>(&self, future: F) -> std::thread::Result<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let waker = waker_fn(|| ());
        let cx = &mut std::task::Context::from_waker(&waker);
        pin_mut!(future);

        let _guard = CallOnDrop::new(|| Scheduler::close(Arc::clone(&self.context_name)));

        let mut last;
        loop {
            last = Instant::now();

            if let Poll::Ready(t) = future.as_mut().poll(cx) {
                break Ok(t);
            }

            Reactor::with_mut(|reactor| {
                while let Ok(op) = self.cleanup_ops.pop() {
                    let _ = reactor.remove_io(&op.0);
                }

                reactor.react().ok()
            });

            loop {
                match self.tasks.pop_runnable() {
                    Err(_) => break,
                    Ok(runnable) => {
                        panic::catch_unwind(|| runnable.run()).map_err(|err| {
                            gst_error!(
                                RUNTIME_CAT,
                                "A task has panicked within Context {}",
                                self.context_name
                            );

                            err
                        })?;
                    }
                }
            }

            let mut must_awake = self.must_awake.lock().unwrap();
            let mut must_awake = loop {
                if let Some(wait_duration) = self.max_throttling.checked_sub(last.elapsed()) {
                    let result = self
                        .must_awake_cvar
                        .wait_timeout(must_awake, wait_duration)
                        .unwrap();

                    must_awake = result.0;
                    if *must_awake {
                        break must_awake;
                    }
                } else {
                    break must_awake;
                }
            };

            *must_awake = false;
        }
    }

    fn wake_up(&self) {
        let mut must_awake = self.must_awake.lock().unwrap();
        *must_awake = true;
        self.must_awake_cvar.notify_one();
    }

    fn close(context_name: Arc<str>) {
        gst_trace!(
            RUNTIME_CAT,
            "Closing Scheduler for Context {}",
            context_name,
        );

        Reactor::close();

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

impl Drop for Scheduler {
    fn drop(&mut self) {
        gst_debug!(
            RUNTIME_CAT,
            "Terminated: Scheduler for Context {}",
            self.context_name
        );
    }
}

#[derive(Debug)]
struct SchedulerShutdown {
    scheduler: Arc<Scheduler>,
    sender: Option<oneshot::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl SchedulerShutdown {
    fn new(scheduler: Arc<Scheduler>) -> Self {
        SchedulerShutdown {
            scheduler,
            sender: None,
            join: None,
        }
    }

    fn clear(&mut self) {
        self.sender = None;
        self.join = None;
    }
}

impl Drop for SchedulerShutdown {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            gst_debug!(
                RUNTIME_CAT,
                "Shutting down Scheduler thread for Context {}",
                self.scheduler.context_name
            );
            drop(sender);

            // Don't block shutting down itself
            if !self.scheduler.is_current() {
                if let Some(join_handler) = self.join.take() {
                    gst_trace!(
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

#[derive(Debug)]
struct HandleInner {
    scheduler: Arc<Scheduler>,
    shutdown: Mutex<SchedulerShutdown>,
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
        Handle(Arc::new(HandleInner {
            shutdown: Mutex::new(SchedulerShutdown::new(Arc::clone(&scheduler))),
            scheduler,
        }))
    }

    fn set_shutdown(&self, sender: oneshot::Sender<()>, join: thread::JoinHandle<()>) {
        let mut shutdown = self.0.shutdown.lock().unwrap();
        shutdown.sender = Some(sender);
        shutdown.join = Some(join);
    }

    pub fn context_name(&self) -> &str {
        &self.0.scheduler.context_name
    }

    pub fn max_throttling(&self) -> Duration {
        self.0.scheduler.max_throttling
    }

    /// Executes the provided function relatively to this [`Scheduler`]'s [`Reactor`].
    ///
    /// Usefull to initialze i/o sources and timers from outside
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
        self.0.scheduler.wake_up();
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

    pub fn spawn_and_awake<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (task_id, task) = self.0.scheduler.tasks.add(future);
        self.0.scheduler.wake_up();
        JoinHandle::new(task_id, task, self)
    }

    pub fn remove_soure(&self, source: Arc<Source>) {
        if self
            .0
            .scheduler
            .cleanup_ops
            .push(CleanUpOps(source))
            .is_err()
        {
            gst_warning!(RUNTIME_CAT, "scheduler: cleanup_ops is full");
        }
    }

    pub fn has_sub_tasks(&self, task_id: TaskId) -> bool {
        self.0.scheduler.tasks.has_sub_tasks(task_id)
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
    use super::super::Timer;
    use super::*;

    #[test]
    fn block_on_task_join_handle() {
        use std::sync::mpsc;

        let (join_sender, join_receiver) = mpsc::channel();
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();

        std::thread::spawn(move || {
            let handle =
                Scheduler::init("block_on_task_join_handle".into(), Duration::from_millis(2));
            let join_handle = handle.spawn(async {
                Timer::after(Duration::from_millis(5)).await;
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
            Timer::after(Duration::from_millis(5)).await;
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
