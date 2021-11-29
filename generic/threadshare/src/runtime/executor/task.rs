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

use futures::future::BoxFuture;
use futures::prelude::*;

use gst::{gst_log, gst_trace, gst_warning};

use pin_project_lite::pin_project;

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

use crate::runtime::RUNTIME_CAT;

thread_local! {
    static CURRENT_TASK_ID: Cell<Option<TaskId>> = Cell::new(None);
}

#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
pub struct TaskId(pub(super) u64);

impl TaskId {
    const LAST: TaskId = TaskId(u64::MAX);

    fn next(task_id: Self) -> Self {
        TaskId(task_id.0.wrapping_add(1))
    }

    pub(super) fn current() -> Option<TaskId> {
        CURRENT_TASK_ID.try_with(Cell::get).ok().flatten()
    }
}

pub type SubTaskOutput = Result<(), gst::FlowError>;

pin_project! {
    pub(super) struct TaskFuture<F: Future> {
        id: TaskId,
        #[pin]
        future: F,
    }

}

impl<F: Future> TaskFuture<F> {
    pub fn id(&self) -> TaskId {
        self.id
    }
}

impl<F: Future> Future for TaskFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        struct TaskIdGuard {
            prev_task_id: Option<TaskId>,
        }

        impl Drop for TaskIdGuard {
            fn drop(&mut self) {
                let _ = CURRENT_TASK_ID.try_with(|cur| cur.replace(self.prev_task_id.take()));
            }
        }

        let task_id = self.id;
        let project = self.project();

        let _guard = TaskIdGuard {
            prev_task_id: CURRENT_TASK_ID.with(|cur| cur.replace(Some(task_id))),
        };

        project.future.poll(cx)
    }
}

struct Task {
    id: TaskId,
    sub_tasks: VecDeque<BoxFuture<'static, SubTaskOutput>>,
}

impl Task {
    fn new(id: TaskId) -> Self {
        Task {
            id,
            sub_tasks: VecDeque::new(),
        }
    }

    fn add_sub_task<T>(&mut self, sub_task: T)
    where
        T: Future<Output = SubTaskOutput> + Send + 'static,
    {
        self.sub_tasks.push_back(sub_task.boxed());
    }

    fn drain_sub_tasks(&mut self) -> VecDeque<BoxFuture<'static, SubTaskOutput>> {
        std::mem::take(&mut self.sub_tasks)
    }
}

impl fmt::Debug for Task {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Task")
            .field("id", &self.id)
            .field("sub_tasks len", &self.sub_tasks.len())
            .finish()
    }
}

#[derive(Debug)]
pub(super) struct TaskQueue {
    last_task_id: TaskId,
    tasks: HashMap<TaskId, Task>,
    context_name: Arc<str>,
}

impl TaskQueue {
    pub fn new(context_name: Arc<str>) -> Self {
        TaskQueue {
            last_task_id: TaskId::LAST,
            tasks: HashMap::default(),
            context_name,
        }
    }

    pub fn add<F: Future>(&mut self, future: F) -> TaskFuture<F> {
        self.last_task_id = TaskId::next(self.last_task_id);
        self.tasks
            .insert(self.last_task_id, Task::new(self.last_task_id));

        TaskFuture {
            id: self.last_task_id,
            future,
        }
    }

    pub fn remove(&mut self, task_id: TaskId) {
        if let Some(task) = self.tasks.remove(&task_id) {
            if !task.sub_tasks.is_empty() {
                gst_warning!(
                    RUNTIME_CAT,
                    "Task {:?} on context {} has {} pending sub tasks",
                    task_id,
                    self.context_name,
                    task.sub_tasks.len(),
                );
            }
        }
    }

    pub fn has_sub_tasks(&self, task_id: TaskId) -> bool {
        self.tasks
            .get(&task_id)
            .map(|t| !t.sub_tasks.is_empty())
            .unwrap_or(false)
    }

    pub fn add_sub_task<T>(&mut self, task_id: TaskId, sub_task: T) -> Result<(), T>
    where
        T: Future<Output = SubTaskOutput> + Send + 'static,
    {
        match self.tasks.get_mut(&task_id) {
            Some(task) => {
                gst_trace!(
                    RUNTIME_CAT,
                    "Adding subtask to {:?} on context {}",
                    task_id,
                    self.context_name
                );
                task.add_sub_task(sub_task);
                Ok(())
            }
            None => {
                gst_trace!(RUNTIME_CAT, "Task was removed in the meantime");
                Err(sub_task)
            }
        }
    }

    pub fn drain_sub_tasks(
        &mut self,
        task_id: TaskId,
    ) -> impl Future<Output = SubTaskOutput> + Send + 'static {
        let sub_tasks = self
            .tasks
            .get_mut(&task_id)
            .map(|task| (task.drain_sub_tasks(), Arc::clone(&self.context_name)));

        async move {
            if let Some((mut sub_tasks, context_name)) = sub_tasks {
                if !sub_tasks.is_empty() {
                    gst_log!(
                        RUNTIME_CAT,
                        "Scheduling draining {} sub tasks from {:?} on '{}'",
                        sub_tasks.len(),
                        task_id,
                        &context_name,
                    );

                    for sub_task in sub_tasks.drain(..) {
                        sub_task.await?;
                    }
                }
            }

            Ok(())
        }
    }
}
