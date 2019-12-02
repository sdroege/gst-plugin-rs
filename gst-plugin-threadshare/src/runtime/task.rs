// Copyright (C) 2019 François Laignel <fengalin@free.fr>
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

//! An execution loop to run asynchronous processing on a [`Context`].
//!
//! [`Context`]: ../executor/struct.Context.html

use futures::channel::oneshot;
use futures::future::{self, BoxFuture};
use futures::lock::Mutex;
use futures::prelude::*;

use gst::TaskState;
use gst::{gst_debug, gst_log, gst_trace, gst_warning};

use std::fmt;
use std::sync::Arc;

use super::future::{abortable_waitable, AbortWaitHandle};
use super::{Context, RUNTIME_CAT};

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
    loop_end_sender: Option<oneshot::Sender<()>>,
    loop_handle: Option<AbortWaitHandle>,
}

impl Default for TaskInner {
    fn default() -> Self {
        TaskInner {
            context: None,
            state: TaskState::Stopped,
            loop_end_sender: None,
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
/// [`Context`]: struct.Context.html
#[derive(Debug)]
pub struct Task(Arc<Mutex<TaskInner>>);

impl Default for Task {
    fn default() -> Self {
        Task(Arc::new(Mutex::new(TaskInner::default())))
    }
}

impl Task {
    pub async fn prepare(&self, context: Context) -> Result<(), TaskError> {
        let mut inner = self.0.lock().await;
        if inner.state != TaskState::Stopped {
            return Err(TaskError::ActiveTask);
        }

        inner.context = Some(context);
        Ok(())
    }

    pub async fn unprepare(&self) -> Result<(), TaskError> {
        let mut inner = self.0.lock().await;
        if inner.state != TaskState::Stopped {
            return Err(TaskError::ActiveTask);
        }

        inner.context = None;
        Ok(())
    }

    pub async fn state(&self) -> TaskState {
        self.0.lock().await.state
    }

    /// `Starts` the `Task`.
    ///
    /// The `Task` will loop on the provided @func.
    /// The execution occurs on the `Task`'s context.
    pub async fn start<F, Fut>(&self, mut func: F)
    where
        F: (FnMut() -> Fut) + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let inner_clone = Arc::clone(&self.0);
        let mut inner = self.0.lock().await;
        match inner.state {
            TaskState::Started => {
                gst_log!(RUNTIME_CAT, "Task already Started");
                return;
            }
            TaskState::Paused | TaskState::Stopped => (),
            other => unreachable!("Unexpected Task state {:?}", other),
        }

        gst_debug!(RUNTIME_CAT, "Starting Task");

        let (loop_fut, loop_handle) = abortable_waitable(async move {
            loop {
                func().await;

                let mut inner = inner_clone.lock().await;
                match inner.state {
                    TaskState::Started => (),
                    TaskState::Paused | TaskState::Stopped => {
                        inner.loop_handle = None;
                        inner.loop_end_sender.take();

                        break;
                    }
                    other => unreachable!("Unexpected Task state {:?}", other),
                }
            }
        });

        inner
            .context
            .as_ref()
            .expect("Context not set")
            .spawn(loop_fut.map(drop));

        inner.loop_handle = Some(loop_handle);
        inner.state = TaskState::Started;

        gst_debug!(RUNTIME_CAT, "Task Started");
    }

    /// Pauses the `Started` `Task`.
    pub async fn pause(&self) -> BoxFuture<'static, ()> {
        let mut inner = self.0.lock().await;
        match inner.state {
            TaskState::Started => {
                gst_log!(RUNTIME_CAT, "Pausing Task");

                inner.state = TaskState::Paused;

                let (sender, receiver) = oneshot::channel();
                inner.loop_end_sender = Some(sender);

                async move {
                    let _ = receiver.await;
                    gst_log!(RUNTIME_CAT, "Task Paused");
                }
                .boxed()
            }
            TaskState::Paused => {
                gst_trace!(RUNTIME_CAT, "Task already Paused");

                future::ready(()).boxed()
            }
            other => {
                gst_warning!(RUNTIME_CAT, "Attempting to pause Task in state {:?}", other,);

                future::ready(()).boxed()
            }
        }
    }

    pub async fn stop(&self) {
        let mut inner = self.0.lock().await;
        if inner.state == TaskState::Stopped {
            gst_log!(RUNTIME_CAT, "Task already stopped");
            return;
        }

        gst_debug!(RUNTIME_CAT, "Stopping Task");

        if let Some(loop_handle) = inner.loop_handle.take() {
            let _ = loop_handle.abort_and_wait().await;
        }

        inner.state = TaskState::Stopped;

        gst_debug!(RUNTIME_CAT, "Task Stopped");
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::mpsc;
    use futures::lock::Mutex;

    use std::sync::Arc;

    use crate::runtime::Context;

    use super::*;

    #[tokio::test]
    async fn task() {
        gst::init().unwrap();

        let context = Context::acquire("task", 2).unwrap();

        let task = Task::default();
        task.prepare(context).await.unwrap();

        let (mut sender, receiver) = mpsc::channel(0);
        let receiver = Arc::new(Mutex::new(receiver));

        gst_debug!(RUNTIME_CAT, "task test: starting");
        task.start(move || {
            let receiver = Arc::clone(&receiver);
            async move {
                gst_debug!(RUNTIME_CAT, "task test: awaiting receiver");
                match receiver.lock().await.next().await {
                    Some(_) => gst_debug!(RUNTIME_CAT, "task test: item received"),
                    None => gst_debug!(RUNTIME_CAT, "task test: channel complete"),
                }
            }
        })
        .await;

        gst_debug!(RUNTIME_CAT, "task test: sending item");
        sender.send(()).await.unwrap();
        gst_debug!(RUNTIME_CAT, "task test: item sent");

        gst_debug!(RUNTIME_CAT, "task test: pausing");
        let pause_completion = task.pause().await;

        gst_debug!(RUNTIME_CAT, "task test: dropping sender");
        drop(sender);

        gst_debug!(RUNTIME_CAT, "task test: awaiting pause completion");
        pause_completion.await;

        gst_debug!(RUNTIME_CAT, "task test: stopping");
        task.stop().await;
        gst_debug!(RUNTIME_CAT, "task test: stopped");
    }
}
