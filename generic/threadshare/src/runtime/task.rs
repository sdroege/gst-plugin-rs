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
use std::stringify;
use std::sync::{Arc, Mutex, MutexGuard};

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

// FIXME impl Future so that we can await without matching on the variant
/// Transition details.
///
/// A state transition occurs as a result of a triggering event.
#[derive(Debug)]
pub enum TransitionStatus {
    /// Transition completed successfully.
    Complete {
        origin: TaskState,
        target: TaskState,
    },
    /// Asynchronously awaiting for transition completion.
    ///
    /// This occurs when the event is triggered from a `Context`.
    Async {
        trigger: Trigger,
        origin: TaskState,
        ack_handle: JoinHandle<Result<TransitionStatus, TransitionError>>,
    },
    // FIXME remove or edit doc
    /// Not waiting for transition completion.
    ///
    /// This is to prevent:
    /// - A deadlock when executing a transition action.
    /// - A potential infinite wait when pausing a running loop
    ///   which could be awaiting for an `iterate` to complete.
    NotWaiting { trigger: Trigger, origin: TaskState },
    /// Skipping triggering event due to current state.
    Skipped { trigger: Trigger, state: TaskState },
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

type AckSender = oneshot::Sender<Result<TransitionStatus, TransitionError>>;
type AckReceiver = oneshot::Receiver<Result<TransitionStatus, TransitionError>>;

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

    fn send_ack(self, res: Result<TransitionStatus, TransitionError>) {
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

        self.triggering_evt_tx
            .try_send(triggering_evt)
            .unwrap_or_else(|err| {
                panic!("trigger channel failure: {}", err);
            });

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
        let res = Ok(TransitionStatus::Complete {
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
        let res = Ok(TransitionStatus::Skipped {
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

    pub fn prepare(
        &self,
        task_impl: impl TaskImpl,
        context: Context,
    ) -> Result<TransitionStatus, TransitionError> {
        let mut inner = self.0.lock().unwrap();

        let origin = inner.state;
        match origin {
            TaskState::Unprepared => (),
            TaskState::Prepared | TaskState::Preparing => {
                gst::debug!(RUNTIME_CAT, "Task already {:?}", origin);
                return Ok(TransitionStatus::Skipped {
                    trigger: Trigger::Prepare,
                    state: origin,
                });
            }
            state => {
                gst::warning!(RUNTIME_CAT, "Attempt to prepare Task in state {:?}", state);
                return Err(TransitionError {
                    trigger: Trigger::Prepare,
                    state: inner.state,
                    err_msg: gst::error_msg!(
                        gst::CoreError::StateChange,
                        ["Attempt to prepare Task in state {:?}", state]
                    ),
                });
            }
        }

        assert!(inner.state_machine_handle.is_none());

        inner.state = TaskState::Preparing;

        gst::log!(RUNTIME_CAT, "Spawning task state machine");
        inner.state_machine_handle = Some(StateMachine::spawn(
            self.0.clone(),
            Box::new(task_impl),
            context.clone(),
        ));

        let ack_rx = inner.trigger(Trigger::Prepare)?;
        drop(inner);

        let ack_await_fut = async move {
            gst::trace!(RUNTIME_CAT, "Awaiting ack for Prepare");

            let res = ack_rx.await.unwrap();
            if res.is_ok() {
                gst::log!(RUNTIME_CAT, "Received ack {:?} for Prepare", res);
            } else {
                gst::error!(RUNTIME_CAT, "Received ack {:?} for Prepare", res);
            }

            res
        };

        Ok(TransitionStatus::Async {
            trigger: Trigger::Prepare,
            origin: TaskState::Unprepared,
            ack_handle: context.spawn_and_unpark(ack_await_fut),
        })
    }

    pub fn unprepare(&self) -> Result<TransitionStatus, TransitionError> {
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
                    return Ok(TransitionStatus::Skipped {
                        trigger: Trigger::Unprepare,
                        state: origin,
                    });
                }
            },
            state => {
                gst::warning!(
                    RUNTIME_CAT,
                    "Attempt to unprepare Task in state {:?}",
                    state
                );
                return Err(TransitionError {
                    trigger: Trigger::Unprepare,
                    state: inner.state,
                    err_msg: gst::error_msg!(
                        gst::CoreError::StateChange,
                        ["Attempt to unprepare Task in state {:?}", state]
                    ),
                });
            }
        };

        inner.abort_task_loop();
        let ack_rx = state_machine_handle.trigger(Trigger::Unprepare);
        drop(inner);

        let state_machine_end_fut = async {
            state_machine_handle.join().await;

            let res = ack_rx.await.unwrap();
            if res.is_ok() {
                gst::log!(RUNTIME_CAT, "Received ack {:?} for Unprepare", res);
            } else {
                gst::error!(RUNTIME_CAT, "Received ack {:?} for Unprepare", res);
            }

            res
        };

        if let Some(cur_context) = Context::current() {
            let ack_handle = cur_context.spawn_and_unpark(state_machine_end_fut);
            gst::log!(
                RUNTIME_CAT,
                "Will wait for state machine termination completion in {:?} on context {}",
                ack_handle.task_id(),
                cur_context.name()
            );

            return Ok(TransitionStatus::Async {
                trigger: Trigger::Unprepare,
                origin,
                ack_handle,
            });
        } else {
            gst::log!(
                RUNTIME_CAT,
                "Waiting for state machine termination on current thread"
            );
            // Use a light-weight executor, no timer nor async io.
            futures::executor::block_on(state_machine_end_fut.map(drop))
        }

        Ok(TransitionStatus::Complete {
            origin,
            target: TaskState::Unprepared,
        })
    }

    /// Starts the `Task`.
    ///
    /// The execution occurs on the `Task` context.
    pub fn start(&self) -> Result<TransitionStatus, TransitionError> {
        let mut inner = self.0.lock().unwrap();

        let ack_rx = inner.trigger(Trigger::Start)?;

        if let TaskState::Started = inner.state {
            return Ok(TransitionStatus::Skipped {
                trigger: Trigger::Start,
                state: TaskState::Started,
            });
        }

        Self::await_ack(inner, ack_rx, Trigger::Start)
    }

    /// Requests the `Task` loop to pause.
    ///
    /// If an iteration is in progress, it will run to completion,
    /// then no more iteration will be executed before `start` is called again.
    /// Therefore, it is not guaranteed that `Paused` is reached when `pause` returns.
    pub fn pause(&self) -> Result<TransitionStatus, TransitionError> {
        let mut inner = self.0.lock().unwrap();

        let ack_rx = inner.trigger(Trigger::Pause)?;

        if let TaskState::Started = inner.state {
            // FIXME this could be async
            return Ok(TransitionStatus::NotWaiting {
                trigger: Trigger::Pause,
                origin: TaskState::Started,
            });
        }

        Self::await_ack(inner, ack_rx, Trigger::Pause)
    }

    pub fn flush_start(&self) -> Result<TransitionStatus, TransitionError> {
        self.abort_push_await(Trigger::FlushStart)
    }

    pub fn flush_stop(&self) -> Result<TransitionStatus, TransitionError> {
        self.abort_push_await(Trigger::FlushStop)
    }

    /// Stops the `Started` `Task` and wait for it to finish.
    pub fn stop(&self) -> Result<TransitionStatus, TransitionError> {
        self.abort_push_await(Trigger::Stop)
    }

    /// Pushes a [`Trigger`] which requires the iteration loop to abort ASAP.
    ///
    /// This function:
    /// - Aborts the iteration loop aborts.
    /// - Pushes the provided [`Trigger`].
    /// - Awaits for the expected transition as usual.
    fn abort_push_await(&self, trigger: Trigger) -> Result<TransitionStatus, TransitionError> {
        let mut inner = self.0.lock().unwrap();

        inner.abort_task_loop();
        let ack_rx = inner.trigger(trigger)?;
        Self::await_ack(inner, ack_rx, trigger)
    }

    fn await_ack(
        inner: MutexGuard<TaskInner>,
        ack_rx: oneshot::Receiver<Result<TransitionStatus, TransitionError>>,
        trigger: Trigger,
    ) -> Result<TransitionStatus, TransitionError> {
        let origin = inner.state;
        drop(inner);

        let ack_await_fut = async move {
            gst::trace!(RUNTIME_CAT, "Awaiting ack for {:?}", trigger);

            let res = ack_rx.await.unwrap();
            if res.is_ok() {
                gst::log!(RUNTIME_CAT, "Received ack {:?} for {:?}", res, trigger);
            } else {
                gst::error!(RUNTIME_CAT, "Received ack {:?} for {:?}", res, trigger);
            }

            res
        };

        if let Some(cur_context) = Context::current() {
            let ack_handle = cur_context.spawn(ack_await_fut);

            gst::log!(
                RUNTIME_CAT,
                "Awaiting ack for {:?} in {:?} on context {}",
                trigger,
                ack_handle.task_id(),
                cur_context.name()
            );

            Ok(TransitionStatus::Async {
                trigger,
                origin,
                ack_handle,
            })
        } else {
            gst::log!(
                RUNTIME_CAT,
                "Awaiting ack for {:?} on current thread",
                trigger
            );
            // Use a light-weight executor, no timer nor async io.
            futures::executor::block_on(ack_await_fut)
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
                let res = Ok(TransitionStatus::Complete {
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

    use crate::runtime::Context;

    use super::*;

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
                    gst::debug!(RUNTIME_CAT, "task_iterate: prepared");
                    self.prepared_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "task_iterate: started");
                    self.started_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "task_iterate: entering iterate");
                    self.iterate_sender.send(()).await.unwrap();

                    gst::debug!(
                        RUNTIME_CAT,
                        "task_iterate: awaiting complete_iterate_receiver"
                    );

                    let res = self.complete_iterate_receiver.next().await.unwrap();
                    if res.is_ok() {
                        gst::debug!(RUNTIME_CAT, "task_iterate: received Ok => keep looping");
                    } else {
                        gst::debug!(
                            RUNTIME_CAT,
                            "task_iterate: received {:?} => cancelling loop",
                            res
                        );
                    }

                    res
                }
                .boxed()
            }

            fn pause(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "task_iterate: paused");
                    self.paused_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "task_iterate: stopped");
                    self.stopped_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst::debug!(RUNTIME_CAT, "task_iterate: stopped");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn unprepare(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst::debug!(RUNTIME_CAT, "task_iterate: unprepared");
                    self.unprepared_sender.send(()).await.unwrap();
                }
                .boxed()
            }
        }

        let context = Context::acquire("task_iterate", Duration::from_millis(2)).unwrap();

        let task = Task::default();

        assert_eq!(task.state(), TaskState::Unprepared);

        gst::debug!(RUNTIME_CAT, "task_iterate: preparing");

        let (prepared_sender, mut prepared_receiver) = mpsc::channel(1);
        let (started_sender, mut started_receiver) = mpsc::channel(1);
        let (iterate_sender, mut iterate_receiver) = mpsc::channel(1);
        let (mut complete_iterate_sender, complete_iterate_receiver) = mpsc::channel(1);
        let (paused_sender, mut paused_receiver) = mpsc::channel(1);
        let (stopped_sender, mut stopped_receiver) = mpsc::channel(1);
        let (flush_start_sender, mut flush_start_receiver) = mpsc::channel(1);
        let (unprepared_sender, mut unprepared_receiver) = mpsc::channel(1);
        let res = task.prepare(
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

        let prepare_ack_handle = match res {
            Ok(TransitionStatus::Async {
                trigger: Trigger::Prepare,
                origin: TaskState::Unprepared,
                ack_handle,
            }) => ack_handle,
            other => panic!("unexpected {:?}", other),
        };

        gst::debug!(RUNTIME_CAT, "task_iterate: starting (initial)");

        match task.start() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Prepared,
                target: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }

        assert_eq!(task.state(), TaskState::Started);
        // At this point, preparation must be complete
        match block_on(prepare_ack_handle).unwrap() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Unprepared,
                target: TaskState::Prepared,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        block_on(prepared_receiver.next()).unwrap();
        // ... and start executed
        block_on(started_receiver.next()).unwrap();

        assert_eq!(task.state(), TaskState::Started);

        // unlock task loop and keep looping
        block_on(iterate_receiver.next()).unwrap();
        block_on(complete_iterate_sender.send(Ok(()))).unwrap();

        gst::debug!(RUNTIME_CAT, "task_iterate: starting (redundant)");
        // already started
        match task.start() {
            Ok(TransitionStatus::Skipped {
                trigger: Trigger::Start,
                state: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        assert_eq!(task.state(), TaskState::Started);

        gst::debug!(RUNTIME_CAT, "task_iterate: pause (initial)");
        match task.pause() {
            Ok(TransitionStatus::NotWaiting {
                trigger: Trigger::Pause,
                origin: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }

        // Pause transition is asynchronous
        while TaskState::Paused != task.state() {
            std::thread::sleep(Duration::from_millis(2));

            if let Ok(Some(())) = iterate_receiver.try_next() {
                // unlock iteration
                block_on(complete_iterate_sender.send(Ok(()))).unwrap();
            }
        }

        gst::debug!(RUNTIME_CAT, "task_iterate: awaiting pause ack");
        block_on(paused_receiver.next()).unwrap();

        gst::debug!(RUNTIME_CAT, "task_iterate: starting (after pause)");
        match task.start() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Paused,
                target: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }

        assert_eq!(task.state(), TaskState::Started);
        // Paused -> Started
        let _ = block_on(started_receiver.next());

        gst::debug!(RUNTIME_CAT, "task_iterate: stopping");
        match task.stop() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Started,
                target: TaskState::Stopped,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }

        assert_eq!(task.state(), TaskState::Stopped);
        let _ = block_on(stopped_receiver.next());

        // purge remaining iteration received before stop if any
        let _ = iterate_receiver.try_next();

        gst::debug!(RUNTIME_CAT, "task_iterate: starting (after stop)");
        match task.start() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Stopped,
                target: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        let _ = block_on(started_receiver.next());

        gst::debug!(RUNTIME_CAT, "task_iterate: req. iterate to return Eos");
        block_on(iterate_receiver.next()).unwrap();
        block_on(complete_iterate_sender.send(Err(gst::FlowError::Eos))).unwrap();

        gst::debug!(RUNTIME_CAT, "task_iterate: awaiting stop ack");
        block_on(stopped_receiver.next()).unwrap();

        // Wait for state machine to reach Stopped
        while TaskState::Stopped != task.state() {
            std::thread::sleep(Duration::from_millis(2));
        }

        gst::debug!(RUNTIME_CAT, "task_iterate: starting (after stop)");
        match task.start() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Stopped,
                target: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        let _ = block_on(started_receiver.next());

        gst::debug!(RUNTIME_CAT, "task_iterate: req. iterate to return Flushing");
        block_on(iterate_receiver.next()).unwrap();
        block_on(complete_iterate_sender.send(Err(gst::FlowError::Flushing))).unwrap();

        gst::debug!(RUNTIME_CAT, "task_iterate: awaiting flush_start ack");
        block_on(flush_start_receiver.next()).unwrap();

        // Wait for state machine to reach Flushing
        while TaskState::Flushing != task.state() {
            std::thread::sleep(Duration::from_millis(2));
        }

        gst::debug!(RUNTIME_CAT, "task_iterate: stop flushing");
        match task.flush_stop() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Flushing,
                target: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        let _ = block_on(started_receiver.next());

        gst::debug!(RUNTIME_CAT, "task_iterate: req. iterate to return Error");
        block_on(iterate_receiver.next()).unwrap();
        block_on(complete_iterate_sender.send(Err(gst::FlowError::Error))).unwrap();

        // Wait for state machine to reach Error
        while TaskState::Error != task.state() {
            std::thread::sleep(Duration::from_millis(2));
        }

        gst::debug!(
            RUNTIME_CAT,
            "task_iterate: attempting to start (after Error)"
        );
        match task.start() {
            Err(TransitionError {
                trigger: Trigger::Start,
                state: TaskState::Error,
                ..
            }) => (),
            _ => unreachable!(),
        }

        match task.unprepare() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Error,
                target: TaskState::Unprepared,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }

        assert_eq!(task.state(), TaskState::Unprepared);
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

        assert_eq!(task.state(), TaskState::Unprepared);

        let (prepare_error_sender, mut prepare_error_receiver) = mpsc::channel(1);
        task.prepare(
            TaskPrepareTest {
                prepare_error_sender,
            },
            context,
        )
        .unwrap();

        gst::debug!(
            RUNTIME_CAT,
            "prepare_error: await action error notification"
        );
        block_on(prepare_error_receiver.next()).unwrap();

        // Wait for state machine to reach Error
        while TaskState::Error != task.state() {
            std::thread::sleep(Duration::from_millis(2));
        }

        match task.start() {
            Err(TransitionError {
                trigger: Trigger::Start,
                state: TaskState::Error,
                ..
            }) => (),
            other => unreachable!("{:?}", other),
        }

        task.unprepare().unwrap();
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
        task.prepare(TaskPrepareTest { prepare_receiver }, context)
            .unwrap();

        let start_ctx = Context::acquire("prepare_start_ok_requester", Duration::ZERO).unwrap();
        let (ready_sender, ready_receiver) = oneshot::channel();
        let start_handle = start_ctx.spawn(async move {
            assert_eq!(task.state(), TaskState::Preparing);
            gst::debug!(RUNTIME_CAT, "prepare_start_ok: starting");
            let ack_handle = match task.start() {
                Ok(TransitionStatus::Async {
                    trigger: Trigger::Start,
                    origin: TaskState::Preparing,
                    ack_handle,
                }) => ack_handle,
                other => panic!("unexpected {:?}", other),
            };
            ready_sender.send(()).unwrap();
            match ack_handle.await.unwrap() {
                Ok(TransitionStatus::Complete {
                    origin: TaskState::Prepared,
                    target: TaskState::Started,
                }) => (),
                other => panic!("unexpected {:?}", other),
            }
            assert_eq!(task.state(), TaskState::Started);

            let ack_handle = match task.stop() {
                Ok(TransitionStatus::Async {
                    trigger: Trigger::Stop,
                    origin: TaskState::Started,
                    ack_handle,
                }) => ack_handle,
                other => panic!("unexpected {:?}", other),
            };
            match ack_handle.await.unwrap() {
                Ok(TransitionStatus::Complete {
                    origin: TaskState::Started,
                    target: TaskState::Stopped,
                }) => (),
                other => panic!("unexpected {:?}", other),
            }
            assert_eq!(task.state(), TaskState::Stopped);

            let ack_handle = match task.unprepare() {
                Ok(TransitionStatus::Async {
                    trigger: Trigger::Unprepare,
                    origin: TaskState::Stopped,
                    ack_handle,
                }) => ack_handle,
                other => panic!("unexpected {:?}", other),
            };
            match ack_handle.await.unwrap() {
                Ok(TransitionStatus::Complete {
                    origin: TaskState::Stopped,
                    target: TaskState::Unprepared,
                }) => (),
                other => panic!("unexpected {:?}", other),
            }
            assert_eq!(task.state(), TaskState::Unprepared);
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
        let res = task.prepare(
            TaskPrepareTest {
                prepare_receiver,
                prepare_error_sender,
            },
            context,
        );
        let prepare_ack = match res {
            Ok(TransitionStatus::Async {
                trigger: Trigger::Prepare,
                origin: TaskState::Unprepared,
                ack_handle,
            }) => ack_handle,
            other => panic!("unexpected {:?}", other),
        };

        let start_ctx = Context::acquire("prepare_start_error_requester", Duration::ZERO).unwrap();
        let (ready_sender, ready_receiver) = oneshot::channel();
        let start_handle = start_ctx.spawn(async move {
            gst::debug!(RUNTIME_CAT, "prepare_start_error: starting (Err)");
            task.start().unwrap();
            ready_sender.send(()).unwrap();
            // FIXME we loose the origin Trigger (Start)
            // and only get the Trigger returned by handle_action_error
            // see also: comment in exec_action!
            match prepare_ack.await.unwrap() {
                Err(TransitionError {
                    trigger: Trigger::Error,
                    state: TaskState::Preparing,
                    ..
                }) => (),
                other => panic!("unexpected transition res {:?}", other),
            }

            let ack_handle = match task.unprepare().unwrap() {
                TransitionStatus::Async {
                    trigger: Trigger::Unprepare,
                    origin: TaskState::Error,
                    ack_handle,
                } => ack_handle,
                other => panic!("unexpected {:?}", other),
            };
            match ack_handle.await.unwrap() {
                Ok(TransitionStatus::Complete {
                    origin: TaskState::Error,
                    target: TaskState::Unprepared,
                }) => (),
                other => panic!("unexpected {:?}", other),
            }
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
        task.prepare(
            TaskPauseStartTest {
                iterate_sender,
                complete_receiver,
                paused_sender,
            },
            context,
        )
        .unwrap();

        gst::debug!(RUNTIME_CAT, "pause_start: starting");
        match task.start() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Prepared,
                target: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        assert_eq!(task.state(), TaskState::Started);

        gst::debug!(RUNTIME_CAT, "pause_start: awaiting 1st iteration");
        block_on(iterate_receiver.next()).unwrap();

        gst::debug!(RUNTIME_CAT, "pause_start: pausing (1)");
        match task.pause() {
            Ok(TransitionStatus::NotWaiting {
                trigger: Trigger::Pause,
                origin: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }

        gst::debug!(RUNTIME_CAT, "pause_start: sending 1st iteration completion");
        complete_sender.try_send(()).unwrap();

        // Pause transition is asynchronous
        while TaskState::Paused != task.state() {
            std::thread::sleep(Duration::from_millis(5));
        }

        gst::debug!(RUNTIME_CAT, "pause_start: awaiting paused");
        let _ = block_on(paused_receiver.next());

        // Loop held on due to Pause
        iterate_receiver.try_next().unwrap_err();
        match task.start() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Paused,
                target: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        assert_eq!(task.state(), TaskState::Started);

        gst::debug!(RUNTIME_CAT, "pause_start: awaiting 2d iteration");
        block_on(iterate_receiver.next()).unwrap();

        gst::debug!(RUNTIME_CAT, "pause_start: sending 2d iteration completion");
        complete_sender.try_send(()).unwrap();

        task.stop().unwrap();
        task.unprepare().unwrap();
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
        task.prepare(TaskPauseStartTest { iterate_sender }, context)
            .unwrap();

        gst::debug!(RUNTIME_CAT, "successive_pause_start: starting");
        task.start().unwrap();

        gst::debug!(RUNTIME_CAT, "successive_pause_start: awaiting iteration 1");
        block_on(iterate_receiver.next()).unwrap();

        gst::debug!(RUNTIME_CAT, "successive_pause_start: pause and start");
        task.pause().unwrap();
        task.start().unwrap();

        assert_eq!(task.state(), TaskState::Started);

        gst::debug!(RUNTIME_CAT, "successive_pause_start: awaiting iteration 2");
        block_on(iterate_receiver.next()).unwrap();

        gst::debug!(RUNTIME_CAT, "successive_pause_start: stopping");
        task.stop().unwrap();
        task.unprepare().unwrap();
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
        task.prepare(
            TaskFlushTest {
                flush_start_sender,
                flush_stop_sender,
            },
            context,
        )
        .unwrap();

        gst::debug!(RUNTIME_CAT, "flush_regular_sync: start");
        task.start().unwrap();

        gst::debug!(RUNTIME_CAT, "flush_regular_sync: starting flush");
        match task.flush_start() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Started,
                target: TaskState::Flushing,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        assert_eq!(task.state(), TaskState::Flushing);

        block_on(flush_start_receiver.next()).unwrap();

        gst::debug!(RUNTIME_CAT, "flush_regular_sync: stopping flush");
        match task.flush_stop() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Flushing,
                target: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        assert_eq!(task.state(), TaskState::Started);

        block_on(flush_stop_receiver.next()).unwrap();

        task.pause().unwrap();
        task.stop().unwrap();
        task.unprepare().unwrap();
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
        task.prepare(
            TaskFlushTest {
                flush_start_sender,
                flush_stop_sender,
            },
            context,
        )
        .unwrap();

        gst::debug!(RUNTIME_CAT, "flush_regular_different_context: start");
        task.start().unwrap();

        let oob_context = Context::acquire(
            "flush_regular_different_context_oob",
            Duration::from_millis(2),
        )
        .unwrap();

        let task_clone = task.clone();
        let flush_handle = oob_context.spawn(async move {
            let flush_ack_handle = match task_clone.flush_start() {
                Ok(TransitionStatus::Async {
                    trigger: Trigger::FlushStart,
                    origin: TaskState::Started,
                    ack_handle,
                }) => ack_handle,
                other => panic!("unexpected {:?}", other),
            };
            match flush_ack_handle.await.unwrap() {
                Ok(TransitionStatus::Complete {
                    origin: TaskState::Started,
                    target: TaskState::Flushing,
                }) => (),
                other => panic!("unexpected {:?}", other),
            }
            assert_eq!(task_clone.state(), TaskState::Flushing);
            flush_start_receiver.next().await.unwrap();

            let flush_stop_ack_handle = match task_clone.flush_stop() {
                Ok(TransitionStatus::Async {
                    trigger: Trigger::FlushStop,
                    origin: TaskState::Flushing,
                    ack_handle,
                }) => ack_handle,
                other => panic!("unexpected {:?}", other),
            };
            match flush_stop_ack_handle.await.unwrap() {
                Ok(TransitionStatus::Complete {
                    origin: TaskState::Flushing,
                    target: TaskState::Started,
                }) => (),
                other => panic!("unexpected {:?}", other),
            }
            assert_eq!(task_clone.state(), TaskState::Started);
        });

        block_on(flush_handle).unwrap();
        block_on(flush_stop_receiver.next()).unwrap();

        task.stop().unwrap();
        task.unprepare().unwrap();
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
        task.prepare(
            TaskFlushTest {
                flush_start_sender,
                flush_stop_sender,
            },
            context.clone(),
        )
        .unwrap();

        task.start().unwrap();

        let task_clone = task.clone();
        let flush_handle = context.spawn(async move {
            let flush_ack_handle = match task_clone.flush_start() {
                Ok(TransitionStatus::Async {
                    trigger: Trigger::FlushStart,
                    origin: TaskState::Started,
                    ack_handle,
                }) => ack_handle,
                other => panic!("unexpected {:?}", other),
            };
            match flush_ack_handle.await.unwrap() {
                Ok(TransitionStatus::Complete {
                    origin: TaskState::Started,
                    target: TaskState::Flushing,
                }) => (),
                other => panic!("unexpected {:?}", other),
            }
            assert_eq!(task_clone.state(), TaskState::Flushing);
            flush_start_receiver.next().await.unwrap();

            let flush_stop_ack_handle = match task_clone.flush_stop() {
                Ok(TransitionStatus::Async {
                    trigger: Trigger::FlushStop,
                    origin: TaskState::Flushing,
                    ack_handle,
                }) => ack_handle,
                other => panic!("unexpected {:?}", other),
            };
            match flush_stop_ack_handle.await.unwrap() {
                Ok(TransitionStatus::Complete {
                    origin: TaskState::Flushing,
                    target: TaskState::Started,
                }) => (),
                other => panic!("unexpected {:?}", other),
            }
            assert_eq!(task_clone.state(), TaskState::Started);
        });

        block_on(flush_handle).unwrap();
        block_on(flush_stop_receiver.next()).unwrap();

        task.stop().unwrap();
        task.unprepare().unwrap();
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
                    match self.task.flush_start() {
                        Ok(TransitionStatus::Async {
                            trigger: Trigger::FlushStart,
                            origin: TaskState::Started,
                            ..
                        }) => (),
                        other => panic!("unexpected {:?}", other),
                    }
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
        task.prepare(
            TaskFlushTest {
                task: task.clone(),
                flush_start_sender,
            },
            context,
        )
        .unwrap();

        task.start().unwrap();

        gst::debug!(
            RUNTIME_CAT,
            "flush_from_loop: awaiting flush_start notification"
        );
        block_on(flush_start_receiver.next()).unwrap();

        match task.stop() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Flushing,
                target: TaskState::Stopped,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        task.unprepare().unwrap();
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
                        Ok(TransitionStatus::NotWaiting {
                            trigger: Trigger::Pause,
                            origin: TaskState::Started,
                        }) => (),
                        other => panic!("unexpected {:?}", other),
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
        task.prepare(
            TaskStartTest {
                task: task.clone(),
                pause_sender,
            },
            context,
        )
        .unwrap();

        task.start().unwrap();

        gst::debug!(RUNTIME_CAT, "pause_from_loop: awaiting pause notification");
        block_on(pause_receiver.next()).unwrap();

        task.stop().unwrap();
        task.unprepare().unwrap();
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
                        Ok(TransitionStatus::Async {
                            trigger: Trigger::FlushStop,
                            origin: TaskState::Started,
                            ..
                        }) => (),
                        other => panic!("unexpected {:?}", other),
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
        task.prepare(
            TaskFlushTest {
                task: task.clone(),
                flush_stop_sender,
            },
            context,
        )
        .unwrap();

        task.start().unwrap();
        task.flush_start().unwrap();

        gst::debug!(
            RUNTIME_CAT,
            "trigger_from_action: awaiting flush_stop notification"
        );
        block_on(flush_stop_receiver.next()).unwrap();

        task.stop().unwrap();
        task.unprepare().unwrap();
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
        task.prepare(
            TaskFlushTest {
                started_sender,
                flush_start_sender,
                flush_stop_sender,
            },
            context,
        )
        .unwrap();

        // Pause, FlushStart, FlushStop, Start

        gst::debug!(RUNTIME_CAT, "pause_flush_start: pausing");
        match task.pause() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Prepared,
                target: TaskState::Paused,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }

        gst::debug!(RUNTIME_CAT, "pause_flush_start: starting flush");
        match task.flush_start() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Paused,
                target: TaskState::PausedFlushing,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        assert_eq!(task.state(), TaskState::PausedFlushing);
        block_on(flush_start_receiver.next());

        gst::debug!(RUNTIME_CAT, "pause_flush_start: stopping flush");
        match task.flush_stop() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::PausedFlushing,
                target: TaskState::Paused,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        assert_eq!(task.state(), TaskState::Paused);
        block_on(flush_stop_receiver.next());

        // start action not executed
        started_receiver.try_next().unwrap_err();

        gst::debug!(RUNTIME_CAT, "pause_flush_start: starting after flushing");
        match task.start() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Paused,
                target: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        assert_eq!(task.state(), TaskState::Started);
        block_on(started_receiver.next());

        task.stop().unwrap();
        task.unprepare().unwrap();
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
        task.prepare(
            TaskFlushTest {
                started_sender,
                flush_start_sender,
                flush_stop_sender,
            },
            context,
        )
        .unwrap();

        // Pause, FlushStart, Start, FlushStop

        gst::debug!(RUNTIME_CAT, "pause_flushing_start: pausing");
        task.pause().unwrap();

        gst::debug!(RUNTIME_CAT, "pause_flushing_start: starting flush");
        task.flush_start().unwrap();
        assert_eq!(task.state(), TaskState::PausedFlushing);
        block_on(flush_start_receiver.next());

        gst::debug!(RUNTIME_CAT, "pause_flushing_start: starting while flushing");
        match task.start() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::PausedFlushing,
                target: TaskState::Flushing,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        assert_eq!(task.state(), TaskState::Flushing);

        // start action not executed
        started_receiver.try_next().unwrap_err();

        gst::debug!(RUNTIME_CAT, "pause_flushing_start: stopping flush");
        match task.flush_stop() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Flushing,
                target: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        assert_eq!(task.state(), TaskState::Started);
        block_on(flush_stop_receiver.next());
        block_on(started_receiver.next());

        task.stop().unwrap();
        task.unprepare().unwrap();
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
        task.prepare(
            TaskStartTest {
                flush_start_sender,
                flush_stop_sender,
            },
            context,
        )
        .unwrap();

        let oob_context =
            Context::acquire("flush_concurrent_start_oob", Duration::from_millis(2)).unwrap();
        let task_clone = task.clone();

        task.pause().unwrap();

        // Launch flush_start // start
        let (ready_sender, ready_receiver) = oneshot::channel();
        gst::debug!(RUNTIME_CAT, "flush_concurrent_start: spawning flush_start");
        let flush_start_handle = oob_context.spawn(async move {
            gst::debug!(RUNTIME_CAT, "flush_concurrent_start: // flush_start");
            ready_sender.send(()).unwrap();
            let res = task_clone.flush_start().unwrap();
            let ack_handle = match res {
                TransitionStatus::Async {
                    trigger: Trigger::FlushStart,
                    origin: TaskState::Paused,
                    ack_handle,
                } => ack_handle,
                TransitionStatus::Async {
                    trigger: Trigger::FlushStart,
                    origin: TaskState::Started,
                    ack_handle,
                } => ack_handle,
                other => panic!("unexpected {:?}", other),
            };
            ack_handle.await.unwrap().unwrap();
            flush_start_receiver.next().await.unwrap();
        });

        gst::debug!(
            RUNTIME_CAT,
            "flush_concurrent_start: awaiting for oob_context"
        );
        block_on(ready_receiver).unwrap();

        gst::debug!(RUNTIME_CAT, "flush_concurrent_start: // start");
        let res = task.start().unwrap();
        match res {
            TransitionStatus::Complete {
                origin: TaskState::Paused,
                target: TaskState::Started,
            } => (),
            TransitionStatus::Complete {
                origin: TaskState::PausedFlushing,
                target: TaskState::Flushing,
            } => (),
            other => panic!("unexpected {:?}", other),
        }

        block_on(flush_start_handle).unwrap();

        gst::debug!(RUNTIME_CAT, "flush_concurrent_start: requesting flush_stop");
        match task.flush_stop() {
            Ok(TransitionStatus::Complete {
                origin: TaskState::Flushing,
                target: TaskState::Started,
            }) => (),
            other => panic!("unexpected {:?}", other),
        }
        assert_eq!(task.state(), TaskState::Started);
        block_on(flush_stop_receiver.next());

        task.stop().unwrap();
        task.unprepare().unwrap();
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
        task.prepare(
            TaskTimerTest {
                timer: None,
                timer_elapsed_sender: Some(timer_elapsed_sender),
            },
            context,
        )
        .unwrap();

        gst::debug!(RUNTIME_CAT, "start_timer: start");
        task.start().unwrap();

        block_on(timer_elapsed_receiver).unwrap();
        gst::debug!(RUNTIME_CAT, "start_timer: timer elapsed received");

        task.stop().unwrap();
        task.unprepare().unwrap();
    }
}
