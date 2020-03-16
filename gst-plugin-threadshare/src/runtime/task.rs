// Copyright (C) 2019 François Laignel <fengalin@free.fr>
// Copyright (C) 2020 Sebastian Dröge <sebastian@centricular.com>
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

//! An execution loop to run asynchronous processing.

use futures::future::{abortable, AbortHandle, Aborted};
use futures::prelude::*;

use gst::TaskState;
use gst::{gst_debug, gst_log, gst_trace, gst_warning};

use std::fmt;
use std::sync::{Arc, Mutex};

use super::executor::block_on;
use super::{Context, JoinHandle, RUNTIME_CAT};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskError {
    ActiveTask,
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TaskError::ActiveTask => write!(f, "The task is still active"),
        }
    }
}

impl std::error::Error for TaskError {}

#[derive(Debug)]
struct TaskInner {
    context: Option<Context>,
    state: TaskState,
    prepare_handle: Option<JoinHandle<Result<Result<(), gst::FlowError>, Aborted>>>,
    prepare_abort_handle: Option<AbortHandle>,
    abort_handle: Option<AbortHandle>,
    loop_handle: Option<JoinHandle<Result<(), Aborted>>>,
}

impl Default for TaskInner {
    fn default() -> Self {
        TaskInner {
            context: None,
            state: TaskState::Stopped,
            prepare_handle: None,
            prepare_abort_handle: None,
            abort_handle: None,
            loop_handle: None,
        }
    }
}

impl Drop for TaskInner {
    fn drop(&mut self) {
        // Check invariant which can't be held automatically in `Task`
        // because `drop` can't be `async`
        if self.state != TaskState::Stopped {
            panic!("Missing call to `Task::stop`");
        }
    }
}

/// A `Task` operating on a `threadshare` [`Context`].
///
/// [`Context`]: ../executor/struct.Context.html
#[derive(Debug)]
pub struct Task(Arc<Mutex<TaskInner>>);

impl Default for Task {
    fn default() -> Self {
        Task(Arc::new(Mutex::new(TaskInner::default())))
    }
}

impl Task {
    pub fn prepare_with_func<F, Fut>(
        &self,
        context: Context,
        prepare_func: F,
    ) -> Result<(), TaskError>
    where
        F: (FnOnce() -> Fut) + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        gst_debug!(RUNTIME_CAT, "Preparing task");

        let mut inner = self.0.lock().unwrap();
        if inner.state != TaskState::Stopped {
            return Err(TaskError::ActiveTask);
        }

        // Spawn prepare function in the background
        let task_weak = Arc::downgrade(&self.0);
        let (prepare_fut, prepare_abort_handle) = abortable(async move {
            gst_trace!(RUNTIME_CAT, "Calling task prepare function");

            prepare_func().await;

            gst_trace!(RUNTIME_CAT, "Task prepare function finished");

            while Context::current_has_sub_tasks() {
                Context::drain_sub_tasks().await?;
            }

            // Once the prepare function is finished we can forget the corresponding
            // handles so that unprepare and friends don't have to block on it anymore
            if let Some(task_inner) = task_weak.upgrade() {
                let mut inner = task_inner.lock().unwrap();
                inner.prepare_abort_handle = None;
                inner.prepare_handle = None;
            }

            gst_trace!(RUNTIME_CAT, "Task fully prepared");

            Ok(())
        });
        let prepare_handle = context.spawn(prepare_fut);
        inner.prepare_handle = Some(prepare_handle);
        inner.prepare_abort_handle = Some(prepare_abort_handle);

        inner.context = Some(context);

        gst_debug!(RUNTIME_CAT, "Task prepared");

        Ok(())
    }

    pub fn prepare(&self, context: Context) -> Result<(), TaskError> {
        gst_debug!(RUNTIME_CAT, "Preparing task");

        let mut inner = self.0.lock().unwrap();
        if inner.state != TaskState::Stopped {
            return Err(TaskError::ActiveTask);
        }

        inner.prepare_handle = None;
        inner.prepare_abort_handle = None;

        inner.context = Some(context);

        gst_debug!(RUNTIME_CAT, "Task prepared");

        Ok(())
    }

    pub fn unprepare(&self) -> Result<(), TaskError> {
        gst_debug!(RUNTIME_CAT, "Unpreparing task");

        let mut inner = self.0.lock().unwrap();
        if inner.state != TaskState::Stopped {
            return Err(TaskError::ActiveTask);
        }

        // Abort any pending preparation
        if let Some(abort_handle) = inner.prepare_abort_handle.take() {
            abort_handle.abort();
        }
        let prepare_handle = inner.prepare_handle.take();

        let context = inner.context.take().unwrap();
        drop(inner);

        if let Some(prepare_handle) = prepare_handle {
            if let Some((cur_context, cur_task_id)) = Context::current_task() {
                if prepare_handle.is_current() {
                    // This would deadlock!
                    gst_warning!(
                        RUNTIME_CAT,
                        "Trying to stop task {:?} from itself, not waiting",
                        prepare_handle
                    );
                } else if cur_context == context {
                    // This is ok: as we're on the same thread and the prepare function is aborted
                    // this means that it won't ever be called, and we're not inside it here
                    gst_debug!(
                        RUNTIME_CAT,
                        "Asynchronously waiting for task {:?} on the same context",
                        prepare_handle
                    );

                    let _ = Context::add_sub_task(async move {
                        let _ = prepare_handle.await;
                        Ok(())
                    });
                } else {
                    // This is suboptimal but we can't really do this asynchronously as otherwise
                    // it might be started again before it's actually stopped.
                    gst_warning!(
                        RUNTIME_CAT,
                        "Synchronously waiting for task {:?} on task {:?} on context {}",
                        prepare_handle,
                        cur_task_id,
                        cur_context.name()
                    );
                    let _ = block_on(prepare_handle);
                }
            } else {
                gst_debug!(
                    RUNTIME_CAT,
                    "Synchronously waiting for task {:?}",
                    prepare_handle
                );
                let _ = block_on(prepare_handle);
            }
        }

        gst_debug!(RUNTIME_CAT, "Task unprepared");

        Ok(())
    }

    pub fn state(&self) -> TaskState {
        self.0.lock().unwrap().state
    }

    pub fn context(&self) -> Option<Context> {
        self.0.lock().unwrap().context.as_ref().cloned()
    }

    /// `Starts` the `Task`.
    ///
    /// The `Task` will loop on the provided @func.
    /// The execution occurs on the `Task`'s context.
    pub fn start<F, Fut>(&self, mut func: F)
    where
        F: (FnMut() -> Fut) + Send + 'static,
        Fut: Future<Output = glib::Continue> + Send + 'static,
    {
        let inner_clone = Arc::clone(&self.0);
        let mut inner = self.0.lock().unwrap();
        match inner.state {
            TaskState::Started => {
                gst_log!(RUNTIME_CAT, "Task already Started");
                return;
            }
            TaskState::Paused | TaskState::Stopped => (),
            other => unreachable!("Unexpected Task state {:?}", other),
        }

        gst_debug!(RUNTIME_CAT, "Starting Task");

        let prepare_handle = inner.prepare_handle.take();

        // If the task was only cancelled and not actually stopped yet then
        // wait for that to happen as first thing in the new task.
        let loop_handle = inner.loop_handle.take();

        let (loop_fut, abort_handle) = abortable(async move {
            let task_id = Context::current_task().unwrap().1;

            if let Some(loop_handle) = loop_handle {
                gst_trace!(
                    RUNTIME_CAT,
                    "Waiting for previous loop to finish before starting"
                );
                let _ = loop_handle.await;
            }

            // First await on the prepare function, if any
            if let Some(prepare_handle) = prepare_handle {
                gst_trace!(RUNTIME_CAT, "Waiting for prepare before starting");
                let res = prepare_handle.await;
                if res.is_err() {
                    gst_warning!(RUNTIME_CAT, "Preparing failed");
                    return;
                }
            }

            gst_trace!(RUNTIME_CAT, "Starting task loop");

            // Then loop as long as we're actually running
            loop {
                match inner_clone.lock().unwrap().state {
                    TaskState::Started => (),
                    TaskState::Paused | TaskState::Stopped => {
                        gst_trace!(RUNTIME_CAT, "Stopping task loop");
                        break;
                    }
                    other => unreachable!("Unexpected Task state {:?}", other),
                }

                if func().await == glib::Continue(false) {
                    let mut inner = inner_clone.lock().unwrap();

                    // Make sure to only reset the state if this is still the correct task
                    // and no new task was started in the meantime
                    if inner.state == TaskState::Started
                        && inner
                            .loop_handle
                            .as_ref()
                            .map(|h| h.task_id() == task_id)
                            .unwrap_or(false)
                    {
                        gst_trace!(RUNTIME_CAT, "Pausing task loop");
                        inner.state = TaskState::Paused;
                    }

                    break;
                }
            }

            // Once the loop function is finished we can forget the corresponding
            // handles so that unprepare and friends don't have to block on it anymore
            {
                let mut inner = inner_clone.lock().unwrap();

                // Make sure to only reset the state if this is still the correct task
                // and no new task was started in the meantime
                if inner
                    .loop_handle
                    .as_ref()
                    .map(|h| h.task_id() == task_id)
                    .unwrap_or(false)
                {
                    inner.abort_handle = None;
                    inner.loop_handle = None;
                }
            }

            gst_trace!(RUNTIME_CAT, "Task loop finished");
        });

        let loop_handle = inner
            .context
            .as_ref()
            .expect("Context not set")
            .spawn(loop_fut);

        inner.abort_handle = Some(abort_handle);
        inner.loop_handle = Some(loop_handle);
        inner.state = TaskState::Started;

        gst_debug!(RUNTIME_CAT, "Task Started");
    }

    /// Cancels the `Task` so that it stops running as soon as possible.
    pub fn cancel(&self) {
        let mut inner = self.0.lock().unwrap();
        if inner.state != TaskState::Started {
            gst_log!(RUNTIME_CAT, "Task already paused or stopped");
            return;
        }

        gst_debug!(RUNTIME_CAT, "Cancelling Task");

        // Abort any still running loop function
        if let Some(abort_handle) = inner.abort_handle.take() {
            abort_handle.abort();
        }

        inner.state = TaskState::Paused;
    }

    /// Stops the `Started` `Task` and wait for it to finish.
    pub fn stop(&self) {
        let mut inner = self.0.lock().unwrap();
        if inner.state == TaskState::Stopped {
            gst_log!(RUNTIME_CAT, "Task already stopped");
            return;
        }

        gst_debug!(RUNTIME_CAT, "Stopping Task");

        inner.state = TaskState::Stopped;

        // Abort any still running loop function
        if let Some(abort_handle) = inner.abort_handle.take() {
            abort_handle.abort();
        }

        // And now wait for it to actually stop
        let loop_handle = inner.loop_handle.take();
        let context = inner.context.as_ref().unwrap().clone();
        drop(inner);

        if let Some(loop_handle) = loop_handle {
            if let Some((cur_context, cur_task_id)) = Context::current_task() {
                if loop_handle.is_current() {
                    // This would deadlock!
                    gst_warning!(
                        RUNTIME_CAT,
                        "Trying to stop task {:?} from itself, not waiting",
                        loop_handle
                    );
                } else if cur_context == context {
                    // This is ok: as we're on the same thread and the loop function is aborted
                    // this means that it won't ever be called, and we're not inside it here
                    gst_debug!(
                        RUNTIME_CAT,
                        "Asynchronously waiting for task {:?} on the same context",
                        loop_handle
                    );

                    let _ = Context::add_sub_task(async move {
                        let _ = loop_handle.await;
                        Ok(())
                    });
                } else {
                    // This is suboptimal but we can't really do this asynchronously as otherwise
                    // it might be started again before it's actually stopped.
                    gst_warning!(
                        RUNTIME_CAT,
                        "Synchronously waiting for task {:?} on task {:?} on context {}",
                        loop_handle,
                        cur_task_id,
                        cur_context.name()
                    );
                    let _ = block_on(loop_handle);
                }
            } else {
                gst_debug!(
                    RUNTIME_CAT,
                    "Synchronously waiting for task {:?}",
                    loop_handle
                );
                let _ = block_on(loop_handle);
            }
        }

        gst_debug!(RUNTIME_CAT, "Task stopped");
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::{mpsc, oneshot};
    use futures::lock::Mutex;

    use std::sync::Arc;

    use crate::runtime::Context;

    use super::*;

    #[tokio::test]
    async fn task() {
        gst::init().unwrap();

        let context = Context::acquire("task", 2).unwrap();

        let task = Task::default();
        task.prepare(context).unwrap();

        let (mut sender, receiver) = mpsc::channel(0);
        let receiver = Arc::new(Mutex::new(receiver));

        gst_debug!(RUNTIME_CAT, "task test: starting");
        task.start(move || {
            let receiver = Arc::clone(&receiver);
            async move {
                gst_debug!(RUNTIME_CAT, "task test: awaiting receiver");
                match receiver.lock().await.next().await {
                    Some(_) => {
                        gst_debug!(RUNTIME_CAT, "task test: item received");
                        glib::Continue(true)
                    }
                    None => {
                        gst_debug!(RUNTIME_CAT, "task test: channel complete");
                        glib::Continue(false)
                    }
                }
            }
        });

        gst_debug!(RUNTIME_CAT, "task test: sending item");
        sender.send(()).await.unwrap();
        gst_debug!(RUNTIME_CAT, "task test: item sent");

        gst_debug!(RUNTIME_CAT, "task test: dropping sender");
        drop(sender);

        gst_debug!(RUNTIME_CAT, "task test: stopping");
        task.stop();
        gst_debug!(RUNTIME_CAT, "task test: stopped");

        task.unprepare().unwrap();
        gst_debug!(RUNTIME_CAT, "task test: unprepared");
    }

    #[tokio::test]
    async fn task_with_prepare_func() {
        gst::init().unwrap();

        let context = Context::acquire("task_with_prepare_func", 2).unwrap();

        let task = Task::default();

        let (prepare_sender, prepare_receiver) = oneshot::channel();
        task.prepare_with_func(context, move || async move {
            prepare_sender.send(()).unwrap();
        })
        .unwrap();

        let (mut sender, receiver) = mpsc::channel(0);
        let receiver = Arc::new(Mutex::new(receiver));

        let mut prepare_receiver = Some(prepare_receiver);

        gst_debug!(RUNTIME_CAT, "task test: starting");
        task.start(move || {
            if let Some(mut prepare_receiver) = prepare_receiver.take() {
                assert_eq!(prepare_receiver.try_recv().unwrap(), Some(()));
            }

            let receiver = Arc::clone(&receiver);
            async move {
                gst_debug!(RUNTIME_CAT, "task test: awaiting receiver");
                match receiver.lock().await.next().await {
                    Some(_) => {
                        gst_debug!(RUNTIME_CAT, "task test: item received");
                        glib::Continue(true)
                    }
                    None => {
                        gst_debug!(RUNTIME_CAT, "task test: channel complete");
                        glib::Continue(false)
                    }
                }
            }
        });

        gst_debug!(RUNTIME_CAT, "task test: sending item");
        sender.send(()).await.unwrap();
        gst_debug!(RUNTIME_CAT, "task test: item sent");

        gst_debug!(RUNTIME_CAT, "task test: dropping sender");
        drop(sender);

        gst_debug!(RUNTIME_CAT, "task test: stopping");
        task.stop();
        gst_debug!(RUNTIME_CAT, "task test: stopped");

        task.unprepare().unwrap();
        gst_debug!(RUNTIME_CAT, "task test: unprepared");
    }
}
