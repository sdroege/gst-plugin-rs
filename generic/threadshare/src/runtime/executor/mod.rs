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

//! The `Executor` for the `threadshare` GStreamer plugins framework.
//!
//! The [`threadshare`]'s `Executor` consists in a set of [`Context`]s. Each [`Context`] is
//! identified by a `name` and runs a loop in a dedicated `thread`. Users can use the [`Context`]
//! to spawn `Future`s. `Future`s are asynchronous processings which allow waiting for resources
//! in a non-blocking way. Examples of non-blocking operations are:
//!
//! * Waiting for an incoming packet on a Socket.
//! * Waiting for an asynchronous `Mutex` `lock` to succeed.
//! * Waiting for a time related `Future`.
//!
//! `Element` implementations should use [`PadSrc`] & [`PadSink`] which provides high-level features.
//!
//! [`threadshare`]: ../../index.html
//! [`PadSrc`]: ../pad/struct.PadSrc.html
//! [`PadSink`]: ../pad/struct.PadSink.html

mod context;
pub use context::{block_on, block_on_or_add_sub_task, yield_now, Context};

mod scheduler;
use scheduler::{Handle, HandleWeak, Scheduler};

mod join;
pub use join::JoinHandle;

mod task;
pub use task::{SubTaskOutput, TaskId};

struct CallOnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> CallOnDrop<F> {
    fn new(f: F) -> Self {
        CallOnDrop(Some(f))
    }
}

impl<F: FnOnce()> Drop for CallOnDrop<F> {
    fn drop(&mut self) {
        self.0.take().unwrap()()
    }
}
