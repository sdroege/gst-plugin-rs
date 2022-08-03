// Copyright (C) 2019-2022 François Laignel <fengalin@free.fr>
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
//
// SPDX-License-Identifier: LGPL-2.1-or-later

//! An execution loop to run asynchronous processing.

use futures::channel::mpsc as async_mpsc;
use futures::channel::oneshot;
use futures::future::{self, abortable, AbortHandle, Aborted, BoxFuture};
use futures::prelude::*;
use futures::stream::StreamExt;

use std::fmt;
use std::ops::Deref;
use std::pin::Pin;
use std::stringify;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;

use super::{Context, JoinHandle, RUNTIME_CAT};

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy)]
pub enum TaskState {
    Error,
    Flushing,
    Paused,
    PausedFlushing,
    Prepared,
    Preparing,
    Started,
    Stopped,
    Unprepared,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Trigger {
    Error,
    FlushStart,
    FlushStop,
    Pause,
    Prepare,
    Start,
    Stop,
    Unprepare,
}

/// Transition success details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionOk {
    /// Transition completed successfully.
    Complete {
        origin: TaskState,
        target: TaskState,
    },
    /// Not waiting for transition result.
    ///
    /// This is to prevent:
    /// - A deadlock when executing a transition action.
    /// - A potential infinite wait when pausing a running loop
    ///   which could be awaiting for an `iterate` to complete.
    NotWaiting { trigger: Trigger, origin: TaskState },
    /// Skipping triggering event due to current state.
    Skipped { trigger: Trigger, state: TaskState },
}

/// TriggeringEvent error details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransitionError {
    pub trigger: Trigger,
    pub state: TaskState,
    pub err_msg: gst::ErrorMessage,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:?} from state {:?}: {:?}",
            self.trigger, self.state, self.err_msg
        )
    }
}

impl std::error::Error for TransitionError {}

impl From<TransitionError> for gst::ErrorMessage {
    fn from(err: TransitionError) -> Self {
        err.err_msg
    }
}

/// Transition status.
///
/// A state transition occurs as a result of a triggering event.
/// The triggering event is asynchronously handled by a state machine
/// running on a [`Context`].
#[must_use = "This `TransitionStatus` may be `Pending`. In most cases it should be awaited. See `await_maybe_on_context`"]
pub enum TransitionStatus {
    /// Transition result is ready.
    Ready(Result<TransitionOk, TransitionError>),
    /// Transition is pending.
    Pending {
        trigger: Trigger,
        origin: TaskState,
        res_fut: Pin<Box<dyn Future<Output = Result<TransitionOk, TransitionError>> + Send>>,
    },
}

impl TransitionStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, TransitionStatus::Ready { .. })
    }

    pub fn is_pending(&self) -> bool {
        matches!(self, TransitionStatus::Pending { .. })
    }

    /// Converts the `TransitionStatus` into a `Result`.
    ///
    /// This function allows getting the `TransitionError` when
    /// the transition result is ready without `await`ing nor blocking.
    ///
    /// See also [`Self::await_maybe_on_context`].
    // FIXME once stabilized, this could use https://github.com/rust-lang/rust/issues/84277
    pub fn check(self) -> Result<TransitionStatus, TransitionError> {
        match self {
            TransitionStatus::Ready(Err(err)) => Err(err),
            other => Ok(other),
        }
    }

    /// Awaits for this transition to complete, possibly while running on a [`Context`].
    ///
    /// Notes:
    ///
    /// - When running in an `async` block within a running transition or
    ///   task iteration, don't await for the transition as it would deadlock.
    ///   Use [`Self::check`] to make sure the state transition is valid.
    /// - When running in an `async` block out of a running transition or
    ///   task iteration, just `.await` normally. E.g.:
    ///
    /// ```
    /// # use gstthreadshare::runtime::task::{Task, TransitionOk, TransitionError};
    /// # async fn async_fn() -> Result<TransitionOk, TransitionError> {
    /// # let task = Task::default();
    ///   let flush_ok = task.flush_start().await?;
    /// # Ok(flush_ok)
    /// # }
    /// ```
    ///
    /// This function makes sure the transition completes successfully or
    /// produces an error. It must be used in situations where we don't know
    /// whether we are running on a [`Context`] or not. This is the case for
    /// functions in [`PadSrc`] and [`PadSink`] as well as the synchronous
    /// functions transitively called from them.
    ///
    /// As an example, a `PadSrc::src_event` function which handles a
    /// `FlushStart` could call:
    ///
    /// ```
    /// # fn src_event() -> bool {
    /// # let task = gstthreadshare::runtime::Task::default();
    ///   return task
    ///       .flush_start()
    ///       .await_maybe_on_context()
    ///       .is_ok();
    /// # }
    /// ```
    ///
    /// If the transition is already complete, the result is returned immediately.
    ///
    /// If we are NOT running on a [`Context`], the transition result is awaited
    /// by blocking on current thread and the result is returned.
    ///
    /// If we are running on a [`Context`], the transition result is awaited
    /// in a sub task for current [`Context`]'s Scheduler task. As a consequence,
    /// the sub task will be awaited in usual [`Context::drain_sub_tasks`]
    /// rendezvous, ensuring some kind of synchronization. To avoid deadlocks,
    /// `Ok(TransitionOk::NotWaiting { .. })` is immediately returned.
    ///
    /// [`PadSrc`]: ../pad/struct.PadSrc.html
    /// [`PadSink`]: ../pad/struct.PadSink.html
    pub fn await_maybe_on_context(self) -> Result<TransitionOk, TransitionError> {
        use TransitionStatus::*;
        match self {
            Pending {
                trigger,
                origin,
                res_fut,
            } => {
                if let Some(cur_ctx) = Context::current() {
                    gst::debug!(
                        RUNTIME_CAT,
                        "Awaiting for {:?} ack in a subtask on context {}",
                        trigger,
                        cur_ctx.name()
                    );
                    let _ = Context::add_sub_task(async move {
                        let res = res_fut.await;
                        if res.is_ok() {
                            gst::log!(RUNTIME_CAT, "Received ack {:?} for {:?}", res, trigger);
                        } else {
                            gst::error!(RUNTIME_CAT, "Received ack {:?} for {:?}", res, trigger);
                        }

                        Ok(())
                    });

                    Ok(TransitionOk::NotWaiting { trigger, origin })
                } else {
                    gst::debug!(
                        RUNTIME_CAT,
                        "Awaiting for {:?} ack on current thread",
                        trigger,
                    );
                    futures::executor::block_on(res_fut)
                }
            }
            Ready(res) => res,
        }
    }

    /// Awaits for this transition to complete by blocking current thread.
    ///
    /// This function blocks until the transition completes successfully or
    /// produces an error.
    ///
    /// In situations where we don't know whether we are running on a [`Context`]
    /// or not, use [`Self::await_maybe_on_context`] instead.
    ///
    /// # Panics
    ///
    /// Panics if current thread is a [`Context`] thread.
    pub fn block_on(self) -> Result<TransitionOk, TransitionError> {
        assert!(!Context::is_context_thread());
        use TransitionStatus::*;
        match self {
            Pending {
                trigger, res_fut, ..
            } => {
                gst::debug!(
                    RUNTIME_CAT,
                    "Awaiting for {:?} ack on current thread",
                    trigger,
                );
                futures::executor::block_on(res_fut)
            }
            Ready(res) => res,
        }
    }
}

impl Future for TransitionStatus {
    type Output = Result<TransitionOk, TransitionError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        use TransitionStatus::*;

        match &mut *self {
            Ready(res) => Poll::Ready(res.clone()),
            Pending { res_fut, .. } => match Pin::new(res_fut).poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(res) => {
                    *self = Ready(res.clone());

                    Poll::Ready(res)
                }
            },
        }
    }
}

impl From<TransitionOk> for TransitionStatus {
    fn from(ok: TransitionOk) -> Self {
        Self::Ready(Ok(ok))
    }
}

impl From<TransitionError> for TransitionStatus {
    fn from(err: TransitionError) -> Self {
        Self::Ready(Err(err))
    }
}

// Explicit impl due to `res_fut` not implementing `Debug`.
impl fmt::Debug for TransitionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use TransitionStatus::*;
        match self {
            Ready(res) => f.debug_tuple("Ready").field(res).finish(),
            Pending {
                trigger, origin, ..
            } => f
                .debug_struct("Pending")
                .field("trigger", trigger)
                .field("origin", origin)
                .finish(),
        }
    }
}

/// Implementation trait for `Task`s.
///
/// Defines implementations for state transition actions and error handlers.
pub trait TaskImpl: Send + 'static {
    fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        future::ok(()).boxed()
    }

    fn unprepare(&mut self) -> BoxFuture<'_, ()> {
        future::ready(()).boxed()
    }

    fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        future::ok(()).boxed()
    }

    /// Executes an iteration in `TaskState::Started`.
    fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>>;

    fn pause(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        future::ok(()).boxed()
    }

    fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        future::ok(()).boxed()
    }

    fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        future::ok(()).boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        future::ok(()).boxed()
    }

    /// Handles an error occuring during the execution of an iteration.
    ///
    /// This handler also catches errors returned by subtasks spawned by the iteration.
    ///
    /// If the error is unrecoverable, implementations might use `gst::Element::post_error_message`
    /// and return `Trigger::Error`.
    ///
    /// Otherwise, handle the error and return the requested `Transition` to recover.
    ///
    /// Default behaviour depends on the `err`:
    ///
    /// - `FlowError::Flushing` -> `Trigger::FlushStart`.
    /// - `FlowError::Eos` -> `Trigger::Stop`.
    /// - Other `FlowError` -> `Trigger::Error`.
    fn handle_iterate_error(&mut self, err: gst::FlowError) -> BoxFuture<'_, Trigger> {
        async move {
            match err {
                gst::FlowError::Flushing => {
                    gst::debug!(
                        RUNTIME_CAT,
                        "TaskImpl iterate returned Flushing. Posting FlushStart"
                    );
                    Trigger::FlushStart
                }
                gst::FlowError::Eos => {
                    gst::debug!(RUNTIME_CAT, "TaskImpl iterate returned Eos. Posting Stop");
                    Trigger::Stop
                }
                other => {
                    gst::error!(
                        RUNTIME_CAT,
                        "TaskImpl iterate returned {:?}. Posting Error",
                        other
                    );
                    Trigger::Error
                }
            }
        }
        .boxed()
    }

    /// Handles an error occuring during the execution of a transition action.
    ///
    /// This handler also catches errors returned by subtasks spawned by the transition action.
    ///
    /// If the error is unrecoverable, implementations might use `gst::Element::post_error_message`
    /// and return `Trigger::Error`.
    ///
    /// Otherwise, handle the error and return the recovering `Trigger`.
    ///
    /// Default is to `gst::error` log and return `Trigger::Error`.
    fn handle_action_error(
        &mut self,
        trigger: Trigger,
        state: TaskState,
        err: gst::ErrorMessage,
    ) -> BoxFuture<'_, Trigger> {
        async move {
            gst::error!(
                RUNTIME_CAT,
                "TaskImpl transition action error during {:?} from {:?}: {:?}. Posting Trigger::Error",
                trigger,
                state,
                err,
            );

            Trigger::Error
        }
        .boxed()
    }
}

type AckSender = oneshot::Sender<Result<TransitionOk, TransitionError>>;
type AckReceiver = oneshot::Receiver<Result<TransitionOk, TransitionError>>;

struct TriggeringEvent {
    trigger: Trigger,
    ack_tx: AckSender,
}

impl TriggeringEvent {
    fn new(trigger: Trigger) -> (Self, AckReceiver) {
        let (ack_tx, ack_rx) = oneshot::channel();
        let req = TriggeringEvent { trigger, ack_tx };

        (req, ack_rx)
    }

    fn send_ack(self, res: Result<TransitionOk, TransitionError>) {
        let _ = self.ack_tx.send(res);
    }

    fn send_err_ack(self) {
        let res = Err(TransitionError {
            trigger: self.trigger,
            state: TaskState::Error,
            err_msg: gst::error_msg!(
                gst::CoreError::StateChange,
                [
                    "Triggering Event {:?} rejected due to a previous unrecoverable error",
                    self.trigger,
                ]
            ),
        });

        self.send_ack(res);
    }
}

impl fmt::Debug for TriggeringEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TriggeringEvent")
            .field("trigger", &self.trigger)
            .finish()
    }
}

#[derive(Debug)]
struct StateMachineHandle {
    join_handle: JoinHandle<()>,
    triggering_evt_tx: async_mpsc::Sender<TriggeringEvent>,
    context: Context,
}

impl StateMachineHandle {
    fn trigger(&mut self, trigger: Trigger) -> AckReceiver {
        let (triggering_evt, ack_rx) = TriggeringEvent::new(trigger);

        gst::log!(RUNTIME_CAT, "Pushing {:?}", triggering_evt);
        self.triggering_evt_tx.try_send(triggering_evt).unwrap();

        self.context.unpark();

        ack_rx
    }

    async fn join(self) {
        self.join_handle
            .await
            .expect("state machine shouldn't have been cancelled");
    }
}

#[derive(Debug)]
struct TaskInner {
    state: TaskState,
    state_machine_handle: Option<StateMachineHandle>,
    loop_abort_handle: Option<AbortHandle>,
}

impl Default for TaskInner {
    fn default() -> Self {
        TaskInner {
            state: TaskState::Unprepared,
            state_machine_handle: None,
            loop_abort_handle: None,
        }
    }
}

impl TaskInner {
    fn switch_to_state(&mut self, target_state: TaskState, triggering_evt: TriggeringEvent) {
        let res = Ok(TransitionOk::Complete {
            origin: self.state,
            target: target_state,
        });

        self.state = target_state;
        triggering_evt.send_ack(res);
    }

    fn switch_to_err(&mut self, triggering_evt: TriggeringEvent) {
        let res = Err(TransitionError {
            trigger: triggering_evt.trigger,
            state: self.state,
            err_msg: gst::error_msg!(
                gst::CoreError::StateChange,
                [
                    "Unrecoverable error for {:?} from state {:?}",
                    triggering_evt,
                    self.state,
                ]
            ),
        });

        self.state = TaskState::Error;
        triggering_evt.send_ack(res);
    }

    fn skip_triggering_evt(&mut self, triggering_evt: TriggeringEvent) {
        let res = Ok(TransitionOk::Skipped {
            trigger: triggering_evt.trigger,
            state: self.state,
        });

        triggering_evt.send_ack(res);
    }

    fn trigger(&mut self, trigger: Trigger) -> Result<AckReceiver, TransitionError> {
        self.state_machine_handle
            .as_mut()
            .map(|state_machine| state_machine.trigger(trigger))
            .ok_or_else(|| {
                gst::warning!(
                    RUNTIME_CAT,
                    "Unable to send {:?}: no state machine",
                    trigger
                );
                TransitionError {
                    trigger,
                    state: TaskState::Unprepared,
                    err_msg: gst::error_msg!(
                        gst::ResourceError::NotFound,
                        ["Unable to send {:?}: no state machine", trigger]
                    ),
                }
            })
    }

    fn abort_task_loop(&mut self) {
        if let Some(loop_abort_handle) = self.loop_abort_handle.take() {
            loop_abort_handle.abort();
        }
    }
}

impl Drop for TaskInner {
    fn drop(&mut self) {
        if self.state != TaskState::Unprepared {
            // Don't panic here: in case another panic occurs, we would get
            // "panicked while panicking" which would prevents developers
            // from getting the initial panic message.
            gst::fixme!(RUNTIME_CAT, "Missing call to `Task::unprepare`");
        }
    }
}

/// An RAII implementation of scoped locked `TaskState`.
pub struct TaskStateGuard<'guard>(MutexGuard<'guard, TaskInner>);

impl Deref for TaskStateGuard<'_> {
    type Target = TaskState;

    fn deref(&self) -> &Self::Target {
        &(self.0).state
    }
}

/// A `Task` operating on a `threadshare` [`Context`].
///
/// [`Context`]: ../executor/struct.Context.html
#[derive(Debug, Clone)]
pub struct Task(Arc<Mutex<TaskInner>>);

impl Default for Task {
    fn default() -> Self {
        Task(Arc::new(Mutex::new(TaskInner::default())))
    }
}

impl Task {
    pub fn state(&self) -> TaskState {
        self.0.lock().unwrap().state
    }

    pub fn lock_state(&self) -> TaskStateGuard<'_> {
        TaskStateGuard(self.0.lock().unwrap())
    }

    pub fn prepare(&self, task_impl: impl TaskImpl, context: Context) -> TransitionStatus {
        let mut inner = self.0.lock().unwrap();

        let origin = inner.state;
        match origin {
            TaskState::Unprepared => (),
            TaskState::Prepared | TaskState::Preparing => {
                gst::debug!(RUNTIME_CAT, "Task already {:?}", origin);
                return TransitionOk::Skipped {
                    trigger: Trigger::Prepare,
                    state: origin,
                }
                .into();
            }
            state => {
                gst::warning!(RUNTIME_CAT, "Attempt to prepare Task in state {:?}", state);
                return TransitionError {
                    trigger: Trigger::Prepare,
                    state: inner.state,
                    err_msg: gst::error_msg!(
                        gst::CoreError::StateChange,
                        ["Attempt to prepare Task in state {:?}", state]
                    ),
                }
                .into();
            }
        }

        assert!(inner.state_machine_handle.is_none());

        inner.state = TaskState::Preparing;

        gst::log!(RUNTIME_CAT, "Spawning task state machine");
        inner.state_machine_handle = Some(StateMachine::spawn(
            self.0.clone(),
            Box::new(task_impl),
            context,
        ));

        let ack_rx = match inner.trigger(Trigger::Prepare) {
            Ok(ack_rx) => ack_rx,
            Err(err) => return err.into(),
        };
        drop(inner);

        TransitionStatus::Pending {
            trigger: Trigger::Prepare,
            origin: TaskState::Unprepared,
            res_fut: Box::pin(ack_rx.map(Result::unwrap)),
        }
    }

    pub fn unprepare(&self) -> TransitionStatus {
        let mut inner = self.0.lock().unwrap();

        let origin = inner.state;
        let mut state_machine_handle = match origin {
            TaskState::Stopped
            | TaskState::Error
            | TaskState::Prepared
            | TaskState::Preparing
            | TaskState::Unprepared => match inner.state_machine_handle.take() {
                Some(state_machine_handle) => {
                    gst::debug!(RUNTIME_CAT, "Unpreparing task");

                    state_machine_handle
                }
                None => {
                    gst::debug!(RUNTIME_CAT, "Task already unpreparing");
                    return TransitionOk::Skipped {
                        trigger: Trigger::Unprepare,
                        state: origin,
                    }
                    .into();
                }
            },
            state => {
                gst::warning!(
                    RUNTIME_CAT,
                    "Attempt to unprepare Task in state {:?}",
                    state
                );
                return TransitionError {
                    trigger: Trigger::Unprepare,
                    state: inner.state,
                    err_msg: gst::error_msg!(
                        gst::CoreError::StateChange,
                        ["Attempt to unprepare Task in state {:?}", state]
                    ),
                }
                .into();
            }
        };

        inner.abort_task_loop();
        let ack_rx = state_machine_handle.trigger(Trigger::Unprepare);
        drop(inner);

        let state_machine_end_fut = async {
            state_machine_handle.join().await;
            ack_rx.await.unwrap()
        };

        TransitionStatus::Pending {
            trigger: Trigger::Unprepare,
            origin,
            res_fut: Box::pin(state_machine_end_fut),
        }
    }

    /// Starts the `Task`.
    ///
    /// The execution occurs on the `Task` context.
    pub fn start(&self) -> TransitionStatus {
        let mut inner = self.0.lock().unwrap();

        let ack_rx = match inner.trigger(Trigger::Start) {
            Ok(ack_rx) => ack_rx,
            Err(err) => return err.into(),
        };

        if let TaskState::Started = inner.state {
            return TransitionOk::Skipped {
                trigger: Trigger::Start,
                state: TaskState::Started,
            }
            .into();
        }

        let origin = inner.state;
        drop(inner);

        TransitionStatus::Pending {
            trigger: Trigger::Start,
            origin,
            res_fut: Box::pin(ack_rx.map(Result::unwrap)),
        }
    }

    /// Requests the `Task` loop to pause.
    ///
    /// If an iteration is in progress, it will run to completion,
    /// then no more iteration will be executed before `start` is called again.
    /// Therefore, it is not guaranteed that `Paused` is reached when `pause` returns.
    pub fn pause(&self) -> TransitionStatus {
        let mut inner = self.0.lock().unwrap();

        let ack_rx = match inner.trigger(Trigger::Pause) {
            Ok(ack_rx) => ack_rx,
            Err(err) => return err.into(),
        };

        if let TaskState::Started = inner.state {
            // FIXME this could be async when iterate is split into next_item / handle_item
            return TransitionOk::NotWaiting {
                trigger: Trigger::Pause,
                origin: TaskState::Started,
            }
            .into();
        }

        let origin = inner.state;
        drop(inner);

        TransitionStatus::Pending {
            trigger: Trigger::Pause,
            origin,
            res_fut: Box::pin(ack_rx.map(Result::unwrap)),
        }
    }

    pub fn flush_start(&self) -> TransitionStatus {
        self.abort_push_await(Trigger::FlushStart)
    }

    pub fn flush_stop(&self) -> TransitionStatus {
        self.abort_push_await(Trigger::FlushStop)
    }

    /// Stops the `Started` `Task` and wait for it to finish.
    pub fn stop(&self) -> TransitionStatus {
        self.abort_push_await(Trigger::Stop)
    }

    /// Pushes a [`Trigger`] which requires the iteration loop to abort ASAP.
    ///
    /// This function:
    /// - Aborts the iteration loop aborts.
    /// - Pushes the provided [`Trigger`].
    /// - Awaits for the expected transition as usual.
    fn abort_push_await(&self, trigger: Trigger) -> TransitionStatus {
        let mut inner = self.0.lock().unwrap();

        inner.abort_task_loop();
        let ack_rx = match inner.trigger(trigger) {
            Ok(ack_rx) => ack_rx,
            Err(err) => return err.into(),
        };

        let origin = inner.state;
        drop(inner);

        TransitionStatus::Pending {
            trigger,
            origin,
            res_fut: Box::pin(ack_rx.map(Result::unwrap)),
        }
    }
}

struct StateMachine {
    task_impl: Box<dyn TaskImpl>,
    triggering_evt_rx: async_mpsc::Receiver<TriggeringEvent>,
    pending_triggering_evt: Option<TriggeringEvent>,
}

// Make sure the Context doesn't throttle otherwise we end up  with long delays executing
// transition actions in a pipeline with many elements. This is because pipeline serializes
// the transition actions and the Context's scheduler gets a chance to reach its throttling
// state between 2 elements.

macro_rules! exec_action {
    ($self:ident, $action:ident, $triggering_evt:expr, $origin:expr, $task_inner:expr) => {{
        match $self.task_impl.$action().await {
            Ok(()) => {
                let mut res;
                while Context::current_has_sub_tasks() {
                    gst::trace!(RUNTIME_CAT, "Draining subtasks for {}", stringify!($action));
                    res = Context::drain_sub_tasks().await.map_err(|err| {
                        let msg = format!("{} subtask returned {:?}", stringify!($action), err);
                        gst::log!(RUNTIME_CAT, "{}", &msg);
                        gst::error_msg!(gst::CoreError::StateChange, ["{}", &msg])
                    });

                    if res.is_err() {
                        break;
                    }
                }

                Ok($triggering_evt)
            }
            Err(err) => {
                // FIXME problem is that we loose the origin trigger in the
                //       final TransitionStatus.

                let next_trigger = $self
                    .task_impl
                    .handle_action_error($triggering_evt.trigger, $origin, err)
                    .await;

                // Convert triggering event according to the error handler's decision
                gst::trace!(
                    RUNTIME_CAT,
                    "TaskImpl transition action error: converting {:?} to {:?}",
                    $triggering_evt.trigger,
                    next_trigger,
                );

                $triggering_evt.trigger = next_trigger;
                $self.pending_triggering_evt = Some($triggering_evt);

                Err(())
            }
        }
    }};
}

impl StateMachine {
    // Use dynamic dispatch for TaskImpl as it reduces memory usage compared to monomorphization
    // without inducing any significant performance penalties.
    fn spawn(
        task_inner: Arc<Mutex<TaskInner>>,
        task_impl: Box<dyn TaskImpl>,
        context: Context,
    ) -> StateMachineHandle {
        let (triggering_evt_tx, triggering_evt_rx) = async_mpsc::channel(4);

        let state_machine = StateMachine {
            task_impl,
            triggering_evt_rx,
            pending_triggering_evt: None,
        };

        StateMachineHandle {
            join_handle: context.spawn_and_unpark(state_machine.run(task_inner)),
            triggering_evt_tx,
            context,
        }
    }

    async fn run(mut self, task_inner: Arc<Mutex<TaskInner>>) {
        let context = Context::current().expect("must be spawed on a Context");

        let mut triggering_evt = self
            .triggering_evt_rx
            .next()
            .await
            .expect("triggering_evt_rx dropped");

        if let Trigger::Prepare = triggering_evt.trigger {
            gst::trace!(RUNTIME_CAT, "Preparing task");

            let res = exec_action!(
                self,
                prepare,
                triggering_evt,
                TaskState::Unprepared,
                &task_inner
            );
            if let Ok(triggering_evt) = res {
                let mut task_inner = task_inner.lock().unwrap();
                let res = Ok(TransitionOk::Complete {
                    origin: TaskState::Unprepared,
                    target: TaskState::Prepared,
                });

                task_inner.state = TaskState::Prepared;
                triggering_evt.send_ack(res);

                gst::trace!(RUNTIME_CAT, "Task Prepared");
            }
        } else {
            panic!("Unexpected initial trigger {:?}", triggering_evt.trigger);
        }

        loop {
            triggering_evt = match self.pending_triggering_evt.take() {
                Some(pending_triggering_evt) => pending_triggering_evt,
                None => self
                    .triggering_evt_rx
                    .next()
                    .await
                    .expect("triggering_evt_rx dropped"),
            };

            gst::trace!(RUNTIME_CAT, "State machine popped {:?}", triggering_evt);

            match triggering_evt.trigger {
                Trigger::Error => {
                    let mut task_inner = task_inner.lock().unwrap();
                    task_inner.switch_to_err(triggering_evt);
                    gst::trace!(RUNTIME_CAT, "Switched to Error");
                }
                Trigger::Start => {
                    let origin = {
                        let mut task_inner = task_inner.lock().unwrap();
                        let origin = task_inner.state;
                        match origin {
                            TaskState::Stopped | TaskState::Paused | TaskState::Prepared => (),
                            TaskState::PausedFlushing => {
                                task_inner.switch_to_state(TaskState::Flushing, triggering_evt);
                                gst::trace!(
                                    RUNTIME_CAT,
                                    "Switched from PausedFlushing to Flushing"
                                );
                                continue;
                            }
                            TaskState::Error => {
                                triggering_evt.send_err_ack();
                                continue;
                            }
                            state => {
                                task_inner.skip_triggering_evt(triggering_evt);
                                gst::trace!(RUNTIME_CAT, "Skipped Start in state {:?}", state);
                                continue;
                            }
                        }

                        origin
                    };

                    self = Self::start(self, triggering_evt, origin, &task_inner, &context).await;
                    // next/pending triggering event handled in next iteration
                }
                Trigger::Pause => {
                    let (origin, target) = {
                        let mut task_inner = task_inner.lock().unwrap();
                        let origin = task_inner.state;
                        match origin {
                            TaskState::Started | TaskState::Stopped | TaskState::Prepared => {
                                (origin, TaskState::Paused)
                            }
                            TaskState::Flushing => (origin, TaskState::PausedFlushing),
                            TaskState::Error => {
                                triggering_evt.send_err_ack();
                                continue;
                            }
                            state => {
                                task_inner.skip_triggering_evt(triggering_evt);
                                gst::trace!(RUNTIME_CAT, "Skipped Pause in state {:?}", state);
                                continue;
                            }
                        }
                    };

                    let res = exec_action!(self, pause, triggering_evt, origin, &task_inner);
                    if let Ok(triggering_evt) = res {
                        task_inner
                            .lock()
                            .unwrap()
                            .switch_to_state(target, triggering_evt);
                        gst::trace!(RUNTIME_CAT, "Task loop {:?}", target);
                    }
                }
                Trigger::Stop => {
                    let origin = {
                        let mut task_inner = task_inner.lock().unwrap();
                        let origin = task_inner.state;
                        match origin {
                            TaskState::Started
                            | TaskState::Paused
                            | TaskState::PausedFlushing
                            | TaskState::Flushing => origin,
                            TaskState::Error => {
                                triggering_evt.send_err_ack();
                                continue;
                            }
                            state => {
                                task_inner.skip_triggering_evt(triggering_evt);
                                gst::trace!(RUNTIME_CAT, "Skipped Stop in state {:?}", state);
                                continue;
                            }
                        }
                    };

                    let res = exec_action!(self, stop, triggering_evt, origin, &task_inner);
                    if let Ok(triggering_evt) = res {
                        task_inner
                            .lock()
                            .unwrap()
                            .switch_to_state(TaskState::Stopped, triggering_evt);
                        gst::trace!(RUNTIME_CAT, "Task loop Stopped");
                    }
                }
                Trigger::FlushStart => {
                    let (origin, target) = {
                        let mut task_inner = task_inner.lock().unwrap();
                        let origin = task_inner.state;
                        match origin {
                            TaskState::Started => (origin, TaskState::Flushing),
                            TaskState::Paused => (origin, TaskState::PausedFlushing),
                            TaskState::Error => {
                                triggering_evt.send_err_ack();
                                continue;
                            }
                            state => {
                                task_inner.skip_triggering_evt(triggering_evt);
                                gst::trace!(RUNTIME_CAT, "Skipped FlushStart in state {:?}", state);
                                continue;
                            }
                        }
                    };

                    let res = exec_action!(self, flush_start, triggering_evt, origin, &task_inner);
                    if let Ok(triggering_evt) = res {
                        task_inner
                            .lock()
                            .unwrap()
                            .switch_to_state(target, triggering_evt);
                        gst::trace!(RUNTIME_CAT, "Task {:?}", target);
                    }
                }
                Trigger::FlushStop => {
                    let origin = task_inner.lock().unwrap().state;
                    let is_paused = match origin {
                        TaskState::Flushing => false,
                        TaskState::PausedFlushing => true,
                        TaskState::Error => {
                            triggering_evt.send_err_ack();
                            continue;
                        }
                        state => {
                            task_inner
                                .lock()
                                .unwrap()
                                .skip_triggering_evt(triggering_evt);
                            gst::trace!(RUNTIME_CAT, "Skipped FlushStop in state {:?}", state);
                            continue;
                        }
                    };

                    let res = exec_action!(self, flush_stop, triggering_evt, origin, &task_inner);
                    if let Ok(triggering_evt) = res {
                        if is_paused {
                            task_inner
                                .lock()
                                .unwrap()
                                .switch_to_state(TaskState::Paused, triggering_evt);
                            gst::trace!(RUNTIME_CAT, "Switched from PausedFlushing to Paused");
                        } else {
                            self = Self::start(self, triggering_evt, origin, &task_inner, &context)
                                .await;
                            // next/pending triggering event handled in next iteration
                        }
                    }
                }
                Trigger::Unprepare => {
                    // Unprepare is not joined by an ack_rx but by joining the state machine handle
                    self.task_impl.unprepare().await;

                    while Context::current_has_sub_tasks() {
                        gst::trace!(RUNTIME_CAT, "Draining subtasks for unprepare");
                        let res = Context::drain_sub_tasks().await.map_err(|err| {
                            gst::log!(RUNTIME_CAT, "unprepare subtask returned {:?}", err);
                            err
                        });
                        if res.is_err() {
                            break;
                        }
                    }

                    task_inner
                        .lock()
                        .unwrap()
                        .switch_to_state(TaskState::Unprepared, triggering_evt);

                    break;
                }
                _ => unreachable!("State machine handler {:?}", triggering_evt),
            }
        }

        gst::trace!(RUNTIME_CAT, "Task state machine terminated");
    }

    async fn start(
        mut self,
        mut triggering_evt: TriggeringEvent,
        origin: TaskState,
        task_inner: &Arc<Mutex<TaskInner>>,
        context: &Context,
    ) -> Self {
        let triggering_evt = match exec_action!(self, start, triggering_evt, origin, &task_inner) {
            Ok(triggering_evt) => triggering_evt,
            Err(_) => return self,
        };

        let task_inner_cl = Arc::clone(task_inner);
        let loop_fut = async move {
            let (abortable_task_loop, loop_abort_handle) =
                abortable(self.run_loop(Arc::clone(&task_inner_cl)));

            {
                let mut task_inner = task_inner_cl.lock().unwrap();
                task_inner.loop_abort_handle = Some(loop_abort_handle);
                task_inner.switch_to_state(TaskState::Started, triggering_evt);

                gst::trace!(RUNTIME_CAT, "Starting task loop");
            }

            match abortable_task_loop.await {
                Ok(Ok(())) => (),
                Ok(Err(err)) => {
                    let next_trigger = self.task_impl.handle_iterate_error(err).await;
                    let (triggering_evt, _) = TriggeringEvent::new(next_trigger);
                    self.pending_triggering_evt = Some(triggering_evt);
                }
                Err(Aborted) => gst::trace!(RUNTIME_CAT, "Task loop aborted"),
            }

            self
        };

        context.spawn_and_unpark(loop_fut).await.unwrap()
    }

    async fn run_loop(&mut self, task_inner: Arc<Mutex<TaskInner>>) -> Result<(), gst::FlowError> {
        gst::trace!(RUNTIME_CAT, "Task loop started");

        loop {
            while let Ok(Some(triggering_evt)) = self.triggering_evt_rx.try_next() {
                gst::trace!(RUNTIME_CAT, "Task loop popped {:?}", triggering_evt);

                match triggering_evt.trigger {
                    Trigger::Start => {
                        task_inner
                            .lock()
                            .unwrap()
                            .skip_triggering_evt(triggering_evt);
                        gst::trace!(RUNTIME_CAT, "Skipped Start in state Started");
                    }
                    _ => {
                        gst::trace!(
                            RUNTIME_CAT,
                            "Task loop handing {:?} to state machine",
                            triggering_evt,
                        );
                        self.pending_triggering_evt = Some(triggering_evt);
                        return Ok(());
                    }
                }
            }

            // Run the iteration function
            self.task_impl.iterate().await.map_err(|err| {
                gst::log!(RUNTIME_CAT, "Task loop iterate impl returned {:?}", err);
                err
            })?;

            while Context::current_has_sub_tasks() {
                gst::trace!(RUNTIME_CAT, "Draining subtasks for {}", stringify!($action));
                Context::drain_sub_tasks().await.map_err(|err| {
                    gst::log!(RUNTIME_CAT, "Task loop iterate subtask returned {:?}", err);
                    err
                })?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::{mpsc, oneshot};
    use futures::executor::block_on;
    use std::time::Duration;

    use super::{TaskState::*, TransitionOk::*, TransitionStatus::*, Trigger::*, *};
    use crate::runtime::Context;

    #[track_caller]
    fn stop_then_unprepare(task: Task) {
        task.stop().block_on().unwrap();
        task.unprepare().block_on().unwrap();
    }

    #[test]
    fn iterate() {
        gst::init().unwrap();

        struct TaskTest {
            prepared_sender: mpsc::Sender<()>,
            started_sender: mpsc::Sender<()>,
            iterate_sender: mpsc::Sender<()>,
            complete_iterate_receiver: mpsc::Receiver<Result<(), gst::FlowError>>,
            paused_sender: mpsc::Sender<()>,
            stopped_sender: mpsc::Sender<()>,
            flush_start_sender: mpsc::Sender<()>,
            unprepared_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskTest {
            fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "iterate: prepared");
                    self.prepared_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "iterate: started");
                    self.started_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "iterate: entering iterate");
                    self.iterate_sender.send(()).await.unwrap();

                    gst::debug!(RUNTIME_CAT, "iterate: awaiting complete_iterate_receiver");

                    let res = self.complete_iterate_receiver.next().await.unwrap();
                    if res.is_ok() {
                        gst::debug!(RUNTIME_CAT, "iterate: received Ok => keep looping");
                    } else {
                        gst::debug!(
                            RUNTIME_CAT,
                            "iterate: received {:?} => cancelling loop",
                            res
                        );
                    }

                    res
                }
                .boxed()
            }

            fn pause(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "iterate: paused");
                    self.paused_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "iterate: stopped");
                    self.stopped_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "iterate: stopped");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn unprepare(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst::debug!(RUNTIME_CAT, "iterate: unprepared");
                    self.unprepared_sender.send(()).await.unwrap();
                }
                .boxed()
            }
        }

        let context = Context::acquire("iterate", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        assert_eq!(task.state(), Unprepared);

        gst::debug!(RUNTIME_CAT, "iterate: preparing");

        let (prepared_sender, mut prepared_receiver) = mpsc::channel(1);
        let (started_sender, mut started_receiver) = mpsc::channel(1);
        let (iterate_sender, mut iterate_receiver) = mpsc::channel(1);
        let (mut complete_iterate_sender, complete_iterate_receiver) = mpsc::channel(1);
        let (paused_sender, mut paused_receiver) = mpsc::channel(1);
        let (stopped_sender, mut stopped_receiver) = mpsc::channel(1);
        let (flush_start_sender, mut flush_start_receiver) = mpsc::channel(1);
        let (unprepared_sender, mut unprepared_receiver) = mpsc::channel(1);
        let prepare_status = task.prepare(
            TaskTest {
                prepared_sender,
                started_sender,
                iterate_sender,
                complete_iterate_receiver,
                paused_sender,
                stopped_sender,
                flush_start_sender,
                unprepared_sender,
            },
            context,
        );

        assert!(prepare_status.is_pending());
        match prepare_status {
            Pending {
                trigger: Prepare,
                origin: Unprepared,
                ..
            } => (),
            other => panic!("{:?}", other),
        };

        gst::debug!(RUNTIME_CAT, "iterate: starting (async prepare)");
        // also tests await_maybe_on_context
        assert_eq!(
            task.start().await_maybe_on_context().unwrap(),
            Complete {
                origin: Prepared,
                target: Started,
            }
        );
        assert_eq!(task.state(), Started);

        // At this point, preparation must be complete
        // also tests await_maybe_on_context
        assert_eq!(
            prepare_status.await_maybe_on_context().unwrap(),
            Complete {
                origin: Unprepared,
                target: Prepared,
            },
        );
        block_on(prepared_receiver.next()).unwrap();
        // ... and start executed
        block_on(started_receiver.next()).unwrap();

        assert_eq!(task.state(), Started);

        // unlock task loop and keep looping
        block_on(iterate_receiver.next()).unwrap();
        block_on(complete_iterate_sender.send(Ok(()))).unwrap();

        gst::debug!(RUNTIME_CAT, "iterate: starting (redundant)");
        // already started
        assert_eq!(
            task.start().block_on().unwrap(),
            Skipped {
                trigger: Start,
                state: Started,
            },
        );
        assert_eq!(task.state(), Started);

        // Attempt to prepare Task in state Started (also tests check)
        match task.unprepare().check().unwrap_err() {
            TransitionError {
                trigger: Unprepare,
                state: Started,
                ..
            } => (),
            other => panic!("{:?}", other),
        }

        gst::debug!(RUNTIME_CAT, "iterate: pause (initial)");
        let pause_status = task.pause();
        assert!(pause_status.is_ready());
        // also tests `check`
        match pause_status.check().unwrap() {
            Ready(Ok(NotWaiting {
                trigger: Pause,
                origin: Started,
            })) => (),
            other => panic!("{:?}", other),
        }

        // Pause transition is asynchronous FIXME
        while TaskState::Paused != task.state() {
            std::thread::sleep(Duration::from_millis(2));

            if let Ok(Some(())) = iterate_receiver.try_next() {
                // unlock iteration
                block_on(complete_iterate_sender.send(Ok(()))).unwrap();
            }
        }

        gst::debug!(RUNTIME_CAT, "iterate: awaiting pause ack");
        block_on(paused_receiver.next()).unwrap();

        gst::debug!(RUNTIME_CAT, "iterate: starting (after pause)");
        assert_eq!(
            task.start().block_on().unwrap(),
            Complete {
                origin: Paused,
                target: Started,
            },
        );

        assert_eq!(task.state(), Started);
        // Paused -> Started
        let _ = block_on(started_receiver.next());

        gst::debug!(RUNTIME_CAT, "iterate: stopping");
        assert_eq!(
            task.stop().block_on().unwrap(),
            Complete {
                origin: Started,
                target: Stopped,
            },
        );

        assert_eq!(task.state(), Stopped);
        let _ = block_on(stopped_receiver.next());

        // purge remaining iteration received before stop if any
        let _ = iterate_receiver.try_next();

        gst::debug!(RUNTIME_CAT, "iterate: starting (after stop)");
        assert_eq!(
            task.start().block_on().unwrap(),
            Complete {
                origin: Stopped,
                target: Started,
            },
        );
        let _ = block_on(started_receiver.next());

        gst::debug!(RUNTIME_CAT, "iterate: req. iterate to return Eos");
        block_on(iterate_receiver.next()).unwrap();
        block_on(complete_iterate_sender.send(Err(gst::FlowError::Eos))).unwrap();

        gst::debug!(RUNTIME_CAT, "iterate: awaiting stop ack");
        block_on(stopped_receiver.next()).unwrap();

        // Wait for state machine to reach Stopped
        while TaskState::Stopped != task.state() {
            std::thread::sleep(Duration::from_millis(2));
        }

        gst::debug!(RUNTIME_CAT, "iterate: starting (after stop)");
        assert_eq!(
            task.start().block_on().unwrap(),
            Complete {
                origin: Stopped,
                target: Started,
            },
        );
        let _ = block_on(started_receiver.next());

        gst::debug!(RUNTIME_CAT, "iterate: req. iterate to return Flushing");
        block_on(iterate_receiver.next()).unwrap();
        block_on(complete_iterate_sender.send(Err(gst::FlowError::Flushing))).unwrap();

        gst::debug!(RUNTIME_CAT, "iterate: awaiting flush_start ack");
        block_on(flush_start_receiver.next()).unwrap();

        // Wait for state machine to reach Flushing
        while TaskState::Flushing != task.state() {
            std::thread::sleep(Duration::from_millis(2));
        }

        gst::debug!(RUNTIME_CAT, "iterate: stop flushing");
        assert_eq!(
            task.flush_stop().block_on().unwrap(),
            Complete {
                origin: Flushing,
                target: Started,
            },
        );
        let _ = block_on(started_receiver.next());

        gst::debug!(RUNTIME_CAT, "iterate: req. iterate to return Error");
        block_on(iterate_receiver.next()).unwrap();
        block_on(complete_iterate_sender.send(Err(gst::FlowError::Error))).unwrap();

        // Wait for state machine to reach Error
        while TaskState::Error != task.state() {
            std::thread::sleep(Duration::from_millis(2));
        }

        gst::debug!(RUNTIME_CAT, "iterate: attempting to start (after Error)");
        match task.start().block_on().unwrap_err() {
            TransitionError {
                trigger: Start,
                state: TaskState::Error,
                ..
            } => (),
            other => panic!("{:?}", other),
        }

        assert_eq!(
            task.unprepare().block_on().unwrap(),
            Complete {
                origin: TaskState::Error,
                target: Unprepared,
            },
        );

        assert_eq!(task.state(), Unprepared);
        let _ = block_on(unprepared_receiver.next());
    }

    #[test]
    fn prepare_error() {
        gst::init().unwrap();

        struct TaskPrepareTest {
            prepare_error_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskPrepareTest {
            fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "prepare_error: prepare returning an error");
                    Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["prepare_error: intentional error"]
                    ))
                }
                .boxed()
            }

            fn handle_action_error(
                &mut self,
                trigger: Trigger,
                state: TaskState,
                err: gst::ErrorMessage,
            ) -> BoxFuture<'_, Trigger> {
                async move {
                    gst::debug!(
                        RUNTIME_CAT,
                        "prepare_error: handling prepare error {:?}",
                        err
                    );
                    match (trigger, state) {
                        (Trigger::Prepare, TaskState::Unprepared) => {
                            self.prepare_error_sender.send(()).await.unwrap();
                        }
                        other => unreachable!("{:?}", other),
                    }
                    Trigger::Error
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::ok(()).boxed()
            }
        }

        let context = Context::acquire("prepare_error", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        assert_eq!(task.state(), Unprepared);

        let (prepare_error_sender, mut prepare_error_receiver) = mpsc::channel(1);
        let prepare_status = task.prepare(
            TaskPrepareTest {
                prepare_error_sender,
            },
            context,
        );

        gst::debug!(
            RUNTIME_CAT,
            "prepare_error: await action error notification"
        );
        block_on(prepare_error_receiver.next()).unwrap();

        match prepare_status.block_on().unwrap_err() {
            TransitionError {
                trigger: Trigger::Error,
                state: Preparing,
                ..
            } => (),
            other => panic!("{:?}", other),
        }

        // Wait for state machine to reach Error
        while TaskState::Error != task.state() {
            std::thread::sleep(Duration::from_millis(2));
        }

        match task.start().block_on().unwrap_err() {
            TransitionError {
                trigger: Start,
                state: TaskState::Error,
                ..
            } => (),
            other => panic!("{:?}", other),
        }

        block_on(task.unprepare()).unwrap();
    }

    #[test]
    fn prepare_start_ok() {
        // Hold the preparation function so that it completes after the start request is engaged

        gst::init().unwrap();

        struct TaskPrepareTest {
            prepare_receiver: mpsc::Receiver<()>,
        }

        impl TaskImpl for TaskPrepareTest {
            fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(
                        RUNTIME_CAT,
                        "prepare_start_ok: preparation awaiting trigger"
                    );
                    self.prepare_receiver.next().await.unwrap();
                    gst::debug!(RUNTIME_CAT, "prepare_start_ok: preparation complete Ok");
                    Ok(())
                }
                .boxed()
            }

            fn handle_action_error(
                &mut self,
                _trigger: Trigger,
                _state: TaskState,
                _err: gst::ErrorMessage,
            ) -> BoxFuture<'_, Trigger> {
                unreachable!("prepare_start_ok: handle_prepare_error");
            }

            fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "prepare_start_ok: started");
                    Ok(())
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::pending::<Result<(), gst::FlowError>>().boxed()
            }
        }

        let context = Context::acquire("prepare_start_ok", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (mut prepare_sender, prepare_receiver) = mpsc::channel(1);
        let _ = task.prepare(TaskPrepareTest { prepare_receiver }, context);

        let start_ctx = Context::acquire("prepare_start_ok_requester", Duration::ZERO).unwrap();
        let (ready_sender, ready_receiver) = oneshot::channel();
        let start_handle = start_ctx.spawn(async move {
            assert_eq!(task.state(), Preparing);
            gst::debug!(RUNTIME_CAT, "prepare_start_ok: starting");
            let start_status = task.start();
            match start_status {
                Pending {
                    trigger: Start,
                    origin: Preparing,
                    ..
                } => (),
                other => panic!("{:?}", other),
            }
            ready_sender.send(()).unwrap();
            assert_eq!(
                start_status.await.unwrap(),
                Complete {
                    origin: Prepared,
                    target: Started,
                },
            );
            assert_eq!(task.state(), Started);

            let stop_status = task.stop();
            match stop_status {
                Pending {
                    trigger: Stop,
                    origin: Started,
                    ..
                } => (),
                other => panic!("{:?}", other),
            }
            assert_eq!(
                stop_status.await.unwrap(),
                Complete {
                    origin: Started,
                    target: Stopped,
                },
            );
            assert_eq!(task.state(), Stopped);

            let unprepare_status = task.unprepare();
            match unprepare_status {
                Pending {
                    trigger: Unprepare,
                    origin: Stopped,
                    ..
                } => (),
                other => panic!("{:?}", other),
            };
            assert_eq!(
                unprepare_status.await.unwrap(),
                Complete {
                    origin: Stopped,
                    target: Unprepared,
                },
            );
            assert_eq!(task.state(), Unprepared);
        });

        gst::debug!(RUNTIME_CAT, "prepare_start_ok: awaiting for start_ctx");
        block_on(ready_receiver).unwrap();

        gst::debug!(RUNTIME_CAT, "prepare_start_ok: triggering preparation");
        block_on(prepare_sender.send(())).unwrap();

        block_on(start_handle).unwrap();
    }

    #[test]
    fn prepare_start_error() {
        // Hold the preparation function so that it completes after the start request is engaged

        gst::init().unwrap();

        struct TaskPrepareTest {
            prepare_receiver: mpsc::Receiver<()>,
            prepare_error_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskPrepareTest {
            fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(
                        RUNTIME_CAT,
                        "prepare_start_error: preparation awaiting trigger"
                    );
                    self.prepare_receiver.next().await.unwrap();
                    gst::debug!(RUNTIME_CAT, "prepare_start_error: preparation complete Err");

                    Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["prepare_start_error: intentional error"]
                    ))
                }
                .boxed()
            }

            fn handle_action_error(
                &mut self,
                trigger: Trigger,
                state: TaskState,
                err: gst::ErrorMessage,
            ) -> BoxFuture<'_, Trigger> {
                async move {
                    gst::debug!(
                        RUNTIME_CAT,
                        "prepare_start_error: handling prepare error {:?}",
                        err
                    );
                    match (trigger, state) {
                        (Trigger::Prepare, TaskState::Unprepared) => {
                            self.prepare_error_sender.send(()).await.unwrap();
                        }
                        other => panic!("action error for {:?}", other),
                    }
                    Trigger::Error
                }
                .boxed()
            }

            fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                unreachable!("prepare_start_error: start");
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                unreachable!("prepare_start_error: iterate");
            }
        }

        let context = Context::acquire("prepare_start_error", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (mut prepare_sender, prepare_receiver) = mpsc::channel(1);
        let (prepare_error_sender, mut prepare_error_receiver) = mpsc::channel(1);
        let prepare_status = task.prepare(
            TaskPrepareTest {
                prepare_receiver,
                prepare_error_sender,
            },
            context,
        );
        match prepare_status {
            Pending {
                trigger: Prepare,
                origin: Unprepared,
                ..
            } => (),
            other => panic!("{:?}", other),
        };

        let start_ctx = Context::acquire("prepare_start_error_requester", Duration::ZERO).unwrap();
        let (ready_sender, ready_receiver) = oneshot::channel();
        let start_handle = start_ctx.spawn(async move {
            gst::debug!(RUNTIME_CAT, "prepare_start_error: starting (Err)");
            let _ = task.start();
            ready_sender.send(()).unwrap();
            // FIXME we loose the origin Trigger (Start)
            // and only get the Trigger returned by handle_action_error
            // see also: comment in exec_action!
            match prepare_status.await {
                Err(TransitionError {
                    trigger: Trigger::Error,
                    state: Preparing,
                    ..
                }) => (),
                other => panic!("{:?}", other),
            }

            let unprepare_status = task.unprepare();
            match unprepare_status {
                Pending {
                    trigger: Unprepare,
                    origin: TaskState::Error,
                    ..
                } => (),
                other => panic!("{:?}", other),
            };
            assert_eq!(
                unprepare_status.await.unwrap(),
                Complete {
                    origin: TaskState::Error,
                    target: Unprepared,
                },
            );
        });

        gst::debug!(RUNTIME_CAT, "prepare_start_error: awaiting for start_ctx");
        block_on(ready_receiver).unwrap();

        gst::debug!(
            RUNTIME_CAT,
            "prepare_start_error: triggering preparation (failure)"
        );
        block_on(prepare_sender.send(())).unwrap();

        gst::debug!(
            RUNTIME_CAT,
            "prepare_start_error: await prepare error notification"
        );
        block_on(prepare_error_receiver.next()).unwrap();

        block_on(start_handle).unwrap();
    }

    #[test]
    fn pause_start() {
        gst::init().unwrap();

        struct TaskPauseStartTest {
            iterate_sender: mpsc::Sender<()>,
            complete_receiver: mpsc::Receiver<()>,
            paused_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskPauseStartTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "pause_start: entering iteration");
                    self.iterate_sender.send(()).await.unwrap();

                    gst::debug!(RUNTIME_CAT, "pause_start: iteration awaiting completion");
                    self.complete_receiver.next().await.unwrap();
                    gst::debug!(RUNTIME_CAT, "pause_start: iteration complete");

                    Ok(())
                }
                .boxed()
            }

            fn pause(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "pause_start: paused");
                    self.paused_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("pause_start", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (iterate_sender, mut iterate_receiver) = mpsc::channel(1);
        let (mut complete_sender, complete_receiver) = mpsc::channel(0);
        let (paused_sender, mut paused_receiver) = mpsc::channel(1);
        let _ = task.prepare(
            TaskPauseStartTest {
                iterate_sender,
                complete_receiver,
                paused_sender,
            },
            context,
        );

        gst::debug!(RUNTIME_CAT, "pause_start: starting");
        assert_eq!(
            task.start().block_on().unwrap(),
            Complete {
                origin: Prepared,
                target: Started,
            },
        );
        assert_eq!(task.state(), Started);

        gst::debug!(RUNTIME_CAT, "pause_start: awaiting 1st iteration");
        block_on(iterate_receiver.next()).unwrap();

        gst::debug!(RUNTIME_CAT, "pause_start: pausing (1)");
        match task.pause() {
            Ready(Ok(NotWaiting {
                trigger: Pause,
                origin: Started,
            })) => (),
            other => panic!("{:?}", other),
        }

        gst::debug!(RUNTIME_CAT, "pause_start: sending 1st iteration completion");
        complete_sender.try_send(()).unwrap();

        // Pause transition is asynchronous FIXME
        while TaskState::Paused != task.state() {
            std::thread::sleep(Duration::from_millis(5));
        }

        gst::debug!(RUNTIME_CAT, "pause_start: awaiting paused");
        let _ = block_on(paused_receiver.next());

        // Loop held on due to Pause
        iterate_receiver.try_next().unwrap_err();
        assert_eq!(
            task.start().block_on().unwrap(),
            Complete {
                origin: Paused,
                target: Started,
            },
        );
        assert_eq!(task.state(), Started);

        gst::debug!(RUNTIME_CAT, "pause_start: awaiting 2d iteration");
        block_on(iterate_receiver.next()).unwrap();

        gst::debug!(RUNTIME_CAT, "pause_start: sending 2d iteration completion");
        complete_sender.try_send(()).unwrap();

        stop_then_unprepare(task);
    }

    #[test]
    fn successive_pause_start() {
        // Purpose: check pause cancellation.
        gst::init().unwrap();

        struct TaskPauseStartTest {
            iterate_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskPauseStartTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "successive_pause_start: iteration");
                    self.iterate_sender.send(()).await.unwrap();

                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("successive_pause_start", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (iterate_sender, mut iterate_receiver) = mpsc::channel(1);
        let _ = task.prepare(TaskPauseStartTest { iterate_sender }, context);

        gst::debug!(RUNTIME_CAT, "successive_pause_start: starting");
        block_on(task.start()).unwrap();

        gst::debug!(RUNTIME_CAT, "successive_pause_start: awaiting iteration 1");
        block_on(iterate_receiver.next()).unwrap();

        gst::debug!(RUNTIME_CAT, "successive_pause_start: pause and start");
        let _ = task.pause();
        block_on(task.start()).unwrap();

        assert_eq!(task.state(), Started);

        gst::debug!(RUNTIME_CAT, "successive_pause_start: awaiting iteration 2");
        block_on(iterate_receiver.next()).unwrap();

        gst::debug!(RUNTIME_CAT, "successive_pause_start: stopping");
        stop_then_unprepare(task);
    }

    #[test]
    fn flush_regular_sync() {
        gst::init().unwrap();

        struct TaskFlushTest {
            flush_start_sender: mpsc::Sender<()>,
            flush_stop_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskFlushTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::pending::<Result<(), gst::FlowError>>().boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "flush_regular_sync: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "flush_regular_sync: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("flush_regular_sync", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (flush_start_sender, mut flush_start_receiver) = mpsc::channel(1);
        let (flush_stop_sender, mut flush_stop_receiver) = mpsc::channel(1);
        let _ = task.prepare(
            TaskFlushTest {
                flush_start_sender,
                flush_stop_sender,
            },
            context,
        );

        gst::debug!(RUNTIME_CAT, "flush_regular_sync: start");
        block_on(task.start()).unwrap();

        gst::debug!(RUNTIME_CAT, "flush_regular_sync: starting flush");
        assert_eq!(
            task.flush_start().block_on().unwrap(),
            Complete {
                origin: Started,
                target: Flushing,
            },
        );
        assert_eq!(task.state(), Flushing);

        block_on(flush_start_receiver.next()).unwrap();

        gst::debug!(RUNTIME_CAT, "flush_regular_sync: stopping flush");
        assert_eq!(
            task.flush_stop().await_maybe_on_context().unwrap(),
            Complete {
                origin: Flushing,
                target: Started,
            },
        );
        assert_eq!(task.state(), Started);

        block_on(flush_stop_receiver.next()).unwrap();

        let _ = task.pause();
        stop_then_unprepare(task);
    }

    #[test]
    fn flush_regular_different_context() {
        // Purpose: make sure a flush sequence triggered from a Context doesn't block.
        gst::init().unwrap();

        struct TaskFlushTest {
            flush_start_sender: mpsc::Sender<()>,
            flush_stop_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskFlushTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::pending::<Result<(), gst::FlowError>>().boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(
                        RUNTIME_CAT,
                        "flush_regular_different_context: started flushing"
                    );
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(
                        RUNTIME_CAT,
                        "flush_regular_different_context: stopped flushing"
                    );
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context =
            Context::acquire("flush_regular_different_context", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (flush_start_sender, mut flush_start_receiver) = mpsc::channel(1);
        let (flush_stop_sender, mut flush_stop_receiver) = mpsc::channel(1);
        let _ = task.prepare(
            TaskFlushTest {
                flush_start_sender,
                flush_stop_sender,
            },
            context,
        );

        gst::debug!(RUNTIME_CAT, "flush_regular_different_context: start");
        task.start().block_on().unwrap();

        let oob_context = Context::acquire(
            "flush_regular_different_context_oob",
            Duration::from_millis(2),
        )
        .unwrap();

        let task_clone = task.clone();
        let flush_res_fut = oob_context.spawn(async move {
            let flush_start_status = task_clone.flush_start();
            match flush_start_status {
                Pending {
                    trigger: FlushStart,
                    origin: Started,
                    ..
                } => (),
                other => panic!("{:?}", other),
            };
            assert_eq!(
                flush_start_status.await.unwrap(),
                Complete {
                    origin: Started,
                    target: Flushing,
                },
            );
            assert_eq!(task_clone.state(), Flushing);
            flush_start_receiver.next().await.unwrap();

            let flush_stop_status = task_clone.flush_stop();
            match flush_stop_status {
                Pending {
                    trigger: FlushStop,
                    origin: Flushing,
                    ..
                } => (),
                other => panic!("{:?}", other),
            };
            assert_eq!(
                flush_stop_status.await_maybe_on_context().unwrap(),
                NotWaiting {
                    trigger: FlushStop,
                    origin: Flushing,
                },
            );

            Context::drain_sub_tasks().await.unwrap();
            assert_eq!(task_clone.state(), Started);
        });

        block_on(flush_res_fut).unwrap();
        block_on(flush_stop_receiver.next()).unwrap();

        stop_then_unprepare(task);
    }

    #[test]
    fn flush_regular_same_context() {
        // Purpose: make sure a flush sequence triggered from the same Context doesn't block.
        gst::init().unwrap();

        struct TaskFlushTest {
            flush_start_sender: mpsc::Sender<()>,
            flush_stop_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskFlushTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::pending::<Result<(), gst::FlowError>>().boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "flush_regular_same_context: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "flush_regular_same_context: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context =
            Context::acquire("flush_regular_same_context", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (flush_start_sender, mut flush_start_receiver) = mpsc::channel(1);
        let (flush_stop_sender, mut flush_stop_receiver) = mpsc::channel(1);
        let _ = task.prepare(
            TaskFlushTest {
                flush_start_sender,
                flush_stop_sender,
            },
            context.clone(),
        );

        block_on(task.start()).unwrap();

        let task_clone = task.clone();
        let flush_handle = context.spawn(async move {
            let flush_start_status = task_clone.flush_start();
            match flush_start_status {
                Pending {
                    trigger: FlushStart,
                    origin: Started,
                    ..
                } => (),
                other => panic!("{:?}", other),
            };
            assert_eq!(
                flush_start_status.await.unwrap(),
                Complete {
                    origin: Started,
                    target: Flushing,
                },
            );
            assert_eq!(task_clone.state(), Flushing);
            flush_start_receiver.next().await.unwrap();

            let flush_stop_status = task_clone.flush_stop();
            match flush_stop_status {
                Pending {
                    trigger: FlushStop,
                    origin: Flushing,
                    ..
                } => (),
                other => panic!("{:?}", other),
            };
            assert_eq!(
                flush_stop_status.await.unwrap(),
                Complete {
                    origin: Flushing,
                    target: Started,
                },
            );
            assert_eq!(task_clone.state(), Started);
        });

        block_on(flush_handle).unwrap();
        block_on(flush_stop_receiver.next()).unwrap();

        stop_then_unprepare(task);
    }

    #[test]
    fn flush_from_loop() {
        // Purpose: make sure a flush_start triggered from an iteration doesn't block.
        gst::init().unwrap();

        struct TaskFlushTest {
            task: Task,
            flush_start_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskFlushTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "flush_from_loop: flush_start from iteration");
                    let flush_status = self.task.flush_start();
                    match flush_status {
                        Pending {
                            trigger: FlushStart,
                            origin: Started,
                            ..
                        } => (),
                        other => panic!("{:?}", other),
                    }
                    flush_status.await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "flush_from_loop: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("flush_from_loop", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (flush_start_sender, mut flush_start_receiver) = mpsc::channel(1);
        let _ = task.prepare(
            TaskFlushTest {
                task: task.clone(),
                flush_start_sender,
            },
            context,
        );

        let _ = task.start();

        gst::debug!(
            RUNTIME_CAT,
            "flush_from_loop: awaiting flush_start notification"
        );
        block_on(flush_start_receiver.next()).unwrap();

        assert_eq!(
            task.stop().block_on().unwrap(),
            Complete {
                origin: Flushing,
                target: Stopped,
            },
        );
        task.unprepare().block_on().unwrap();
    }

    #[test]
    fn pause_from_loop() {
        // Purpose: make sure a start triggered from an iteration doesn't block.
        // E.g. an auto pause cancellation after a delay.
        gst::init().unwrap();

        struct TaskStartTest {
            task: Task,
            pause_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskStartTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "pause_from_loop: entering iteration");

                    crate::runtime::time::delay_for(Duration::from_millis(50)).await;

                    gst::debug!(RUNTIME_CAT, "pause_from_loop: pause from iteration");
                    match self.task.pause() {
                        Ready(Ok(TransitionOk::NotWaiting {
                            trigger: Pause,
                            origin: Started,
                        })) => (),
                        other => panic!("{:?}", other),
                    }

                    Ok(())
                }
                .boxed()
            }

            fn pause(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "pause_from_loop: entering pause action");
                    self.pause_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("pause_from_loop", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (pause_sender, mut pause_receiver) = mpsc::channel(1);
        let _ = task.prepare(
            TaskStartTest {
                task: task.clone(),
                pause_sender,
            },
            context,
        );

        let _ = task.start();

        gst::debug!(RUNTIME_CAT, "pause_from_loop: awaiting pause notification");
        block_on(pause_receiver.next()).unwrap();

        stop_then_unprepare(task);
    }

    #[test]
    fn trigger_from_action() {
        // Purpose: make sure an event triggered from a transition action doesn't block.
        gst::init().unwrap();

        struct TaskFlushTest {
            task: Task,
            flush_stop_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskFlushTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::pending::<Result<(), gst::FlowError>>().boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(
                        RUNTIME_CAT,
                        "trigger_from_action: flush_start triggering flush_stop"
                    );
                    match self.task.flush_stop() {
                        Pending {
                            trigger: FlushStop,
                            origin: Started,
                            ..
                        } => (),
                        other => panic!("{:?}", other),
                    }

                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "trigger_from_action: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("trigger_from_action", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (flush_stop_sender, mut flush_stop_receiver) = mpsc::channel(1);
        let _ = task.prepare(
            TaskFlushTest {
                task: task.clone(),
                flush_stop_sender,
            },
            context,
        );

        task.start().block_on().unwrap();
        let _ = task.flush_start();

        gst::debug!(
            RUNTIME_CAT,
            "trigger_from_action: awaiting flush_stop notification"
        );
        block_on(flush_stop_receiver.next()).unwrap();

        stop_then_unprepare(task);
    }

    #[test]
    fn pause_flush_start() {
        gst::init().unwrap();

        struct TaskFlushTest {
            started_sender: mpsc::Sender<()>,
            flush_start_sender: mpsc::Sender<()>,
            flush_stop_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskFlushTest {
            fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "pause_flush_start: started");
                    self.started_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::pending::<Result<(), gst::FlowError>>().boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "pause_flush_start: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "pause_flush_start: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("pause_flush_start", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (started_sender, mut started_receiver) = mpsc::channel(1);
        let (flush_start_sender, mut flush_start_receiver) = mpsc::channel(1);
        let (flush_stop_sender, mut flush_stop_receiver) = mpsc::channel(1);
        let _ = task.prepare(
            TaskFlushTest {
                started_sender,
                flush_start_sender,
                flush_stop_sender,
            },
            context,
        );

        // Pause, FlushStart, FlushStop, Start

        gst::debug!(RUNTIME_CAT, "pause_flush_start: pausing");
        assert_eq!(
            task.pause().block_on().unwrap(),
            Complete {
                origin: Prepared,
                target: Paused,
            },
        );

        gst::debug!(RUNTIME_CAT, "pause_flush_start: starting flush");
        assert_eq!(
            task.flush_start().block_on().unwrap(),
            Complete {
                origin: Paused,
                target: PausedFlushing,
            },
        );
        assert_eq!(task.state(), PausedFlushing);
        block_on(flush_start_receiver.next());

        gst::debug!(RUNTIME_CAT, "pause_flush_start: stopping flush");
        assert_eq!(
            task.flush_stop().block_on().unwrap(),
            Complete {
                origin: PausedFlushing,
                target: Paused,
            },
        );
        assert_eq!(task.state(), Paused);
        block_on(flush_stop_receiver.next());

        // start action not executed
        started_receiver.try_next().unwrap_err();

        gst::debug!(RUNTIME_CAT, "pause_flush_start: starting after flushing");
        assert_eq!(
            task.start().block_on().unwrap(),
            Complete {
                origin: Paused,
                target: Started,
            },
        );
        assert_eq!(task.state(), Started);
        block_on(started_receiver.next());

        stop_then_unprepare(task);
    }

    #[test]
    fn pause_flushing_start() {
        gst::init().unwrap();

        struct TaskFlushTest {
            started_sender: mpsc::Sender<()>,
            flush_start_sender: mpsc::Sender<()>,
            flush_stop_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskFlushTest {
            fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "pause_flushing_start: started");
                    self.started_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::pending::<Result<(), gst::FlowError>>().boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "pause_flushing_start: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "pause_flushing_start: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("pause_flushing_start", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (started_sender, mut started_receiver) = mpsc::channel(1);
        let (flush_start_sender, mut flush_start_receiver) = mpsc::channel(1);
        let (flush_stop_sender, mut flush_stop_receiver) = mpsc::channel(1);
        let _ = task.prepare(
            TaskFlushTest {
                started_sender,
                flush_start_sender,
                flush_stop_sender,
            },
            context,
        );

        // Pause, FlushStart, Start, FlushStop

        gst::debug!(RUNTIME_CAT, "pause_flushing_start: pausing");
        let _ = task.pause();

        gst::debug!(RUNTIME_CAT, "pause_flushing_start: starting flush");
        block_on(task.flush_start()).unwrap();
        assert_eq!(task.state(), PausedFlushing);
        block_on(flush_start_receiver.next());

        gst::debug!(RUNTIME_CAT, "pause_flushing_start: starting while flushing");
        assert_eq!(
            task.start().block_on().unwrap(),
            Complete {
                origin: PausedFlushing,
                target: Flushing,
            },
        );
        assert_eq!(task.state(), Flushing);

        // start action not executed
        started_receiver.try_next().unwrap_err();

        gst::debug!(RUNTIME_CAT, "pause_flushing_start: stopping flush");
        assert_eq!(
            task.flush_stop().block_on().unwrap(),
            Complete {
                origin: Flushing,
                target: Started,
            },
        );
        assert_eq!(task.state(), Started);
        block_on(flush_stop_receiver.next());
        block_on(started_receiver.next());

        stop_then_unprepare(task);
    }

    #[test]
    fn flush_concurrent_start() {
        // Purpose: check the racy case of start being triggered in // after flush_start
        // e.g.: a FlushStart event received on a Pad and the element starting after a Pause
        gst::init().unwrap();

        struct TaskStartTest {
            flush_start_sender: mpsc::Sender<()>,
            flush_stop_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskStartTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::pending::<Result<(), gst::FlowError>>().boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "flush_concurrent_start: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "flush_concurrent_start: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("flush_concurrent_start", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (flush_start_sender, mut flush_start_receiver) = mpsc::channel(1);
        let (flush_stop_sender, mut flush_stop_receiver) = mpsc::channel(1);
        let _ = task.prepare(
            TaskStartTest {
                flush_start_sender,
                flush_stop_sender,
            },
            context,
        );

        let oob_context =
            Context::acquire("flush_concurrent_start_oob", Duration::from_millis(2)).unwrap();
        let task_clone = task.clone();

        block_on(task.pause()).unwrap();

        // Launch flush_start // start
        let (ready_sender, ready_receiver) = oneshot::channel();
        gst::debug!(RUNTIME_CAT, "flush_concurrent_start: spawning flush_start");
        let flush_start_handle = oob_context.spawn(async move {
            gst::debug!(RUNTIME_CAT, "flush_concurrent_start: // flush_start");
            ready_sender.send(()).unwrap();
            let status = task_clone.flush_start();
            match status {
                Pending {
                    trigger: FlushStart,
                    origin: Paused,
                    ..
                } => (),
                Pending {
                    trigger: FlushStart,
                    origin: Started,
                    ..
                } => (),
                other => panic!("{:?}", other),
            };
            status.await.unwrap();
            flush_start_receiver.next().await.unwrap();
        });

        gst::debug!(
            RUNTIME_CAT,
            "flush_concurrent_start: awaiting for oob_context"
        );
        block_on(ready_receiver).unwrap();

        gst::debug!(RUNTIME_CAT, "flush_concurrent_start: // start");
        match block_on(task.start()) {
            Ok(TransitionOk::Complete {
                origin: Paused,
                target: Started,
            }) => (),
            Ok(TransitionOk::Complete {
                origin: PausedFlushing,
                target: Flushing,
            }) => (),
            other => panic!("{:?}", other),
        }

        block_on(flush_start_handle).unwrap();

        gst::debug!(RUNTIME_CAT, "flush_concurrent_start: requesting flush_stop");
        assert_eq!(
            task.flush_stop().block_on().unwrap(),
            Complete {
                origin: Flushing,
                target: Started,
            },
        );
        assert_eq!(task.state(), Started);
        block_on(flush_stop_receiver.next());

        stop_then_unprepare(task);
    }

    #[test]
    fn start_timer() {
        // Purpose: make sure a Timer initialized in a transition is
        // available when iterating in the loop.
        gst::init().unwrap();

        struct TaskTimerTest {
            timer: Option<crate::runtime::Timer>,
            timer_elapsed_sender: Option<oneshot::Sender<()>>,
        }

        impl TaskImpl for TaskTimerTest {
            fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    self.timer = Some(crate::runtime::time::delay_for(Duration::from_millis(50)));
                    gst::debug!(RUNTIME_CAT, "start_timer: started");
                    Ok(())
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "start_timer: awaiting timer");
                    self.timer.take().unwrap().await;
                    gst::debug!(RUNTIME_CAT, "start_timer: timer elapsed");

                    if let Some(timer_elapsed_sender) = self.timer_elapsed_sender.take() {
                        timer_elapsed_sender.send(()).unwrap();
                    }

                    Err(gst::FlowError::Eos)
                }
                .boxed()
            }
        }

        let context = Context::acquire("start_timer", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        let (timer_elapsed_sender, timer_elapsed_receiver) = oneshot::channel();
        let _ = task.prepare(
            TaskTimerTest {
                timer: None,
                timer_elapsed_sender: Some(timer_elapsed_sender),
            },
            context,
        );

        gst::debug!(RUNTIME_CAT, "start_timer: start");
        let _ = task.start();

        block_on(timer_elapsed_receiver).unwrap();
        gst::debug!(RUNTIME_CAT, "start_timer: timer elapsed received");

        stop_then_unprepare(task);
    }
}
