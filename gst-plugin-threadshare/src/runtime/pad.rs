// Copyright (C) 2019-2020 François Laignel <fengalin@free.fr>
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

//! An implementation of `Pad`s to run asynchronous processings.
//!
//! [`PadSink`] & [`PadSrc`] provide an asynchronous API to ease the development of `Element`s in
//! the `threadshare` GStreamer plugins framework.
//!
//! The diagram below shows how the [`PadSrc`] & [`PadSink`] and the related `struct`s integrate in
//! `ts` `Element`s.
//!
//! Note: [`PadSrc`] & [`PadSink`] only support `gst::PadMode::Push` at the moment.
//!
//! ```text
//!    ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓          ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━
//!                    Element A               ┃          ┃          Element B
//!                                            ┃          ┃
//!                 ╭─────────────────╮        ┃          ┃       ╭──────────────────╮
//!                 │     PadSrc      │        ┃          ┃       │      PadSink     │
//!                 │     Handler     │        ┃          ┃       │      Handler     │
//!                 │─────────────────│        ┃          ┃       │──────────────────│
//!                 │ - src_activate* │     ╭──┸──╮    ╭──┸──╮    │ - sink_activate* │
//!                 │ - src_event*    │<────│     │<╌╌╌│     │───>│ - sink_chain*    │
//!                 │ - src_query     │<────│ gst │    │ gst │───>│ - sink_event*    │
//!                 │─────────────────│     │     │    │     │───>│ - sink_query     │
//!                 │ - task fn       │     │ Pad │    │ Pad │    ╰──────────────────╯
//!                 ╰─────────────────╯  ╭─>│     │╌╌╌>│     │─╮            │
//!           ╭───────╯      │           │  ╰──┰──╯    ╰──┰──╯ ╰───────╮    │
//!    ╭────────────╮   ╭────────╮ push* │     ┃          ┃          ╭─────────╮
//!    │ Pad Task ↺ │<──│ PadSrc │───────╯     ┃          ┃          │ PadSink │
//!    ╰────────────╯   ╰────────╯             ┃          ┃          ╰─────────╯
//!    ━━━━━━━━━━━━━━━━━━━━━━│━━━━━━━━━━━━━━━━━┛          ┗━━━━━━━━━━━━━━━│━━━━━━━━━━━━
//!                          ╰───────────────────╮      ╭─────────────────╯
//!                                          ╭──────────────╮
//!                                          │  PadContext  │
//!                                          │╭────────────╮│
//!                                          ││  Context ↺ ││
//!                                          ╰╰────────────╯╯
//! ```
//!
//! Asynchronous operations for both [`PadSrc`] in `Element A` and [`PadSink`] in `Element B` run on
//!  the same [`Context`], which can also be shared by other `Element`s or instances of the same
//! `Element`s in multiple `Pipeline`s.
//!
//! `Element A` & `Element B` can also be linked to non-threadshare `Element`s in which case, they
//! operate in a regular synchronous way.
//!
//! Note that only operations on the streaming thread (serialized events, buffers, serialized
//! queries) are handled from the `PadContext` and asynchronously, everything else operates
//! blocking.
//!
//! [`PadSink`]: struct.PadSink.html
//! [`PadSrc`]: struct.PadSrc.html
//! [`Context`]: ../executor/struct.Context.html

use futures::future;
use futures::future::BoxFuture;
use futures::prelude::*;

use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_fixme, gst_log, gst_loggable_error};
use gst::{FlowError, FlowSuccess};

use std::fmt;
use std::marker::PhantomData;
use std::sync;
use std::sync::{Arc, Weak};

use super::executor::{block_on_or_add_sub_task, Context};
use super::task::Task;
use super::RUNTIME_CAT;

/// Errors related to [`PadSrc`] `Context` handling.
///
/// [`PadSrc`]: struct.PadSrc.html
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PadContextError {
    ActiveContext,
    ActiveTask,
}

impl fmt::Display for PadContextError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PadContextError::ActiveContext => {
                write!(f, "The PadSrc is already operating on a Context")
            }
            PadContextError::ActiveTask => write!(f, "A task is still active"),
        }
    }
}

impl std::error::Error for PadContextError {}

#[inline]
fn event_ret_to_event_full_res(
    ret: bool,
    event_type: gst::EventType,
) -> Result<FlowSuccess, FlowError> {
    if ret {
        Ok(FlowSuccess::Ok)
    } else if event_type == gst::EventType::Caps {
        Err(FlowError::NotNegotiated)
    } else {
        Err(FlowError::Error)
    }
}

#[inline]
fn event_to_event_full(ret: bool, event_type: gst::EventType) -> Result<FlowSuccess, FlowError> {
    event_ret_to_event_full_res(ret, event_type)
}

#[inline]
fn event_to_event_full_serialized(
    ret: BoxFuture<'static, bool>,
    event_type: gst::EventType,
) -> BoxFuture<'static, Result<FlowSuccess, FlowError>> {
    ret.map(move |ret| event_ret_to_event_full_res(ret, event_type))
        .boxed()
}

/// A trait to define `handler`s for [`PadSrc`] callbacks.
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`PadSrc`]: struct.PadSrc.html
/// [`pad` module]: index.html
pub trait PadSrcHandler: Clone + Send + Sync + 'static {
    type ElementImpl: ElementImpl + ObjectSubclass;

    fn src_activate(
        &self,
        pad: &PadSrcRef,
        _imp: &Self::ElementImpl,
        _element: &gst::Element,
    ) -> Result<(), gst::LoggableError> {
        let gst_pad = pad.gst_pad();
        if gst_pad.is_active() {
            gst_debug!(
                RUNTIME_CAT,
                obj: gst_pad,
                "Already activated in {:?} mode ",
                gst_pad.get_mode()
            );
            return Ok(());
        }

        gst_pad
            .activate_mode(gst::PadMode::Push, true)
            .map_err(|err| {
                gst_error!(
                    RUNTIME_CAT,
                    obj: gst_pad,
                    "Error in PadSink activate: {:?}",
                    err
                );
                gst_loggable_error!(RUNTIME_CAT, "Error in PadSink activate: {:?}", err)
            })
    }

    fn src_activatemode(
        &self,
        _pad: &PadSrcRef,
        _imp: &Self::ElementImpl,
        _element: &gst::Element,
        _mode: gst::PadMode,
        _active: bool,
    ) -> Result<(), gst::LoggableError> {
        Ok(())
    }

    fn src_event(
        &self,
        pad: &PadSrcRef,
        _imp: &Self::ElementImpl,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        gst_log!(RUNTIME_CAT, obj: pad.gst_pad(), "Handling {:?}", event);
        pad.gst_pad().event_default(Some(element), event)
    }

    fn src_event_full(
        &self,
        pad: &PadSrcRef,
        imp: &Self::ElementImpl,
        element: &gst::Element,
        event: gst::Event,
    ) -> Result<FlowSuccess, FlowError> {
        // default is to dispatch to `src_event`
        // (as implemented in `gst_pad_send_event_unchecked`)
        let event_type = event.get_type();
        event_to_event_full(self.src_event(pad, imp, element, event), event_type)
    }

    fn src_query(
        &self,
        pad: &PadSrcRef,
        _imp: &Self::ElementImpl,
        element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(RUNTIME_CAT, obj: pad.gst_pad(), "Handling {:?}", query);
        if query.is_serialized() {
            // FIXME serialized queries should be handled with the dataflow
            // but we can't return a `Future` because we couldn't honor QueryRef's lifetime
            false
        } else {
            pad.gst_pad().query_default(Some(element), query)
        }
    }
}

#[derive(Default, Debug)]
struct PadSrcState;

#[derive(Debug)]
struct PadSrcInner {
    state: sync::Mutex<PadSrcState>,
    gst_pad: gst::Pad,
    task: Task,
}

impl PadSrcInner {
    fn new(gst_pad: gst::Pad) -> Self {
        if gst_pad.get_direction() != gst::PadDirection::Src {
            panic!("Wrong pad direction for PadSrc");
        }

        PadSrcInner {
            state: sync::Mutex::new(PadSrcState::default()),
            gst_pad,
            task: Task::default(),
        }
    }
}

/// A [`PadSrc`] which can be moved in [`handler`]s functions and `Future`s.
///
/// Call [`upgrade`] to use the [`PadSrc`].
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`PadSrc`]: struct.PadSrc.html
/// [`handler`]: trait.PadSrcHandler.html
/// [`upgrade`]: struct.PadSrcWeak.html#method.upgrade
/// [`pad` module]: index.html
#[derive(Clone, Debug)]
pub struct PadSrcWeak(Weak<PadSrcInner>);

impl PadSrcWeak {
    pub fn upgrade(&self) -> Option<PadSrcRef<'_>> {
        self.0.upgrade().map(PadSrcRef::new)
    }
}

/// A [`PadSrc`] to be used in `Handler`s functions and `Future`s.
///
/// Call [`downgrade`] if you need to `clone` the [`PadSrc`].
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`PadSrc`]: struct.PadSrc.html
/// [`PadSrcWeak`]: struct.PadSrcWeak.html
/// [`downgrade`]: struct.PadSrcRef.html#method.downgrade
/// [`pad` module]: index.html
#[derive(Debug)]
pub struct PadSrcRef<'a> {
    strong: PadSrcStrong,
    phantom: PhantomData<&'a PadSrcStrong>,
}

impl<'a> PadSrcRef<'a> {
    fn new(inner_arc: Arc<PadSrcInner>) -> Self {
        PadSrcRef {
            strong: PadSrcStrong(inner_arc),
            phantom: PhantomData,
        }
    }

    pub fn gst_pad(&self) -> &gst::Pad {
        self.strong.gst_pad()
    }

    ///// Spawns `future` using current [`PadContext`].
    /////
    ///// # Panics
    /////
    ///// This function panics if the `PadSrc` is not prepared.
    /////
    ///// [`PadContext`]: ../struct.PadContext.html
    //pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    //where
    //    Fut: Future + Send + 'static,
    //    Fut::Output: Send + 'static,
    //{
    //    self.strong.spawn(future)
    //}

    pub fn downgrade(&self) -> PadSrcWeak {
        self.strong.downgrade()
    }

    pub async fn push(&self, buffer: gst::Buffer) -> Result<FlowSuccess, FlowError> {
        self.strong.push(buffer).await
    }

    pub async fn push_list(&self, list: gst::BufferList) -> Result<FlowSuccess, FlowError> {
        self.strong.push_list(list).await
    }

    pub async fn push_event(&self, event: gst::Event) -> bool {
        self.strong.push_event(event).await
    }

    /// `Start` the `Pad` `task`.
    ///
    /// The `Task` will loop on the provided `func`.
    /// The execution occurs on the `Task`'s context.
    pub fn start_task<F, Fut>(&self, func: F)
    where
        F: (FnMut() -> Fut) + Send + 'static,
        Fut: Future<Output = glib::Continue> + Send + 'static,
    {
        self.strong.start_task(func);
    }

    /// Pauses the `Started` `Pad` `Task`.
    pub fn pause_task(&self) {
        self.strong.pause_task();
    }

    /// Cancels the `Started` `Pad` `Task`.
    pub fn cancel_task(&self) {
        self.strong.cancel_task();
    }

    /// Stops the `Started` `Pad` `Task`.
    pub fn stop_task(&self) {
        self.strong.stop_task();
    }

    fn activate_mode_hook(
        &self,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        // Important: don't panic here as the hook is used without `catch_panic_pad_function`
        // in the default `activatemode` handling
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "ActivateMode {:?}, {}", mode, active);

        if mode == gst::PadMode::Pull {
            gst_error!(RUNTIME_CAT, obj: self.gst_pad(), "Pull mode not supported by PadSrc");
            return Err(gst_loggable_error!(
                RUNTIME_CAT,
                "Pull mode not supported by PadSrc"
            ));
        }

        Ok(())
    }
}

#[derive(Debug)]
struct PadSrcStrong(Arc<PadSrcInner>);

impl PadSrcStrong {
    fn new(gst_pad: gst::Pad) -> Self {
        PadSrcStrong(Arc::new(PadSrcInner::new(gst_pad)))
    }

    #[inline]
    fn gst_pad(&self) -> &gst::Pad {
        &self.0.gst_pad
    }

    //#[inline]
    //fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    //where
    //    Fut: Future + Send + 'static,
    //    Fut::Output: Send + 'static,
    //{
    //    let pad_ctx = self.pad_context_priv();
    //    pad_ctx
    //        .as_ref()
    //        .expect("PadContext not initialized")
    //        .spawn(future)
    //}

    #[inline]
    fn downgrade(&self) -> PadSrcWeak {
        PadSrcWeak(Arc::downgrade(&self.0))
    }

    #[inline]
    async fn push(&self, buffer: gst::Buffer) -> Result<FlowSuccess, FlowError> {
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Pushing {:?}", buffer);

        let success = self.gst_pad().push(buffer).map_err(|err| {
            gst_error!(RUNTIME_CAT,
                obj: self.gst_pad(),
                "Failed to push Buffer to PadSrc: {:?}",
                err,
            );
            err
        })?;

        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Processing any pending sub tasks");
        while Context::current_has_sub_tasks() {
            Context::drain_sub_tasks().await?;
        }

        Ok(success)
    }

    #[inline]
    async fn push_list(&self, list: gst::BufferList) -> Result<FlowSuccess, FlowError> {
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Pushing {:?}", list);

        let success = self.gst_pad().push_list(list).map_err(|err| {
            gst_error!(
                RUNTIME_CAT,
                obj: self.gst_pad(),
                "Failed to push BufferList to PadSrc: {:?}",
                err,
            );
            err
        })?;

        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Processing any pending sub tasks");
        while Context::current_has_sub_tasks() {
            Context::drain_sub_tasks().await?;
        }

        Ok(success)
    }

    #[inline]
    async fn push_event(&self, event: gst::Event) -> bool {
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Pushing {:?}", event);

        let was_handled = self.gst_pad().push_event(event);

        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Processing any pending sub tasks");
        while Context::current_has_sub_tasks() {
            if Context::drain_sub_tasks().await.is_err() {
                return false;
            }
        }

        was_handled
    }

    #[inline]
    fn start_task<F, Fut>(&self, func: F)
    where
        F: (FnMut() -> Fut) + Send + 'static,
        Fut: Future<Output = glib::Continue> + Send + 'static,
    {
        self.0.task.start(func);
    }

    #[inline]
    fn pause_task(&self) {
        self.0.task.pause();
    }

    #[inline]
    fn cancel_task(&self) {
        self.0.task.cancel();
    }

    #[inline]
    fn stop_task(&self) {
        self.0.task.stop();
    }
}

/// The `PadSrc` which `Element`s must own.
///
/// Call [`downgrade`] if you need to `clone` the `PadSrc`.
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`downgrade`]: struct.PadSrc.html#method.downgrade
/// [`pad` module]: index.html
#[derive(Debug)]
pub struct PadSrc(PadSrcStrong);

impl PadSrc {
    pub fn new(gst_pad: gst::Pad) -> Self {
        let this = PadSrc(PadSrcStrong::new(gst_pad));
        this.set_default_activatemode_function();

        this
    }

    pub fn new_from_template(templ: &gst::PadTemplate, name: Option<&str>) -> Self {
        Self::new(gst::Pad::new_from_template(templ, name))
    }

    pub fn as_ref(&self) -> PadSrcRef<'_> {
        PadSrcRef::new(Arc::clone(&(self.0).0))
    }

    pub fn gst_pad(&self) -> &gst::Pad {
        self.0.gst_pad()
    }

    pub fn downgrade(&self) -> PadSrcWeak {
        self.0.downgrade()
    }

    pub fn check_reconfigure(&self) -> bool {
        self.gst_pad().check_reconfigure()
    }

    fn set_default_activatemode_function(&self) {
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_activatemode_function(move |gst_pad, _parent, mode, active| {
                // Important: don't panic here as we operate without `catch_panic_pad_function`
                // because we may not know which element the PadSrc is associated to yet
                this_weak
                    .upgrade()
                    .ok_or_else(|| {
                        gst_error!(RUNTIME_CAT, obj: gst_pad, "PadSrc no longer exists");
                        gst_loggable_error!(RUNTIME_CAT, "PadSrc no longer exists")
                    })?
                    .activate_mode_hook(mode, active)
            });
    }

    ///// Spawns `future` using current [`PadContext`].
    /////
    ///// # Panics
    /////
    ///// This function panics if the `PadSrc` is not prepared.
    /////
    ///// [`PadContext`]: ../struct.PadContext.html
    //pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    //where
    //    Fut: Future + Send + 'static,
    //    Fut::Output: Send + 'static,
    //{
    //    self.0.spawn(future)
    //}

    fn init_pad_functions<H: PadSrcHandler>(&self, handler: &H) {
        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_activate_function(move |gst_pad, parent| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || {
                        gst_error!(RUNTIME_CAT, obj: gst_pad, "Panic in PadSrc activate");
                        Err(gst_loggable_error!(RUNTIME_CAT, "Panic in PadSrc activate"))
                    },
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSrc no longer exists");
                        handler.src_activate(&this_ref, imp, element)
                    },
                )
            });

        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_activatemode_function(move |gst_pad, parent, mode, active| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || {
                        gst_error!(RUNTIME_CAT, obj: gst_pad, "Panic in PadSrc activatemode");
                        Err(gst_loggable_error!(
                            RUNTIME_CAT,
                            "Panic in PadSrc activatemode"
                        ))
                    },
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSrc no longer exists");
                        this_ref.activate_mode_hook(mode, active)?;
                        handler.src_activatemode(&this_ref, imp, element, mode, active)
                    },
                )
            });

        // No need to `set_event_function` since `set_event_full_function`
        // overrides it and dispatches to `src_event` when necessary
        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_event_full_function(move |_gst_pad, parent, event| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || Err(FlowError::Error),
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSrc no longer exists");
                        handler.src_event_full(&this_ref, imp, &element, event)
                    },
                )
            });

        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_query_function(move |_gst_pad, parent, query| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || false,
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSrc no longer exists");
                        if !query.is_serialized() {
                            handler.src_query(&this_ref, imp, &element, query)
                        } else {
                            gst_fixme!(RUNTIME_CAT, obj: this_ref.gst_pad(), "Serialized Query not supported");
                            false
                        }
                    },
                )
            });
    }

    pub fn prepare<H: PadSrcHandler>(
        &self,
        context: Context,
        handler: &H,
    ) -> Result<(), super::task::TaskError> {
        let _state = (self.0).0.state.lock().unwrap();
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Preparing");

        (self.0).0.task.prepare(context)?;

        self.init_pad_functions(handler);

        Ok(())
    }

    pub fn prepare_with_func<H: PadSrcHandler, F, Fut>(
        &self,
        context: Context,
        handler: &H,
        prepare_func: F,
    ) -> Result<(), super::task::TaskError>
    where
        F: (FnOnce() -> Fut) + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let _state = (self.0).0.state.lock().unwrap();
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Preparing");

        (self.0).0.task.prepare_with_func(context, prepare_func)?;

        self.init_pad_functions(handler);

        Ok(())
    }

    /// Releases the resources held by this `PadSrc`.
    pub fn unprepare(&self) -> Result<(), PadContextError> {
        let _state = (self.0).0.state.lock().unwrap();
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Unpreparing");

        (self.0)
            .0
            .task
            .unprepare()
            .map_err(|_| PadContextError::ActiveTask)?;

        self.gst_pad()
            .set_activate_function(move |_gst_pad, _parent| {
                Err(gst_loggable_error!(RUNTIME_CAT, "PadSrc unprepared"))
            });
        self.set_default_activatemode_function();
        self.gst_pad()
            .set_event_function(move |_gst_pad, _parent, _event| false);
        self.gst_pad()
            .set_event_full_function(move |_gst_pad, _parent, _event| Err(FlowError::Flushing));
        self.gst_pad()
            .set_query_function(move |_gst_pad, _parent, _query| false);

        Ok(())
    }

    pub async fn push(&self, buffer: gst::Buffer) -> Result<FlowSuccess, FlowError> {
        self.0.push(buffer).await
    }

    pub async fn push_list(&self, list: gst::BufferList) -> Result<FlowSuccess, FlowError> {
        self.0.push_list(list).await
    }

    pub async fn push_event(&self, event: gst::Event) -> bool {
        self.0.push_event(event).await
    }

    /// `Start` the `Pad` `task`.
    ///
    /// The `Task` will loop on the provided `func`.
    /// The execution occurs on the `Task`'s context.
    pub fn start_task<F, Fut>(&self, func: F)
    where
        F: (FnMut() -> Fut) + Send + 'static,
        Fut: Future<Output = glib::Continue> + Send + 'static,
    {
        self.0.start_task(func);
    }

    pub fn pause_task(&self) {
        self.0.pause_task();
    }

    pub fn cancel_task(&self) {
        self.0.cancel_task();
    }

    pub fn stop_task(&self) {
        self.0.stop_task();
    }
}

/// A trait to define `handler`s for [`PadSink`] callbacks.
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`PadSink`]: struct.PadSink.html
/// [`pad` module]: index.html
pub trait PadSinkHandler: Clone + Send + Sync + 'static {
    type ElementImpl: ElementImpl + ObjectSubclass;

    fn sink_activate(
        &self,
        pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        _element: &gst::Element,
    ) -> Result<(), gst::LoggableError> {
        let gst_pad = pad.gst_pad();
        if gst_pad.is_active() {
            gst_debug!(
                RUNTIME_CAT,
                obj: gst_pad,
                "Already activated in {:?} mode ",
                gst_pad.get_mode()
            );
            return Ok(());
        }

        gst_pad
            .activate_mode(gst::PadMode::Push, true)
            .map_err(|err| {
                gst_error!(
                    RUNTIME_CAT,
                    obj: gst_pad,
                    "Error in PadSink activate: {:?}",
                    err
                );
                gst_loggable_error!(RUNTIME_CAT, "Error in PadSink activate: {:?}", err)
            })
    }

    fn sink_activatemode(
        &self,
        _pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        _element: &gst::Element,
        _mode: gst::PadMode,
        _active: bool,
    ) -> Result<(), gst::LoggableError> {
        Ok(())
    }

    fn sink_chain(
        &self,
        _pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        _element: &gst::Element,
        _buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<FlowSuccess, FlowError>> {
        future::err(FlowError::NotSupported).boxed()
    }

    fn sink_chain_list(
        &self,
        _pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        _element: &gst::Element,
        _buffer_list: gst::BufferList,
    ) -> BoxFuture<'static, Result<FlowSuccess, FlowError>> {
        future::err(FlowError::NotSupported).boxed()
    }

    fn sink_event(
        &self,
        pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        assert!(!event.is_serialized());
        gst_log!(RUNTIME_CAT, obj: pad.gst_pad(), "Handling {:?}", event);
        pad.gst_pad().event_default(Some(element), event)
    }

    fn sink_event_serialized(
        &self,
        pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        element: &gst::Element,
        event: gst::Event,
    ) -> BoxFuture<'static, bool> {
        assert!(event.is_serialized());
        let pad_weak = pad.downgrade();
        let element = element.clone();

        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            gst_log!(RUNTIME_CAT, obj: pad.gst_pad(), "Handling {:?}", event);

            pad.gst_pad().event_default(Some(&element), event)
        }
        .boxed()
    }

    fn sink_event_full(
        &self,
        pad: &PadSinkRef,
        imp: &Self::ElementImpl,
        element: &gst::Element,
        event: gst::Event,
    ) -> Result<FlowSuccess, FlowError> {
        assert!(!event.is_serialized());
        // default is to dispatch to `sink_event`
        // (as implemented in `gst_pad_send_event_unchecked`)
        let event_type = event.get_type();
        event_to_event_full(self.sink_event(pad, imp, element, event), event_type)
    }

    fn sink_event_full_serialized(
        &self,
        pad: &PadSinkRef,
        imp: &Self::ElementImpl,
        element: &gst::Element,
        event: gst::Event,
    ) -> BoxFuture<'static, Result<FlowSuccess, FlowError>> {
        assert!(event.is_serialized());
        // default is to dispatch to `sink_event`
        // (as implemented in `gst_pad_send_event_unchecked`)
        let event_type = event.get_type();
        event_to_event_full_serialized(
            self.sink_event_serialized(pad, imp, element, event),
            event_type,
        )
    }

    fn sink_query(
        &self,
        pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        if query.is_serialized() {
            gst_log!(RUNTIME_CAT, obj: pad.gst_pad(), "Dropping {:?}", query);
            // FIXME serialized queries should be handled with the dataflow
            // but we can't return a `Future` because we couldn't honor QueryRef's lifetime
            false
        } else {
            gst_log!(RUNTIME_CAT, obj: pad.gst_pad(), "Handling {:?}", query);
            pad.gst_pad().query_default(Some(element), query)
        }
    }
}

#[derive(Debug)]
struct PadSinkInner {
    gst_pad: gst::Pad,
}

impl PadSinkInner {
    fn new(gst_pad: gst::Pad) -> Self {
        if gst_pad.get_direction() != gst::PadDirection::Sink {
            panic!("Wrong pad direction for PadSink");
        }

        PadSinkInner { gst_pad }
    }
}

/// A [`PadSink`] which can be moved in `Handler`s functions and `Future`s.
///
/// Call [`upgrade`] to use the [`PadSink`].
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`PadSink`]: struct.PadSink.html
/// [`upgrade`]: struct.PadSinkWeak.html#method.upgrade
/// [`pad` module]: index.html
#[derive(Clone, Debug)]
pub struct PadSinkWeak(Weak<PadSinkInner>);

impl PadSinkWeak {
    pub fn upgrade(&self) -> Option<PadSinkRef<'_>> {
        self.0.upgrade().map(PadSinkRef::new)
    }
}

/// A [`PadSink`] to be used in [`handler`]s functions and `Future`s.
///
/// Call [`downgrade`] if you need to `clone` the [`PadSink`].
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`PadSink`]: struct.PadSink.html
/// [`handler`]: trait.PadSinkHandler.html
/// [`downgrade`]: struct.PadSinkRef.html#method.downgrade
/// [`pad` module]: index.html
#[derive(Debug)]
pub struct PadSinkRef<'a> {
    strong: PadSinkStrong,
    phantom: PhantomData<&'a PadSrcStrong>,
}

impl<'a> PadSinkRef<'a> {
    fn new(inner_arc: Arc<PadSinkInner>) -> Self {
        PadSinkRef {
            strong: PadSinkStrong(inner_arc),
            phantom: PhantomData,
        }
    }

    pub fn gst_pad(&self) -> &gst::Pad {
        self.strong.gst_pad()
    }

    pub fn downgrade(&self) -> PadSinkWeak {
        self.strong.downgrade()
    }

    fn activate_mode_hook(
        &self,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        // Important: don't panic here as the hook is used without `catch_panic_pad_function`
        // in the default `activatemode` handling
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "ActivateMode {:?}, {}", mode, active);

        if mode == gst::PadMode::Pull {
            gst_error!(RUNTIME_CAT, obj: self.gst_pad(), "Pull mode not supported by PadSink");
            return Err(gst_loggable_error!(
                RUNTIME_CAT,
                "Pull mode not supported by PadSink"
            ));
        }

        Ok(())
    }

    fn handle_future(
        &self,
        fut: impl Future<Output = Result<FlowSuccess, FlowError>> + Send + 'static,
    ) -> Result<FlowSuccess, FlowError> {
        // First try to add it as a sub task to the current task, if any
        if let Err(fut) = Context::add_sub_task(fut.map(|res| res.map(drop))) {
            // FIXME: update comments below
            // Not on a context thread: execute the Future immediately.
            //
            // - If there is no PadContext, we don't have any other options.
            // - If there is a PadContext, it means that we received it from
            //   an upstream element, but there is at least one non-ts element
            //   operating on another thread in between, so we can't take
            //   advantage of the task queue.
            //
            // Note: we don't use `crate::runtime::executor::block_on` here
            // because `Context::is_context_thread()` is checked in the `if`
            // statement above.
            block_on_or_add_sub_task(fut.map(|res| res.map(|_| gst::FlowSuccess::Ok)))
                .unwrap_or(Ok(gst::FlowSuccess::Ok))
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }
}

#[derive(Debug)]
struct PadSinkStrong(Arc<PadSinkInner>);

impl PadSinkStrong {
    fn new(gst_pad: gst::Pad) -> Self {
        PadSinkStrong(Arc::new(PadSinkInner::new(gst_pad)))
    }

    fn gst_pad(&self) -> &gst::Pad {
        &self.0.gst_pad
    }

    fn downgrade(&self) -> PadSinkWeak {
        PadSinkWeak(Arc::downgrade(&self.0))
    }
}

/// The `PadSink` which `Element`s must own.
///
/// Call [`downgrade`] if you need to `clone` the `PadSink`.
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`downgrade`]: struct.PadSink.html#method.downgrade
/// [`pad` module]: index.html
#[derive(Debug)]
pub struct PadSink(PadSinkStrong);

impl PadSink {
    pub fn new(gst_pad: gst::Pad) -> Self {
        let this = PadSink(PadSinkStrong::new(gst_pad));
        this.set_default_activatemode_function();

        this
    }

    pub fn new_from_template(templ: &gst::PadTemplate, name: Option<&str>) -> Self {
        Self::new(gst::Pad::new_from_template(templ, name))
    }

    pub fn gst_pad(&self) -> &gst::Pad {
        self.0.gst_pad()
    }

    pub fn downgrade(&self) -> PadSinkWeak {
        self.0.downgrade()
    }

    fn set_default_activatemode_function(&self) {
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_activatemode_function(move |gst_pad, _parent, mode, active| {
                // Important: don't panic here as we operate without `catch_panic_pad_function`
                // because we may not know which element the PadSrc is associated to yet
                this_weak
                    .upgrade()
                    .ok_or_else(|| {
                        gst_error!(RUNTIME_CAT, obj: gst_pad, "PadSink no longer exists");
                        gst_loggable_error!(RUNTIME_CAT, "PadSink no longer exists")
                    })?
                    .activate_mode_hook(mode, active)
            });
    }

    fn init_pad_functions<H: PadSinkHandler>(&self, handler: &H) {
        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_activate_function(move |gst_pad, parent| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || {
                        gst_error!(RUNTIME_CAT, obj: gst_pad, "Panic in PadSink activate");
                        Err(gst_loggable_error!(
                            RUNTIME_CAT,
                            "Panic in PadSink activate"
                        ))
                    },
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSink no longer exists");
                        handler.sink_activate(&this_ref, imp, element)
                    },
                )
            });

        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_activatemode_function(move |gst_pad, parent, mode, active| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || {
                        gst_error!(RUNTIME_CAT, obj: gst_pad, "Panic in PadSink activatemode");
                        Err(gst_loggable_error!(
                            RUNTIME_CAT,
                            "Panic in PadSink activatemode"
                        ))
                    },
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSink no longer exists");
                        this_ref.activate_mode_hook(mode, active)?;

                        handler.sink_activatemode(&this_ref, imp, element, mode, active)
                    },
                )
            });

        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_chain_function(move |_gst_pad, parent, buffer| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || Err(FlowError::Error),
                    move |imp, element| {
                        if Context::current_has_sub_tasks() {
                            let this_weak = this_weak.clone();
                            let handler = handler.clone();
                            let element = element.clone();
                            let delayed_fut = async move {
                                let imp =
                                    <H::ElementImpl as ObjectSubclass>::from_instance(&element);
                                let this_ref =
                                    this_weak.upgrade().ok_or(gst::FlowError::Flushing)?;
                                handler.sink_chain(&this_ref, imp, &element, buffer).await
                            };
                            let _ = Context::add_sub_task(delayed_fut.map(|res| res.map(drop)));

                            Ok(gst::FlowSuccess::Ok)
                        } else {
                            let this_ref = this_weak.upgrade().expect("PadSink no longer exists");
                            let chain_fut = handler.sink_chain(&this_ref, imp, &element, buffer);
                            this_ref.handle_future(chain_fut)
                        }
                    },
                )
            });

        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_chain_list_function(move |_gst_pad, parent, list| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || Err(FlowError::Error),
                    move |imp, element| {
                        if Context::current_has_sub_tasks() {
                            let this_weak = this_weak.clone();
                            let handler = handler.clone();
                            let element = element.clone();
                            let delayed_fut = async move {
                                let imp =
                                    <H::ElementImpl as ObjectSubclass>::from_instance(&element);
                                let this_ref =
                                    this_weak.upgrade().ok_or(gst::FlowError::Flushing)?;
                                handler
                                    .sink_chain_list(&this_ref, imp, &element, list)
                                    .await
                            };
                            let _ = Context::add_sub_task(delayed_fut.map(|res| res.map(drop)));

                            Ok(gst::FlowSuccess::Ok)
                        } else {
                            let this_ref = this_weak.upgrade().expect("PadSink no longer exists");
                            let chain_list_fut =
                                handler.sink_chain_list(&this_ref, imp, &element, list);
                            this_ref.handle_future(chain_list_fut)
                        }
                    },
                )
            });

        // No need to `set_event_function` since `set_event_full_function`
        // overrides it and dispatches to `sink_event` when necessary
        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_event_full_function(move |_gst_pad, parent, event| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || Err(FlowError::Error),
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSink no longer exists");
                        if event.is_serialized() {
                            if Context::current_has_sub_tasks() {
                                let this_weak = this_weak.clone();
                                let handler = handler.clone();
                                let element = element.clone();
                                let delayed_fut = async move {
                                    let imp =
                                        <H::ElementImpl as ObjectSubclass>::from_instance(&element);
                                    let this_ref =
                                        this_weak.upgrade().ok_or(gst::FlowError::Flushing)?;

                                    handler
                                        .sink_event_full_serialized(&this_ref, imp, &element, event)
                                        .await
                                };
                                let _ = Context::add_sub_task(delayed_fut.map(|res| res.map(drop)));

                                Ok(gst::FlowSuccess::Ok)
                            } else {
                                let event_fut = handler
                                    .sink_event_full_serialized(&this_ref, imp, &element, event);
                                this_ref.handle_future(event_fut)
                            }
                        } else {
                            handler.sink_event_full(&this_ref, imp, &element, event)
                        }
                    },
                )
            });

        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_query_function(move |_gst_pad, parent, query| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || false,
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSink no longer exists");
                        if !query.is_serialized() {
                            handler.sink_query(&this_ref, imp, &element, query)
                        } else {
                            gst_fixme!(RUNTIME_CAT, obj: this_ref.gst_pad(), "Serialized Query not supported");
                            false
                        }
                    },
                )
            });
    }

    pub fn prepare<H: PadSinkHandler>(&self, handler: &H) {
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Preparing");
        self.init_pad_functions(handler);
    }

    /// Releases the resources held by this `PadSink`.
    pub fn unprepare(&self) {
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Unpreparing");

        self.gst_pad()
            .set_activate_function(move |_gst_pad, _parent| {
                Err(gst_loggable_error!(RUNTIME_CAT, "PadSink unprepared"))
            });
        self.set_default_activatemode_function();
        self.gst_pad()
            .set_chain_function(move |_gst_pad, _parent, _buffer| Err(FlowError::Flushing));
        self.gst_pad()
            .set_chain_list_function(move |_gst_pad, _parent, _list| Err(FlowError::Flushing));
        self.gst_pad()
            .set_event_function(move |_gst_pad, _parent, _event| false);
        self.gst_pad()
            .set_event_full_function(move |_gst_pad, _parent, _event| Err(FlowError::Flushing));
        self.gst_pad()
            .set_query_function(move |_gst_pad, _parent, _query| false);
    }
}
