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
use futures::prelude::*;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use super::context::Context;
use super::TaskId;
use super::{Handle, HandleWeak, Scheduler};

#[derive(Debug)]
pub struct JoinError(TaskId);

impl fmt::Display for JoinError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "{:?} was Canceled", self.0)
    }
}

impl std::error::Error for JoinError {}

pub struct JoinHandle<T> {
    receiver: oneshot::Receiver<T>,
    handle: HandleWeak,
    task_id: TaskId,
}

unsafe impl<T: Send> Send for JoinHandle<T> {}
unsafe impl<T: Send> Sync for JoinHandle<T> {}

impl<T> JoinHandle<T> {
    pub(super) fn new(receiver: oneshot::Receiver<T>, handle: &Handle, task_id: TaskId) -> Self {
        JoinHandle {
            receiver,
            handle: handle.downgrade(),
            task_id,
        }
    }

    pub fn is_current(&self) -> bool {
        if let Some((cur_scheduler, task_id)) = Scheduler::current().zip(TaskId::current()) {
            self.handle.upgrade().map_or(false, |self_scheduler| {
                self_scheduler == cur_scheduler && task_id == self.task_id
            })
        } else {
            false
        }
    }

    pub fn context(&self) -> Option<Context> {
        self.handle.upgrade().map(Context::from)
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if self.as_ref().is_current() {
            panic!("Trying to join task {:?} from itself", self.as_ref());
        }

        self.as_mut()
            .receiver
            .poll_unpin(cx)
            .map_err(|_| JoinError(self.task_id))
    }
}

impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let context_name = self
            .handle
            .upgrade()
            .map(|handle| handle.context_name().to_owned());

        fmt.debug_struct("JoinHandle")
            .field("context", &context_name)
            .field("task_id", &self.task_id)
            .finish()
    }
}
