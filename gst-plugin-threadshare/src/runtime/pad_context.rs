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

//! Types that allow `Pad`s to operate within the threadshare runtime.

use futures::prelude::*;

use glib;
use glib::{glib_boxed_derive_traits, glib_boxed_type};

use std::marker::PhantomData;

use super::executor::{Context, ContextWeak, JoinHandle, TaskOutput, TaskQueueId};

#[derive(Clone)]
pub struct PadContextWeak {
    context_weak: ContextWeak,
    queue_id: TaskQueueId,
}

impl PadContextWeak {
    pub fn upgrade(&self) -> Option<PadContextRef> {
        self.context_weak
            .upgrade()
            .map(|inner| PadContextRef::new(inner, self.queue_id))
    }
}

impl glib::subclass::boxed::BoxedType for PadContextWeak {
    const NAME: &'static str = "TsPadContext";

    glib_boxed_type!();
}

glib_boxed_derive_traits!(PadContextWeak);

impl std::fmt::Debug for PadContextWeak {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self.context_weak.upgrade() {
            Some(context) => write!(
                f,
                "PadContext {{ context: '{}'), {:?} }}",
                context.name(),
                self.queue_id
            ),
            None => write!(
                f,
                "PadContext {{ context: _NO LONGER AVAILABLE_, {:?} }}",
                self.queue_id
            ),
        }
    }
}

#[derive(Debug)]
pub struct PadContextRef<'a> {
    strong: PadContextStrong,
    phantom: PhantomData<&'a PadContextStrong>,
}

impl<'a> PadContextRef<'a> {
    fn new(context: Context, queue_id: TaskQueueId) -> Self {
        PadContextRef {
            strong: PadContextStrong { context, queue_id },
            phantom: PhantomData,
        }
    }
}

impl<'a> PadContextRef<'a> {
    pub fn downgrade(&self) -> PadContextWeak {
        self.strong.downgrade()
    }

    pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.strong.context.spawn(future)
    }

    pub fn add_pending_task<T>(&self, task: T)
    where
        T: Future<Output = TaskOutput> + Send + 'static,
    {
        self.strong.add_pending_task(task);
    }

    pub fn drain_pending_tasks(&self) -> Option<impl Future<Output = TaskOutput>> {
        self.strong.drain_pending_tasks()
    }

    pub fn clear_pending_tasks(&self) {
        self.strong.clear_pending_tasks();
    }

    pub fn context(&self) -> &Context {
        &self.strong.context
    }
}

impl std::fmt::Display for PadContextRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.strong.fmt(f)
    }
}

#[derive(Debug)]
struct PadContextStrong {
    context: Context,
    queue_id: TaskQueueId,
}

impl PadContextStrong {
    #[inline]
    pub fn downgrade(&self) -> PadContextWeak {
        PadContextWeak {
            context_weak: self.context.downgrade(),
            queue_id: self.queue_id,
        }
    }

    #[inline]
    fn add_pending_task<T>(&self, task: T)
    where
        T: Future<Output = TaskOutput> + Send + 'static,
    {
        self.context
            .add_task(self.queue_id, task)
            .expect("TaskQueueId controlled by TaskContext");
    }

    #[inline]
    fn drain_pending_tasks(&self) -> Option<impl Future<Output = TaskOutput>> {
        self.context.drain_task_queue(self.queue_id)
    }

    #[inline]
    fn clear_pending_tasks(&self) {
        self.context.clear_task_queue(self.queue_id);
    }
}

impl std::fmt::Display for PadContextStrong {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Context('{}'), {:?}", self.context.name(), self.queue_id)
    }
}

/// A wrapper on a [`Context`] with additional features for [`PadSrc`] & [`PadSink`].
///
/// [`Context`]: ../executor/struct.Context.html
/// [`PadSrc`]: ../pad/struct.PadSrc.html
/// [`PadSink`]: ../pad/struct.PadSink.html
#[derive(Debug)]
pub struct PadContext(PadContextStrong);

impl PadContext {
    pub fn new(context: Context) -> Self {
        PadContext(PadContextStrong {
            queue_id: context.acquire_task_queue_id(),
            context,
        })
    }

    pub fn downgrade(&self) -> PadContextWeak {
        self.0.downgrade()
    }

    pub fn as_ref(&self) -> PadContextRef<'_> {
        PadContextRef::new(self.0.context.clone(), self.0.queue_id)
    }

    pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.0.context.spawn(future)
    }

    pub fn drain_pending_tasks(&self) -> Option<impl Future<Output = TaskOutput>> {
        self.0.drain_pending_tasks()
    }

    pub fn clear_pending_tasks(&self) {
        self.0.clear_pending_tasks();
    }

    pub(super) fn new_sticky_event(&self) -> gst::Event {
        let s = gst::Structure::new("ts-pad-context", &[("pad-context", &self.downgrade())]);
        gst::Event::new_custom_downstream_sticky(s).build()
    }

    #[inline]
    pub fn is_pad_context_sticky_event(event: &gst::event::CustomDownstreamSticky) -> bool {
        event.get_structure().unwrap().get_name() == "ts-pad-context"
    }

    #[inline]
    pub fn is_pad_context_event(event: &gst::Event) -> bool {
        if let gst::EventView::CustomDownstreamSticky(e) = event.view() {
            return Self::is_pad_context_sticky_event(&e);
        }

        false
    }

    pub fn check_pad_context_event(event: &gst::Event) -> Option<PadContextWeak> {
        if let gst::EventView::CustomDownstreamSticky(e) = event.view() {
            if Self::is_pad_context_sticky_event(&e) {
                let s = e.get_structure().unwrap();
                let pad_context = s
                    .get::<&PadContextWeak>("pad-context")
                    .expect("event field")
                    .expect("missing event field")
                    .clone();

                Some(pad_context)
            } else {
                None
            }
        } else {
            None
        }
    }
}

impl Drop for PadContext {
    fn drop(&mut self) {
        self.0.context.release_task_queue(self.0.queue_id);
    }
}

impl std::fmt::Display for PadContext {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
