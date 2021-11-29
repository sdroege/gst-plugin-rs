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

use futures::channel::oneshot;

use gio::glib::clone::Downgrade;
use gst::{gst_debug, gst_trace};

use std::cell::RefCell;
use std::future::Future;
use std::sync::mpsc as sync_mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

use super::task::{SubTaskOutput, TaskFuture, TaskId, TaskQueue};
use super::{CallOnDrop, JoinHandle};
use crate::runtime::RUNTIME_CAT;

thread_local! {
    static CURRENT_SCHEDULER: RefCell<Option<Weak<Scheduler>>> = RefCell::new(None);
}

#[derive(Debug)]
pub(super) struct Scheduler {
    context_name: Arc<str>,
    max_throttling: Duration,
    task_queue: Mutex<TaskQueue>,
    rt_handle: Mutex<tokio::runtime::Handle>,
    shutdown: Mutex<Option<SchedulerShutdown>>,
}

impl Scheduler {
    pub const DUMMY_NAME: &'static str = "DUMMY";

    pub fn start(context_name: &str, max_throttling: Duration) -> Handle {
        let context_name = Arc::from(context_name);

        let (handle_sender, handle_receiver) = sync_mpsc::channel();
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let thread_ctx_name = Arc::clone(&context_name);
        let join = thread::spawn(move || {
            gst_debug!(
                RUNTIME_CAT,
                "Started Scheduler thread for Context '{}'",
                thread_ctx_name
            );

            let (mut rt, handle) = Scheduler::init(thread_ctx_name, max_throttling);
            handle_sender.send(handle.clone()).unwrap();

            let _ = rt.block_on(shutdown_receiver);
        });

        let handle = handle_receiver.recv().expect("Context thread init failed");
        *handle.0.shutdown.lock().unwrap() = Some(SchedulerShutdown {
            context_name,
            sender: Some(shutdown_sender),
            join: Some(join),
        });

        handle
    }

    fn init(context_name: Arc<str>, max_throttling: Duration) -> (tokio::runtime::Runtime, Handle) {
        let runtime = tokio::runtime::Builder::new()
            .basic_scheduler()
            .enable_all()
            .max_throttling(max_throttling)
            .build()
            .expect("Couldn't build the runtime");

        let scheduler = Arc::new(Scheduler {
            context_name: context_name.clone(),
            max_throttling,
            task_queue: Mutex::new(TaskQueue::new(context_name)),
            rt_handle: Mutex::new(runtime.handle().clone()),
            shutdown: Mutex::new(None),
        });

        CURRENT_SCHEDULER.with(|cur_scheduler| {
            *cur_scheduler.borrow_mut() = Some(scheduler.downgrade());
        });

        (runtime, scheduler.into())
    }

    pub fn block_on<F: Future>(future: F) -> <F as Future>::Output {
        assert!(
            !Scheduler::is_scheduler_thread(),
            "Attempt at blocking on from an existing Scheduler thread."
        );
        let (mut rt, handle) = Scheduler::init(Scheduler::DUMMY_NAME.into(), Duration::ZERO);

        let handle_clone = handle.clone();
        let task = handle.0.task_queue.lock().unwrap().add(async move {
            let res = future.await;

            let task_id = TaskId::current().unwrap();
            while handle_clone.has_sub_tasks(task_id) {
                if handle_clone.drain_sub_tasks(task_id).await.is_err() {
                    break;
                }
            }

            res
        });

        let task_id = task.id();
        gst_trace!(RUNTIME_CAT, "Blocking on current thread with {:?}", task_id,);

        let _guard = CallOnDrop::new(|| {
            gst_trace!(
                RUNTIME_CAT,
                "Blocking on current thread with {:?} done",
                task_id,
            );

            handle.remove_task(task_id);
        });

        rt.block_on(task)
    }

    pub(super) fn is_scheduler_thread() -> bool {
        CURRENT_SCHEDULER.with(|cur_scheduler| cur_scheduler.borrow().is_some())
    }

    pub(super) fn current() -> Option<Handle> {
        CURRENT_SCHEDULER.with(|cur_scheduler| {
            cur_scheduler
                .borrow()
                .as_ref()
                .and_then(Weak::upgrade)
                .map(Handle::from)
        })
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        // No more strong handlers point to this
        // Scheduler, so remove its thread local key.
        let _ = CURRENT_SCHEDULER.try_with(|cur_scheduler| {
            *cur_scheduler.borrow_mut() = None;
        });

        gst_debug!(
            RUNTIME_CAT,
            "Terminated: Scheduler for Context '{}'",
            self.context_name
        );
    }
}

#[derive(Debug)]
pub(super) struct SchedulerShutdown {
    context_name: Arc<str>,
    sender: Option<oneshot::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for SchedulerShutdown {
    fn drop(&mut self) {
        gst_debug!(
            RUNTIME_CAT,
            "Shutting down Scheduler thread for Context '{}'",
            self.context_name
        );
        self.sender.take().unwrap();

        gst_trace!(
            RUNTIME_CAT,
            "Waiting for Scheduler to shutdown for Context '{}'",
            self.context_name
        );
        let _ = self.join.take().unwrap().join();
    }
}

#[derive(Clone, Debug)]
pub(super) struct HandleWeak(Weak<Scheduler>);

impl HandleWeak {
    pub(super) fn upgrade(&self) -> Option<Handle> {
        self.0.upgrade().map(Handle)
    }
}

#[derive(Clone, Debug)]
pub(super) struct Handle(Arc<Scheduler>);

impl Handle {
    pub fn context_name(&self) -> &str {
        &self.0.context_name
    }

    pub fn max_throttling(&self) -> Duration {
        self.0.max_throttling
    }

    pub fn enter<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.0.rt_handle.lock().unwrap().enter(f)
    }

    pub fn add_task<F: Future>(&self, future: F) -> TaskFuture<F> {
        let task = self.0.task_queue.lock().unwrap().add(future);
        task
    }

    pub fn remove_task(&self, task_id: TaskId) {
        self.0.task_queue.lock().unwrap().remove(task_id);
    }

    pub fn spawn<F>(&self, future: F, must_awake: bool) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let task = self.add_task(future);
        let task_id = task.id();
        let (sender, receiver) = oneshot::channel();

        gst_trace!(
            RUNTIME_CAT,
            "Spawning new task_id {:?} on context {}",
            task.id(),
            self.0.context_name
        );

        let this = self.clone();
        let spawn_fut = async move {
            gst_trace!(
                RUNTIME_CAT,
                "Running task_id {:?} on context {}",
                task_id,
                this.context_name()
            );

            let _guard = CallOnDrop::new(|| {
                gst_trace!(
                    RUNTIME_CAT,
                    "Task {:?} on context {} done",
                    task_id,
                    this.context_name()
                );

                this.0.task_queue.lock().unwrap().remove(task_id);
            });

            let _ = sender.send(task.await);
        };

        if must_awake {
            let _ = self.0.rt_handle.lock().unwrap().awake_and_spawn(spawn_fut);
        } else {
            let _ = self.0.rt_handle.lock().unwrap().spawn(spawn_fut);
        }

        JoinHandle::new(receiver, self, task_id)
    }

    pub fn has_sub_tasks(&self, task_id: TaskId) -> bool {
        let ret = self.0.task_queue.lock().unwrap().has_sub_tasks(task_id);
        ret
    }

    pub fn add_sub_task<T>(&self, task_id: TaskId, sub_task: T) -> Result<(), T>
    where
        T: Future<Output = SubTaskOutput> + Send + 'static,
    {
        let res = self
            .0
            .task_queue
            .lock()
            .unwrap()
            .add_sub_task(task_id, sub_task);
        res
    }

    pub fn downgrade(&self) -> HandleWeak {
        HandleWeak(self.0.downgrade())
    }

    pub async fn drain_sub_tasks(&self, task_id: TaskId) -> SubTaskOutput {
        let sub_tasks_fut = self.0.task_queue.lock().unwrap().drain_sub_tasks(task_id);
        sub_tasks_fut.await
    }
}

impl From<Arc<Scheduler>> for Handle {
    fn from(arc: Arc<Scheduler>) -> Self {
        Handle(arc)
    }
}

impl PartialEq for Handle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
