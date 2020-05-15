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

//! An execution loop to run asynchronous processing.

use futures::channel::mpsc as async_mpsc;
use futures::channel::oneshot;
use futures::future::{self, abortable, AbortHandle, Aborted, BoxFuture};
use futures::prelude::*;
use futures::stream::StreamExt;

use gst::{gst_debug, gst_error, gst_error_msg, gst_fixme, gst_log, gst_trace, gst_warning};

use std::fmt;
use std::ops::Deref;
use std::stringify;
use std::sync::{Arc, Mutex, MutexGuard};

use super::executor::{block_on_or_add_sub_task, TaskId};
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
    Unpreparing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transition {
    Error,
    FlushStart,
    FlushStop,
    Pause,
    Prepare,
    Start,
    Stop,
    Unprepare,
}

/// TransitionRequest error details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransitionError {
    pub transition: Transition,
    pub state: TaskState,
    pub err_msg: gst::ErrorMessage,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:?} from state {:?}: {:?}",
            self.transition, self.state, self.err_msg
        )
    }
}

impl std::error::Error for TransitionError {}

impl From<TransitionError> for gst::ErrorMessage {
    fn from(err: TransitionError) -> Self {
        err.err_msg
    }
}

/// Transition request handling details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransitionStatus {
    /// Transition completed successfully.
    Complete {
        origin: TaskState,
        target: TaskState,
    },
    /// The transition acknowledgement was spawned in a subtask.
    ///
    /// This occurs when the transition is requested from a `Context`.
    Async {
        transition: Transition,
        origin: TaskState,
    },
    /// Not waiting for transition completion.
    ///
    /// This is to prevent:
    /// - A deadlock when executing from a `TaskImpl` hook.
    /// - A potential infinite wait when pausing a running loop
    ///   which could be awaiting for an `iterate` to complete.
    NotWaiting {
        transition: Transition,
        origin: TaskState,
    },
    /// Skipping transition due to current state.
    Skipped {
        transition: Transition,
        state: TaskState,
    },
}

/// Implementation trait for `Task`s.
///
/// Defines implementations for specific state transitions.
///
/// In the event of a failure, the implementer must call
/// `gst_element_error!`.
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
    /// and return `Transition::Error`.
    ///
    /// Otherwise, handle the error and return the requested `Transition` to recover.
    ///
    /// Default behaviour depends on the `err`:
    ///
    /// - `FlowError::Flushing` -> `Transition::FlushStart`.
    /// - `FlowError::Eos` -> `Transition::Stop`.
    /// - Other `FlowError` -> `Transition::Error`.
    fn handle_iterate_error(&mut self, err: gst::FlowError) -> BoxFuture<'_, Transition> {
        async move {
            match err {
                gst::FlowError::Flushing => {
                    gst_debug!(
                        RUNTIME_CAT,
                        "TaskImpl iterate returned Flushing. Posting FlushStart"
                    );
                    Transition::FlushStart
                }
                gst::FlowError::Eos => {
                    gst_debug!(RUNTIME_CAT, "TaskImpl iterate returned Eos. Posting Stop");
                    Transition::Stop
                }
                other => {
                    gst_error!(
                        RUNTIME_CAT,
                        "TaskImpl iterate returned {:?}. Posting Error",
                        other
                    );
                    Transition::Error
                }
            }
        }
        .boxed()
    }

    /// Handles an error occuring during the execution of a transition hook.
    ///
    /// This handler also catches errors returned by subtasks spawned by the transition hook.
    ///
    /// If the error is unrecoverable, implementations might use `gst::Element::post_error_message`
    /// and return `Transition::Error`.
    ///
    /// Otherwise, handle the error and return the requested `Transition` to recover.
    ///
    /// Default is to `gst_error` log and return `Transition::Error`.
    fn handle_hook_error(
        &mut self,
        transition: Transition,
        state: TaskState,
        err: gst::ErrorMessage,
    ) -> BoxFuture<'_, Transition> {
        async move {
            gst_error!(
                RUNTIME_CAT,
                "TaskImpl hook error during {:?} from {:?}: {:?}. Posting Transition::Error",
                transition,
                state,
                err,
            );

            Transition::Error
        }
        .boxed()
    }
}

struct TransitionRequest {
    kind: Transition,
    ack_tx: oneshot::Sender<Result<TransitionStatus, TransitionError>>,
}

impl TransitionRequest {
    fn new(
        kind: Transition,
    ) -> (
        Self,
        oneshot::Receiver<Result<TransitionStatus, TransitionError>>,
    ) {
        let (ack_tx, ack_rx) = oneshot::channel();
        let req = TransitionRequest { kind, ack_tx };

        (req, ack_rx)
    }

    fn send_ack(self, res: Result<TransitionStatus, TransitionError>) {
        let _ = self.ack_tx.send(res);
    }

    fn send_err_ack(self) {
        let res = Err(TransitionError {
            transition: self.kind,
            state: TaskState::Error,
            err_msg: gst_error_msg!(
                gst::CoreError::StateChange,
                [
                    "Transition {:?} failed due to a previous unrecoverable error",
                    self.kind,
                ]
            ),
        });

        self.send_ack(res);
    }
}

impl fmt::Debug for TransitionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransitionRequest")
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Debug)]
struct TaskInner {
    // The state machine needs an un-throttling Context because otherwise
    // `transition_rx.next()` would throttle at each transition request.
    // Since pipelines serialize state changes, this would lead to long starting / stopping
    // when a pipeline consists in a large number of elements.
    // See also the comment about transition_tx below.
    state_machine_context: Context,
    // The TaskImpl processings are spawned on the Task Context by the state machine.
    context: Option<Context>,
    state: TaskState,
    state_machine_handle: Option<JoinHandle<()>>,
    // The transition channel allows serializing transitions handling,
    // preventing race conditions when transitions are run in //.
    transition_tx: Option<async_mpsc::Sender<TransitionRequest>>,
    prepare_abort_handle: Option<AbortHandle>,
    loop_abort_handle: Option<AbortHandle>,
    spawned_task_id: Option<TaskId>,
}

impl Default for TaskInner {
    fn default() -> Self {
        TaskInner {
            state_machine_context: Context::acquire("state_machine", 0).unwrap(),
            context: None,
            state: TaskState::Unprepared,
            state_machine_handle: None,
            transition_tx: None,
            prepare_abort_handle: None,
            loop_abort_handle: None,
            spawned_task_id: None,
        }
    }
}

impl TaskInner {
    fn switch_to_state(&mut self, target_state: TaskState, transition_req: TransitionRequest) {
        let res = Ok(TransitionStatus::Complete {
            origin: self.state,
            target: target_state,
        });

        self.state = target_state;
        transition_req.send_ack(res);
    }

    fn switch_to_err(&mut self, transition_req: TransitionRequest) {
        let res = Err(TransitionError {
            transition: transition_req.kind,
            state: self.state,
            err_msg: gst_error_msg!(
                gst::CoreError::StateChange,
                [
                    "Unrecoverable error for {:?} from state {:?}",
                    transition_req,
                    self.state,
                ]
            ),
        });

        self.state = TaskState::Error;
        transition_req.send_ack(res);
    }

    fn skip_transition(&mut self, transition_req: TransitionRequest) {
        let res = Ok(TransitionStatus::Skipped {
            transition: transition_req.kind,
            state: self.state,
        });

        transition_req.send_ack(res);
    }

    fn request_transition(
        &mut self,
        kind: Transition,
    ) -> Result<oneshot::Receiver<Result<TransitionStatus, TransitionError>>, TransitionError> {
        let transition_tx = self.transition_tx.as_mut().unwrap();

        let (transition_req, ack_rx) = TransitionRequest::new(kind);

        gst_log!(RUNTIME_CAT, "Pushing {:?}", transition_req);

        transition_tx.try_send(transition_req).or_else(|err| {
            let resource_err = if err.is_full() {
                gst::ResourceError::NoSpaceLeft
            } else {
                gst::ResourceError::Close
            };

            gst_warning!(RUNTIME_CAT, "Unable to send {:?}: {:?}", kind, err);
            Err(TransitionError {
                transition: kind,
                state: self.state,
                err_msg: gst_error_msg!(resource_err, ["Unable to send {:?}: {:?}", kind, err]),
            })
        })?;

        Ok(ack_rx)
    }
}

impl Drop for TaskInner {
    fn drop(&mut self) {
        if self.state != TaskState::Unprepared {
            // Don't panic here: in case another panic occurs, we would get
            // "panicked while panicking" which would prevents developers
            // from getting the initial panic message.
            gst_fixme!(RUNTIME_CAT, "Missing call to `Task::unprepare`");
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

    pub fn context(&self) -> Option<Context> {
        self.0.lock().unwrap().context.as_ref().cloned()
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
                gst_debug!(RUNTIME_CAT, "Task already {:?}", origin);
                return Ok(TransitionStatus::Skipped {
                    transition: Transition::Prepare,
                    state: origin,
                });
            }
            state => {
                gst_warning!(RUNTIME_CAT, "Attempt to prepare Task in state {:?}", state);
                return Err(TransitionError {
                    transition: Transition::Prepare,
                    state: inner.state,
                    err_msg: gst_error_msg!(
                        gst::CoreError::StateChange,
                        ["Attempt to prepare Task in state {:?}", state]
                    ),
                });
            }
        }

        assert!(inner.state_machine_handle.is_none());

        inner.state = TaskState::Preparing;

        gst_log!(RUNTIME_CAT, "Starting task state machine");

        // FIXME allow configuration of the channel buffer size,
        // this determines the contention on the Task.
        let (transition_tx, transition_rx) = async_mpsc::channel(4);
        let state_machine = StateMachine::new(Box::new(task_impl), transition_rx);
        let (transition_req, _) = TransitionRequest::new(Transition::Prepare);
        inner.state_machine_handle = Some(inner.state_machine_context.spawn(state_machine.run(
            Arc::clone(&self.0),
            context.clone(),
            transition_req,
        )));

        inner.transition_tx = Some(transition_tx);
        inner.context = Some(context);

        gst_log!(RUNTIME_CAT, "Task state machine started");

        Ok(TransitionStatus::Async {
            transition: Transition::Prepare,
            origin,
        })
    }

    pub fn unprepare(&self) -> Result<TransitionStatus, TransitionError> {
        let mut inner = self.0.lock().unwrap();

        let origin = inner.state;
        match origin {
            TaskState::Stopped | TaskState::Error | TaskState::Prepared | TaskState::Preparing => {
                gst_debug!(RUNTIME_CAT, "Unpreparing task");
            }
            TaskState::Unprepared | TaskState::Unpreparing => {
                gst_debug!(RUNTIME_CAT, "Task already {:?}", origin);
                return Ok(TransitionStatus::Skipped {
                    transition: Transition::Unprepare,
                    state: origin,
                });
            }
            state => {
                gst_warning!(
                    RUNTIME_CAT,
                    "Attempt to unprepare Task in state {:?}",
                    state
                );
                return Err(TransitionError {
                    transition: Transition::Unprepare,
                    state: inner.state,
                    err_msg: gst_error_msg!(
                        gst::CoreError::StateChange,
                        ["Attempt to unprepare Task in state {:?}", state]
                    ),
                });
            }
        }

        inner.state = TaskState::Unpreparing;

        if let Some(loop_abort_handle) = inner.loop_abort_handle.take() {
            loop_abort_handle.abort();
        }

        let _ = inner.request_transition(Transition::Unprepare).unwrap();
        let transition_tx = inner.transition_tx.take().unwrap();

        let state_machine_handle = inner.state_machine_handle.take();
        let context = inner.context.take().unwrap();

        if let Some(prepare_abort_handle) = inner.prepare_abort_handle.take() {
            prepare_abort_handle.abort();
        }

        drop(inner);

        match state_machine_handle {
            Some(state_machine_handle) => {
                gst_log!(
                    RUNTIME_CAT,
                    "Synchronously waiting for the state machine {:?}",
                    state_machine_handle,
                );
                let join_fut = block_on_or_add_sub_task(async {
                    state_machine_handle.await.unwrap();

                    drop(transition_tx);
                    drop(context);

                    gst_debug!(RUNTIME_CAT, "Task unprepared");
                });

                if join_fut.is_none() {
                    return Ok(TransitionStatus::Async {
                        transition: Transition::Unprepare,
                        origin,
                    });
                }
            }
            None => {
                drop(transition_tx);
                drop(context);
            }
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

        let ack_rx = inner.request_transition(Transition::Start)?;

        if let TaskState::Started = inner.state {
            return Ok(TransitionStatus::NotWaiting {
                transition: Transition::Start,
                origin: TaskState::Started,
            });
        }

        Self::await_ack(inner, ack_rx, Transition::Start)
    }

    /// Requests the `Task` loop to pause.
    ///
    /// If an iteration is in progress, it will run to completion,
    /// then no more iteration will be executed before `start` is called again.
    /// Therefore, it is not guaranteed that `Paused` is reached when `pause` returns.
    pub fn pause(&self) -> Result<TransitionStatus, TransitionError> {
        let mut inner = self.0.lock().unwrap();

        let ack_rx = inner.request_transition(Transition::Pause)?;

        if let TaskState::Started = inner.state {
            return Ok(TransitionStatus::NotWaiting {
                transition: Transition::Pause,
                origin: TaskState::Started,
            });
        }

        Self::await_ack(inner, ack_rx, Transition::Pause)
    }

    pub fn flush_start(&self) -> Result<TransitionStatus, TransitionError> {
        let mut inner = self.0.lock().unwrap();

        if let Some(loop_abort_handle) = inner.loop_abort_handle.take() {
            loop_abort_handle.abort();
        }

        Self::push_and_await_transition(inner, Transition::FlushStart)
    }

    pub fn flush_stop(&self) -> Result<TransitionStatus, TransitionError> {
        let mut inner = self.0.lock().unwrap();

        if let Some(loop_abort_handle) = inner.loop_abort_handle.take() {
            loop_abort_handle.abort();
        }

        Self::push_and_await_transition(inner, Transition::FlushStop)
    }

    /// Stops the `Started` `Task` and wait for it to finish.
    pub fn stop(&self) -> Result<TransitionStatus, TransitionError> {
        let mut inner = self.0.lock().unwrap();

        if let Some(loop_abort_handle) = inner.loop_abort_handle.take() {
            loop_abort_handle.abort();
        }

        Self::push_and_await_transition(inner, Transition::Stop)
    }

    fn push_and_await_transition(
        mut inner: MutexGuard<TaskInner>,
        transition: Transition,
    ) -> Result<TransitionStatus, TransitionError> {
        let ack_rx = inner.request_transition(transition)?;

        Self::await_ack(inner, ack_rx, transition)
    }

    fn await_ack(
        inner: MutexGuard<TaskInner>,
        ack_rx: oneshot::Receiver<Result<TransitionStatus, TransitionError>>,
        transition: Transition,
    ) -> Result<TransitionStatus, TransitionError> {
        let origin = inner.state;

        // Since transition handling is serialized by the state machine and
        // we hold a lock on TaskInner, we can verify if current spawned loop / tansition hook
        // task_id matches the task_id of current subtask, if any.
        if let Some(spawned_task_id) = inner.spawned_task_id {
            if let Some((cur_context, cur_task_id)) = Context::current_task() {
                if cur_task_id == spawned_task_id && &cur_context == inner.context.as_ref().unwrap()
                {
                    // Don't block as this would deadlock
                    gst_log!(
                        RUNTIME_CAT,
                        "Requested {:?} from loop or transition hook, not waiting",
                        transition,
                    );
                    return Ok(TransitionStatus::NotWaiting { transition, origin });
                }
            }
        }

        drop(inner);

        block_on_or_add_sub_task(async move {
            gst_trace!(RUNTIME_CAT, "Awaiting ack for {:?}", transition);

            let res = ack_rx.await.unwrap();
            if res.is_ok() {
                gst_log!(RUNTIME_CAT, "Received ack {:?} for {:?}", res, transition);
            } else {
                gst_error!(RUNTIME_CAT, "Received ack {:?} for {:?}", res, transition);
            }

            res
        })
        .unwrap_or_else(|| {
            // Future was spawned as a subtask
            Ok(TransitionStatus::Async { transition, origin })
        })
    }
}

struct StateMachine {
    task_impl: Box<dyn TaskImpl>,
    transition_rx: async_mpsc::Receiver<TransitionRequest>,
    pending_transition: Option<TransitionRequest>,
}

// Make sure the Context doesn't throttle otherwise we end up  with long delays
// executing transition in a pipeline with many elements. This is because pipeline
// serializes the transitions and the Context's scheduler gets a chance to reach its
// throttling state between 2 elements.

macro_rules! exec_hook {
    ($self:ident, $hook:ident, $transition_req:expr, $origin:expr, $task_inner:expr, $context:expr) => {{
        let transition = $transition_req.kind;

        let hook_fut = async move {
            let mut res = $self.task_impl.$hook().await;

            if res.is_ok() {
                while Context::current_has_sub_tasks() {
                    gst_trace!(RUNTIME_CAT, "Draining subtasks for {}", stringify!($hook));
                    res = Context::drain_sub_tasks().await.map_err(|err| {
                        let msg = format!("{} subtask returned {:?}", stringify!($hook), err);
                        gst_log!(RUNTIME_CAT, "{}", &msg);
                        gst_error_msg!(gst::CoreError::StateChange, ["{}", &msg])
                    });

                    if res.is_err() {
                        break;
                    }
                }
            }

            let res = match res {
                Ok(()) => Ok(()),
                Err(err) => {
                    let next_transition_req = $self
                        .task_impl
                        .handle_hook_error(transition, $origin, err)
                        .await;
                    Err(next_transition_req)
                }
            };

            ($self, res)
        };

        let join_handle = {
            let mut task_inner = $task_inner.lock().unwrap();
            let join_handle = $context.awake_and_spawn(hook_fut);
            task_inner.spawned_task_id = Some(join_handle.task_id());

            join_handle
        };

        let (this, res) = join_handle.map(|res| res.unwrap()).await;
        $self = this;

        match res {
            Ok(()) => Ok($transition_req),
            Err(next_transition_req) => {
                // Convert transition according to the error handler's decision
                gst_trace!(
                    RUNTIME_CAT,
                    "TaskImpl hook error: converting {:?} to {:?}",
                    $transition_req.kind,
                    next_transition_req,
                );

                $transition_req.kind = next_transition_req;
                $self.pending_transition = Some($transition_req);

                Err(())
            }
        }
    }};
}

impl StateMachine {
    // Use dynamic dispatch for TaskImpl as it reduces memory usage compared to monomorphization
    // without inducing any significant performance penalties.
    fn new(
        task_impl: Box<dyn TaskImpl>,
        transition_rx: async_mpsc::Receiver<TransitionRequest>,
    ) -> Self {
        StateMachine {
            task_impl,
            transition_rx,
            pending_transition: None,
        }
    }

    async fn run(
        mut self,
        task_inner: Arc<Mutex<TaskInner>>,
        context: Context,
        mut transition_req: TransitionRequest,
    ) {
        gst_trace!(RUNTIME_CAT, "Preparing task");

        {
            let res = exec_hook!(
                self,
                prepare,
                transition_req,
                TaskState::Preparing,
                &task_inner,
                &context
            );
            if let Ok(transition_req) = res {
                task_inner
                    .lock()
                    .unwrap()
                    .switch_to_state(TaskState::Prepared, transition_req);
                gst_trace!(RUNTIME_CAT, "Task Prepared");
            }
        }

        loop {
            let mut transition_req = match self.pending_transition.take() {
                Some(pending_transition_req) => pending_transition_req,
                None => self
                    .transition_rx
                    .next()
                    .await
                    .expect("transition_rx dropped"),
            };

            gst_trace!(RUNTIME_CAT, "State machine popped {:?}", transition_req);

            match transition_req.kind {
                Transition::Error => {
                    let mut task_inner = task_inner.lock().unwrap();
                    task_inner.switch_to_err(transition_req);
                    gst_trace!(RUNTIME_CAT, "Switched to Error");
                }
                Transition::Start => {
                    let origin = {
                        let mut task_inner = task_inner.lock().unwrap();
                        let origin = task_inner.state;
                        match origin {
                            TaskState::Stopped | TaskState::Paused | TaskState::Prepared => (),
                            TaskState::PausedFlushing => {
                                task_inner.switch_to_state(TaskState::Flushing, transition_req);
                                gst_trace!(RUNTIME_CAT, "Switched from PausedFlushing to Flushing");
                                continue;
                            }
                            TaskState::Error => {
                                transition_req.send_err_ack();
                                continue;
                            }
                            state => {
                                task_inner.skip_transition(transition_req);
                                gst_trace!(RUNTIME_CAT, "Skipped Start in state {:?}", state);
                                continue;
                            }
                        }

                        origin
                    };

                    self =
                        Self::spawn_loop(self, transition_req, origin, &task_inner, &context).await;
                    // next/pending transition handled in next iteration
                }
                Transition::Pause => {
                    let (origin, target) = {
                        let mut task_inner = task_inner.lock().unwrap();
                        let origin = task_inner.state;
                        match origin {
                            TaskState::Started | TaskState::Stopped | TaskState::Prepared => {
                                (origin, TaskState::Paused)
                            }
                            TaskState::Flushing => (origin, TaskState::PausedFlushing),
                            TaskState::Error => {
                                transition_req.send_err_ack();
                                continue;
                            }
                            state => {
                                task_inner.skip_transition(transition_req);
                                gst_trace!(RUNTIME_CAT, "Skipped Pause in state {:?}", state);
                                continue;
                            }
                        }
                    };

                    let res =
                        exec_hook!(self, pause, transition_req, origin, &task_inner, &context);
                    if let Ok(transition_req) = res {
                        task_inner
                            .lock()
                            .unwrap()
                            .switch_to_state(target, transition_req);
                        gst_trace!(RUNTIME_CAT, "Task loop {:?}", target);
                    }
                }
                Transition::Stop => {
                    let origin = {
                        let mut task_inner = task_inner.lock().unwrap();
                        let origin = task_inner.state;
                        match origin {
                            TaskState::Started
                            | TaskState::Paused
                            | TaskState::PausedFlushing
                            | TaskState::Flushing => origin,
                            TaskState::Error => {
                                transition_req.send_err_ack();
                                continue;
                            }
                            state => {
                                task_inner.skip_transition(transition_req);
                                gst_trace!(RUNTIME_CAT, "Skipped Stop in state {:?}", state);
                                continue;
                            }
                        }
                    };

                    let res = exec_hook!(self, stop, transition_req, origin, &task_inner, &context);
                    if let Ok(transition_req) = res {
                        task_inner
                            .lock()
                            .unwrap()
                            .switch_to_state(TaskState::Stopped, transition_req);
                        gst_trace!(RUNTIME_CAT, "Task loop Stopped");
                    }
                }
                Transition::FlushStart => {
                    let (origin, target) = {
                        let mut task_inner = task_inner.lock().unwrap();
                        let origin = task_inner.state;
                        match origin {
                            TaskState::Started => (origin, TaskState::Flushing),
                            TaskState::Paused => (origin, TaskState::PausedFlushing),
                            TaskState::Error => {
                                transition_req.send_err_ack();
                                continue;
                            }
                            state => {
                                task_inner.skip_transition(transition_req);
                                gst_trace!(RUNTIME_CAT, "Skipped FlushStart in state {:?}", state);
                                continue;
                            }
                        }
                    };

                    let res = exec_hook!(
                        self,
                        flush_start,
                        transition_req,
                        origin,
                        &task_inner,
                        &context
                    );
                    if let Ok(transition_req) = res {
                        task_inner
                            .lock()
                            .unwrap()
                            .switch_to_state(target, transition_req);
                        gst_trace!(RUNTIME_CAT, "Task {:?}", target);
                    }
                }
                Transition::FlushStop => {
                    let origin = task_inner.lock().unwrap().state;
                    let is_paused = match origin {
                        TaskState::Flushing => false,
                        TaskState::PausedFlushing => true,
                        TaskState::Error => {
                            transition_req.send_err_ack();
                            continue;
                        }
                        state => {
                            task_inner.lock().unwrap().skip_transition(transition_req);
                            gst_trace!(RUNTIME_CAT, "Skipped FlushStop in state {:?}", state);
                            continue;
                        }
                    };

                    let res = exec_hook!(
                        self,
                        flush_stop,
                        transition_req,
                        origin,
                        &task_inner,
                        &context
                    );
                    if let Ok(transition_req) = res {
                        if is_paused {
                            task_inner
                                .lock()
                                .unwrap()
                                .switch_to_state(TaskState::Paused, transition_req);
                            gst_trace!(RUNTIME_CAT, "Switched from PausedFlushing to Paused");
                        } else {
                            self = Self::spawn_loop(
                                self,
                                transition_req,
                                origin,
                                &task_inner,
                                &context,
                            )
                            .await;
                            // next/pending transition handled in next iteration
                        }
                    }
                }
                Transition::Unprepare => {
                    // Unprepare is not joined by an ack_rx but by joining the state machine
                    // handle, so we don't need to keep track of the spwaned_task_id
                    context
                        .awake_and_spawn(async move {
                            self.task_impl.unprepare().await;

                            while Context::current_has_sub_tasks() {
                                gst_trace!(RUNTIME_CAT, "Draining subtasks for unprepare");
                                let res = Context::drain_sub_tasks().await.map_err(|err| {
                                    gst_log!(RUNTIME_CAT, "unprepare subtask returned {:?}", err);
                                    err
                                });
                                if res.is_err() {
                                    break;
                                }
                            }
                        })
                        .await
                        .unwrap();

                    task_inner
                        .lock()
                        .unwrap()
                        .switch_to_state(TaskState::Unprepared, transition_req);

                    break;
                }
                _ => unreachable!("State machine handler {:?}", transition_req),
            }
        }

        gst_trace!(RUNTIME_CAT, "Task state machine terminated");
    }

    async fn spawn_loop(
        mut self,
        mut transition_req: TransitionRequest,
        origin: TaskState,
        task_inner: &Arc<Mutex<TaskInner>>,
        context: &Context,
    ) -> Self {
        let task_inner_clone = Arc::clone(&task_inner);
        let loop_fut = async move {
            let mut res = self.task_impl.start().await;
            if res.is_ok() {
                while Context::current_has_sub_tasks() {
                    gst_trace!(RUNTIME_CAT, "Draining subtasks for start");
                    res = Context::drain_sub_tasks().await.map_err(|err| {
                        let msg = format!("start subtask returned {:?}", err);
                        gst_log!(RUNTIME_CAT, "{}", &msg);
                        gst_error_msg!(gst::CoreError::StateChange, ["{}", &msg])
                    });

                    if res.is_err() {
                        break;
                    }
                }
            }

            match res {
                Ok(()) => {
                    let abortable_task_loop = {
                        let (abortable_task_loop, loop_abort_handle) =
                            abortable(self.run_loop(Arc::clone(&task_inner_clone)));

                        let mut task_inner = task_inner_clone.lock().unwrap();
                        task_inner.loop_abort_handle = Some(loop_abort_handle);
                        task_inner.switch_to_state(TaskState::Started, transition_req);

                        abortable_task_loop
                    };

                    gst_trace!(RUNTIME_CAT, "Starting task loop");
                    match abortable_task_loop.await {
                        Ok(Ok(())) => (),
                        Ok(Err(err)) => {
                            let next_transition = self.task_impl.handle_iterate_error(err).await;
                            let (transition_req, _) = TransitionRequest::new(next_transition);
                            self.pending_transition = Some(transition_req);
                        }
                        Err(Aborted) => gst_trace!(RUNTIME_CAT, "Task loop aborted"),
                    }
                }
                Err(err) => {
                    // Error while executing start hook
                    let next_transition = self
                        .task_impl
                        .handle_hook_error(transition_req.kind, origin, err)
                        .await;

                    gst_log!(
                        RUNTIME_CAT,
                        "TaskImpl hook error: converting Start to {:?}",
                        next_transition,
                    );

                    transition_req.kind = next_transition;
                    self.pending_transition = Some(transition_req);
                }
            }

            // next/pending transition handled in state machine loop

            self
        };

        let join_handle = {
            let mut task_inner = task_inner.lock().unwrap();
            let join_handle = context.awake_and_spawn(loop_fut);
            task_inner.spawned_task_id = Some(join_handle.task_id());

            join_handle
        };

        join_handle.map(|res| res.unwrap()).await
    }

    async fn run_loop(&mut self, task_inner: Arc<Mutex<TaskInner>>) -> Result<(), gst::FlowError> {
        gst_trace!(RUNTIME_CAT, "Task loop started");

        loop {
            // Check if there is any pending transition_req
            while let Ok(Some(transition_req)) = self.transition_rx.try_next() {
                gst_trace!(RUNTIME_CAT, "Task loop popped {:?}", transition_req);

                match transition_req.kind {
                    Transition::Start => {
                        task_inner.lock().unwrap().skip_transition(transition_req);
                        gst_trace!(RUNTIME_CAT, "Skipped Start in state Started");
                    }
                    _ => {
                        gst_trace!(
                            RUNTIME_CAT,
                            "Task loop handing {:?} to state machine",
                            transition_req,
                        );
                        self.pending_transition = Some(transition_req);
                        return Ok(());
                    }
                }
            }

            // Run the iteration
            self.task_impl.iterate().await.map_err(|err| {
                gst_log!(RUNTIME_CAT, "Task loop iterate impl returned {:?}", err);
                err
            })?;

            while Context::current_has_sub_tasks() {
                gst_trace!(RUNTIME_CAT, "Draining subtasks for {}", stringify!($hook));
                Context::drain_sub_tasks().await.map_err(|err| {
                    gst_log!(RUNTIME_CAT, "Task loop iterate subtask returned {:?}", err);
                    err
                })?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::{mpsc, oneshot};
    use std::time::Duration;

    use crate::runtime::Context;

    use super::*;

    #[tokio::test]
    async fn iterate() {
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
                    gst_debug!(RUNTIME_CAT, "task_iterate: prepared");
                    self.prepared_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "task_iterate: started");
                    self.started_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "task_iterate: entering iterate");
                    self.iterate_sender.send(()).await.unwrap();

                    gst_debug!(
                        RUNTIME_CAT,
                        "task_iterate: awaiting complete_iterate_receiver"
                    );

                    let res = self.complete_iterate_receiver.next().await.unwrap();
                    if res.is_ok() {
                        gst_debug!(RUNTIME_CAT, "task_iterate: received Ok => keep looping");
                    } else {
                        gst_debug!(
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
                    gst_debug!(RUNTIME_CAT, "task_iterate: paused");
                    self.paused_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "task_iterate: stopped");
                    self.stopped_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "task_iterate: stopped");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn unprepare(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "task_iterate: unprepared");
                    self.unprepared_sender.send(()).await.unwrap();
                }
                .boxed()
            }
        }

        let context = Context::acquire("task_iterate", 2).unwrap();

        let task = Task::default();

        assert_eq!(task.state(), TaskState::Unprepared);

        gst_debug!(RUNTIME_CAT, "task_iterate: preparing");

        let (prepared_sender, mut prepared_receiver) = mpsc::channel(1);
        let (started_sender, mut started_receiver) = mpsc::channel(1);
        let (iterate_sender, mut iterate_receiver) = mpsc::channel(1);
        let (mut complete_iterate_sender, complete_iterate_receiver) = mpsc::channel(1);
        let (paused_sender, mut paused_receiver) = mpsc::channel(1);
        let (stopped_sender, mut stopped_receiver) = mpsc::channel(1);
        let (flush_start_sender, mut flush_start_receiver) = mpsc::channel(1);
        let (unprepared_sender, mut unprepared_receiver) = mpsc::channel(1);
        let res = task
            .prepare(
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
            )
            .unwrap();
        assert_eq!(
            res,
            TransitionStatus::Async {
                transition: Transition::Prepare,
                origin: TaskState::Unprepared,
            }
        );

        gst_debug!(RUNTIME_CAT, "task_iterate: starting (initial)");
        assert_eq!(
            task.start().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Prepared,
                target: TaskState::Started
            },
        );

        assert_eq!(task.state(), TaskState::Started);
        // At this point, prepared must be completed
        prepared_receiver.next().await.unwrap();
        // ... and start executed
        started_receiver.next().await.unwrap();

        assert_eq!(task.state(), TaskState::Started);

        // unlock task loop and keep looping
        iterate_receiver.next().await.unwrap();
        complete_iterate_sender.send(Ok(())).await.unwrap();

        gst_debug!(RUNTIME_CAT, "task_iterate: starting (redundant)");
        // start will return immediately
        assert_eq!(
            task.start().unwrap(),
            TransitionStatus::NotWaiting {
                transition: Transition::Start,
                origin: TaskState::Started,
            },
        );
        assert_eq!(task.state(), TaskState::Started);

        gst_debug!(RUNTIME_CAT, "task_iterate: pause (initial)");
        assert_eq!(
            task.pause().unwrap(),
            TransitionStatus::NotWaiting {
                transition: Transition::Pause,
                origin: TaskState::Started,
            },
        );

        // Pause transition is asynchronous
        while TaskState::Paused != task.state() {
            tokio::time::delay_for(Duration::from_millis(2)).await;

            if let Ok(Some(())) = iterate_receiver.try_next() {
                // unlock iteration
                complete_iterate_sender.send(Ok(())).await.unwrap();
            }
        }

        gst_debug!(RUNTIME_CAT, "task_iterate: awaiting pause hook ack");
        paused_receiver.next().await.unwrap();

        gst_debug!(RUNTIME_CAT, "task_iterate: starting (after pause)");
        assert_eq!(
            task.start().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Paused,
                target: TaskState::Started
            },
        );

        assert_eq!(task.state(), TaskState::Started);
        // Paused -> Started
        let _ = started_receiver.next().await;

        gst_debug!(RUNTIME_CAT, "task_iterate: stopping");
        assert_eq!(
            task.stop().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Started,
                target: TaskState::Stopped
            },
        );

        assert_eq!(task.state(), TaskState::Stopped);
        let _ = stopped_receiver.next().await;

        // purge remaining iteration received before stop if any
        let _ = iterate_receiver.try_next();

        gst_debug!(RUNTIME_CAT, "task_iterate: starting (after stop)");
        assert_eq!(
            task.start().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Stopped,
                target: TaskState::Started
            },
        );
        let _ = started_receiver.next().await;

        gst_debug!(RUNTIME_CAT, "task_iterate: req. iterate to return Eos");
        iterate_receiver.next().await.unwrap();
        complete_iterate_sender
            .send(Err(gst::FlowError::Eos))
            .await
            .unwrap();

        gst_debug!(RUNTIME_CAT, "task_iterate: awaiting stop hook ack");
        stopped_receiver.next().await.unwrap();

        // Wait for state machine to reach Stopped
        while TaskState::Stopped != task.state() {
            tokio::time::delay_for(Duration::from_millis(2)).await;
        }

        gst_debug!(RUNTIME_CAT, "task_iterate: starting (after stop)");
        assert_eq!(
            task.start().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Stopped,
                target: TaskState::Started
            },
        );
        let _ = started_receiver.next().await;

        gst_debug!(RUNTIME_CAT, "task_iterate: req. iterate to return Flushing");
        iterate_receiver.next().await.unwrap();
        complete_iterate_sender
            .send(Err(gst::FlowError::Flushing))
            .await
            .unwrap();

        gst_debug!(RUNTIME_CAT, "task_iterate: awaiting flush_start hook ack");
        flush_start_receiver.next().await.unwrap();

        // Wait for state machine to reach Flushing
        while TaskState::Flushing != task.state() {
            tokio::time::delay_for(Duration::from_millis(2)).await;
        }

        gst_debug!(RUNTIME_CAT, "task_iterate: stop flushing");
        assert_eq!(
            task.flush_stop().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Flushing,
                target: TaskState::Started
            },
        );
        let _ = started_receiver.next().await;

        gst_debug!(RUNTIME_CAT, "task_iterate: req. iterate to return Error");
        iterate_receiver.next().await.unwrap();
        complete_iterate_sender
            .send(Err(gst::FlowError::Error))
            .await
            .unwrap();

        // Wait for state machine to reach Error
        while TaskState::Error != task.state() {
            tokio::time::delay_for(Duration::from_millis(2)).await;
        }

        gst_debug!(
            RUNTIME_CAT,
            "task_iterate: attempting to start (after Error)"
        );
        let err = task.start().unwrap_err();
        match err {
            TransitionError {
                transition: Transition::Start,
                state: TaskState::Error,
                ..
            } => (),
            other => unreachable!(other),
        }

        assert_eq!(
            task.unprepare().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Error,
                target: TaskState::Unprepared
            },
        );

        assert_eq!(task.state(), TaskState::Unprepared);
        let _ = unprepared_receiver.next().await;
    }

    #[tokio::test]
    async fn prepare_error() {
        gst::init().unwrap();

        struct TaskPrepareTest {
            prepare_error_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskPrepareTest {
            fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "prepare_error: prepare returning an error");
                    Err(gst_error_msg!(
                        gst::ResourceError::Failed,
                        ["prepare_error: intentional error"]
                    ))
                }
                .boxed()
            }

            fn handle_hook_error(
                &mut self,
                transition: Transition,
                state: TaskState,
                err: gst::ErrorMessage,
            ) -> BoxFuture<'_, Transition> {
                async move {
                    gst_debug!(
                        RUNTIME_CAT,
                        "prepare_error: handling prepare error {:?}",
                        err
                    );
                    match (transition, state) {
                        (Transition::Prepare, TaskState::Preparing) => {
                            self.prepare_error_sender.send(()).await.unwrap();
                        }
                        other => unreachable!("{:?}", other),
                    }
                    Transition::Error
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::ok(()).boxed()
            }
        }

        let context = Context::acquire("prepare_error", 2).unwrap();

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

        gst_debug!(RUNTIME_CAT, "prepare_error: await error hook notification");
        prepare_error_receiver.next().await.unwrap();

        // Wait for state machine to reach Error
        while TaskState::Error != task.state() {
            tokio::time::delay_for(Duration::from_millis(2)).await;
        }

        let res = task.start().unwrap_err();
        match res {
            TransitionError {
                transition: Transition::Start,
                state: TaskState::Error,
                ..
            } => (),
            other => unreachable!("{:?}", other),
        }

        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn prepare_start_ok() {
        // Hold the preparation function so that it completes after the start request is engaged

        gst::init().unwrap();

        struct TaskPrepareTest {
            prepare_receiver: mpsc::Receiver<()>,
        }

        impl TaskImpl for TaskPrepareTest {
            fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(
                        RUNTIME_CAT,
                        "prepare_start_ok: preparation awaiting trigger"
                    );
                    self.prepare_receiver.next().await.unwrap();
                    gst_debug!(RUNTIME_CAT, "prepare_start_ok: preparation complete Ok");
                    Ok(())
                }
                .boxed()
            }

            fn handle_hook_error(
                &mut self,
                _transition: Transition,
                _state: TaskState,
                _err: gst::ErrorMessage,
            ) -> BoxFuture<'_, Transition> {
                unreachable!("prepare_start_ok: handle_prepare_error");
            }

            fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "prepare_start_ok: started");
                    Ok(())
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::pending::<Result<(), gst::FlowError>>().boxed()
            }
        }

        let context = Context::acquire("prepare_start_ok", 2).unwrap();

        let task = Task::default();

        let (mut prepare_sender, prepare_receiver) = mpsc::channel(1);
        task.prepare(TaskPrepareTest { prepare_receiver }, context.clone())
            .unwrap();

        let start_ctx = Context::acquire("prepare_start_ok_requester", 0).unwrap();
        let task_clone = task.clone();
        let (ready_sender, ready_receiver) = oneshot::channel();
        let start_handle = start_ctx.spawn(async move {
            assert_eq!(task_clone.state(), TaskState::Preparing);
            gst_debug!(RUNTIME_CAT, "prepare_start_ok: starting");
            assert_eq!(
                task_clone.start().unwrap(),
                TransitionStatus::Async {
                    transition: Transition::Start,
                    origin: TaskState::Preparing,
                }
            );
            ready_sender.send(()).unwrap();
            Context::drain_sub_tasks().await.unwrap();
            assert_eq!(task_clone.state(), TaskState::Started);

            assert_eq!(
                task.stop().unwrap(),
                TransitionStatus::Async {
                    transition: Transition::Stop,
                    origin: TaskState::Started,
                },
            );
            Context::drain_sub_tasks().await.unwrap();
            assert_eq!(task_clone.state(), TaskState::Stopped);

            assert_eq!(
                task.unprepare().unwrap(),
                TransitionStatus::Async {
                    transition: Transition::Unprepare,
                    origin: TaskState::Stopped,
                },
            );
            Context::drain_sub_tasks().await.unwrap();
            assert_eq!(task_clone.state(), TaskState::Unprepared);
        });

        gst_debug!(RUNTIME_CAT, "prepare_start_ok: awaiting for start_ctx");
        ready_receiver.await.unwrap();

        gst_debug!(RUNTIME_CAT, "prepare_start_ok: triggering preparation");
        prepare_sender.send(()).await.unwrap();

        start_handle.await.unwrap();
    }

    #[tokio::test]
    async fn prepare_start_error() {
        // Hold the preparation function so that it completes after the start request is engaged

        gst::init().unwrap();

        struct TaskPrepareTest {
            prepare_receiver: mpsc::Receiver<()>,
            prepare_error_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskPrepareTest {
            fn prepare(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(
                        RUNTIME_CAT,
                        "prepare_start_error: preparation awaiting trigger"
                    );
                    self.prepare_receiver.next().await.unwrap();
                    gst_debug!(RUNTIME_CAT, "prepare_start_error: preparation complete Err");

                    Err(gst_error_msg!(
                        gst::ResourceError::Failed,
                        ["prepare_start_error: intentional error"]
                    ))
                }
                .boxed()
            }

            fn handle_hook_error(
                &mut self,
                transition: Transition,
                state: TaskState,
                err: gst::ErrorMessage,
            ) -> BoxFuture<'_, Transition> {
                async move {
                    gst_debug!(
                        RUNTIME_CAT,
                        "prepare_start_error: handling prepare error {:?}",
                        err
                    );
                    match (transition, state) {
                        (Transition::Prepare, TaskState::Preparing) => {
                            self.prepare_error_sender.send(()).await.unwrap();
                        }
                        other => unreachable!("hook error for {:?}", other),
                    }
                    Transition::Error
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

        let context = Context::acquire("prepare_start_error", 2).unwrap();

        let task = Task::default();

        let (mut prepare_sender, prepare_receiver) = mpsc::channel(1);
        let (prepare_error_sender, mut prepare_error_receiver) = mpsc::channel(1);
        task.prepare(
            TaskPrepareTest {
                prepare_receiver,
                prepare_error_sender,
            },
            context,
        )
        .unwrap();

        let start_ctx = Context::acquire("prepare_start_error_requester", 0).unwrap();
        let task_clone = task.clone();
        let (ready_sender, ready_receiver) = oneshot::channel();
        let start_handle = start_ctx.spawn(async move {
            assert_eq!(task_clone.state(), TaskState::Preparing);
            gst_debug!(RUNTIME_CAT, "prepare_start_error: starting (Err)");
            task_clone.start().unwrap();
            ready_sender.send(()).unwrap();
            Context::drain_sub_tasks().await.unwrap();

            assert_eq!(
                task.unprepare().unwrap(),
                TransitionStatus::Async {
                    transition: Transition::Unprepare,
                    origin: TaskState::Error,
                },
            );
            Context::drain_sub_tasks().await.unwrap();
        });

        gst_debug!(RUNTIME_CAT, "prepare_start_error: awaiting for start_ctx");
        ready_receiver.await.unwrap();

        gst_debug!(
            RUNTIME_CAT,
            "prepare_start_error: triggering preparation (failure)"
        );
        prepare_sender.send(()).await.unwrap();

        gst_debug!(
            RUNTIME_CAT,
            "prepare_start_error: await prepare error notification"
        );
        prepare_error_receiver.next().await.unwrap();

        start_handle.await.unwrap();
    }

    #[tokio::test]
    async fn pause_start() {
        gst::init().unwrap();

        struct TaskPauseStartTest {
            iterate_sender: mpsc::Sender<()>,
            complete_receiver: mpsc::Receiver<()>,
            paused_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskPauseStartTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_start: entering iteration");
                    self.iterate_sender.send(()).await.unwrap();

                    gst_debug!(RUNTIME_CAT, "pause_start: iteration awaiting completion");
                    self.complete_receiver.next().await.unwrap();
                    gst_debug!(RUNTIME_CAT, "pause_start: iteration complete");

                    Ok(())
                }
                .boxed()
            }

            fn pause(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_start: paused");
                    self.paused_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("pause_start", 2).unwrap();

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

        gst_debug!(RUNTIME_CAT, "pause_start: starting");
        assert_eq!(
            task.start().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Prepared,
                target: TaskState::Started,
            },
        );
        assert_eq!(task.state(), TaskState::Started);

        gst_debug!(RUNTIME_CAT, "pause_start: awaiting 1st iteration");
        iterate_receiver.next().await.unwrap();

        gst_debug!(RUNTIME_CAT, "pause_start: pausing (1)");
        assert_eq!(
            task.pause().unwrap(),
            TransitionStatus::NotWaiting {
                transition: Transition::Pause,
                origin: TaskState::Started,
            },
        );

        gst_debug!(RUNTIME_CAT, "pause_start: sending 1st iteration completion");
        complete_sender.try_send(()).unwrap();

        // Pause transition is asynchronous
        while TaskState::Paused != task.state() {
            tokio::time::delay_for(Duration::from_millis(5)).await;
        }

        gst_debug!(RUNTIME_CAT, "pause_start: awaiting paused");
        let _ = paused_receiver.next().await;

        // Loop held on due to Pause
        iterate_receiver.try_next().unwrap_err();

        assert_eq!(
            task.start().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Paused,
                target: TaskState::Started,
            },
        );
        assert_eq!(task.state(), TaskState::Started);

        gst_debug!(RUNTIME_CAT, "pause_start: awaiting 2d iteration");
        iterate_receiver.next().await.unwrap();

        gst_debug!(RUNTIME_CAT, "pause_start: sending 2d iteration completion");
        complete_sender.try_send(()).unwrap();

        task.stop().unwrap();
        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn successive_pause_start() {
        // Purpose: check pause cancellation.
        gst::init().unwrap();

        struct TaskPauseStartTest {
            iterate_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskPauseStartTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "successive_pause_start: iteration");
                    self.iterate_sender.send(()).await.unwrap();

                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("successive_pause_start", 2).unwrap();

        let task = Task::default();

        let (iterate_sender, mut iterate_receiver) = mpsc::channel(1);
        task.prepare(TaskPauseStartTest { iterate_sender }, context)
            .unwrap();

        gst_debug!(RUNTIME_CAT, "successive_pause_start: starting");
        task.start().unwrap();

        gst_debug!(RUNTIME_CAT, "successive_pause_start: awaiting iteration 1");
        iterate_receiver.next().await.unwrap();

        gst_debug!(RUNTIME_CAT, "successive_pause_start: pause and start");
        task.pause().unwrap();
        task.start().unwrap();

        assert_eq!(task.state(), TaskState::Started);

        gst_debug!(RUNTIME_CAT, "successive_pause_start: awaiting iteration 2");
        iterate_receiver.next().await.unwrap();

        gst_debug!(RUNTIME_CAT, "successive_pause_start: stopping");
        task.stop().unwrap();
        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn flush_regular_sync() {
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
                    gst_debug!(RUNTIME_CAT, "flush_regular_sync: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "flush_regular_sync: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("flush_regular_sync", 2).unwrap();

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

        gst_debug!(RUNTIME_CAT, "flush_regular_sync: start");
        task.start().unwrap();

        gst_debug!(RUNTIME_CAT, "flush_regular_sync: starting flush");
        assert_eq!(
            task.flush_start().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Started,
                target: TaskState::Flushing,
            },
        );
        assert_eq!(task.state(), TaskState::Flushing);

        flush_start_receiver.next().await.unwrap();

        gst_debug!(RUNTIME_CAT, "flush_regular_sync: stopping flush");
        assert_eq!(
            task.flush_stop().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Flushing,
                target: TaskState::Started,
            },
        );
        assert_eq!(task.state(), TaskState::Started);

        flush_stop_receiver.next().await.unwrap();

        task.pause().unwrap();
        task.stop().unwrap();
        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn flush_regular_different_context() {
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
                    gst_debug!(
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
                    gst_debug!(
                        RUNTIME_CAT,
                        "flush_regular_different_context: stopped flushing"
                    );
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("flush_regular_different_context", 2).unwrap();

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

        gst_debug!(RUNTIME_CAT, "flush_regular_different_context: start");
        task.start().unwrap();

        let oob_context = Context::acquire("flush_regular_different_context_oob", 2).unwrap();

        let task_clone = task.clone();
        let flush_handle = oob_context.spawn(async move {
            assert_eq!(
                task_clone.flush_start().unwrap(),
                TransitionStatus::Async {
                    transition: Transition::FlushStart,
                    origin: TaskState::Started,
                },
            );
            Context::drain_sub_tasks().await.unwrap();
            assert_eq!(task_clone.state(), TaskState::Flushing);
            flush_start_receiver.next().await.unwrap();

            assert_eq!(
                task_clone.flush_stop().unwrap(),
                TransitionStatus::Async {
                    transition: Transition::FlushStop,
                    origin: TaskState::Flushing,
                },
            );
            Context::drain_sub_tasks().await.unwrap();
            assert_eq!(task_clone.state(), TaskState::Started);
        });

        flush_handle.await.unwrap();
        flush_stop_receiver.next().await.unwrap();

        task.stop().unwrap();
        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn flush_regular_same_context() {
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
                    gst_debug!(RUNTIME_CAT, "flush_regular_same_context: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "flush_regular_same_context: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("flush_regular_same_context", 2).unwrap();

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

        task.start().unwrap();

        let task_clone = task.clone();
        let flush_handle = task.context().as_ref().unwrap().spawn(async move {
            assert_eq!(
                task_clone.flush_start().unwrap(),
                TransitionStatus::Async {
                    transition: Transition::FlushStart,
                    origin: TaskState::Started,
                },
            );
            Context::drain_sub_tasks().await.unwrap();
            assert_eq!(task_clone.state(), TaskState::Flushing);
            flush_start_receiver.next().await.unwrap();

            assert_eq!(
                task_clone.flush_stop().unwrap(),
                TransitionStatus::Async {
                    transition: Transition::FlushStop,
                    origin: TaskState::Flushing,
                },
            );
            Context::drain_sub_tasks().await.unwrap();
            assert_eq!(task_clone.state(), TaskState::Started);
        });

        flush_handle.await.unwrap();
        flush_stop_receiver.next().await.unwrap();

        task.stop().unwrap();
        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn flush_from_loop() {
        // Purpose: make sure a flush_start transition triggered from an iteration doesn't block.
        gst::init().unwrap();

        struct TaskFlushTest {
            task: Task,
            flush_start_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskFlushTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "flush_from_loop: flush_start from iteration");
                    assert_eq!(
                        self.task.flush_start().unwrap(),
                        TransitionStatus::NotWaiting {
                            transition: Transition::FlushStart,
                            origin: TaskState::Started,
                        },
                    );
                    Ok(())
                }
                .boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "flush_from_loop: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("flush_from_loop", 2).unwrap();

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

        gst_debug!(
            RUNTIME_CAT,
            "flush_from_loop: awaiting flush_start notification"
        );
        flush_start_receiver.next().await.unwrap();

        assert_eq!(
            task.stop().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Flushing,
                target: TaskState::Stopped,
            },
        );
        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn pause_from_loop() {
        // Purpose: make sure a start transition triggered from an iteration doesn't block.
        // E.g. an auto pause cancellation after a delay.
        gst::init().unwrap();

        struct TaskStartTest {
            task: Task,
            pause_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskStartTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_from_loop: entering iteration");

                    tokio::time::delay_for(Duration::from_millis(50)).await;

                    gst_debug!(RUNTIME_CAT, "pause_from_loop: pause from iteration");
                    assert_eq!(
                        self.task.pause().unwrap(),
                        TransitionStatus::NotWaiting {
                            transition: Transition::Pause,
                            origin: TaskState::Started,
                        },
                    );
                    Ok(())
                }
                .boxed()
            }

            fn pause(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_from_loop: entering pause hook");
                    self.pause_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("pause_from_loop", 2).unwrap();

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

        gst_debug!(RUNTIME_CAT, "pause_from_loop: awaiting pause notification");
        pause_receiver.next().await.unwrap();

        task.stop().unwrap();
        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn transition_from_hook() {
        // Purpose: make sure a transition triggered from a transition hook doesn't block.
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
                    gst_debug!(
                        RUNTIME_CAT,
                        "transition_from_hook: flush_start triggering flush_stop"
                    );
                    assert_eq!(
                        self.task.flush_stop().unwrap(),
                        TransitionStatus::NotWaiting {
                            transition: Transition::FlushStop,
                            origin: TaskState::Started,
                        },
                    );
                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "transition_from_hook: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("transition_from_hook", 2).unwrap();

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

        gst_debug!(
            RUNTIME_CAT,
            "transition_from_hook: awaiting flush_stop notification"
        );
        flush_stop_receiver.next().await.unwrap();

        task.stop().unwrap();
        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn pause_flush_start() {
        gst::init().unwrap();

        struct TaskFlushTest {
            started_sender: mpsc::Sender<()>,
            flush_start_sender: mpsc::Sender<()>,
            flush_stop_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskFlushTest {
            fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_flush_start: started");
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
                    gst_debug!(RUNTIME_CAT, "pause_flush_start: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_flush_start: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("pause_flush_start", 2).unwrap();

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

        gst_debug!(RUNTIME_CAT, "pause_flush_start: pausing");
        assert_eq!(
            task.pause().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Prepared,
                target: TaskState::Paused,
            },
        );

        gst_debug!(RUNTIME_CAT, "pause_flush_start: starting flush");
        assert_eq!(
            task.flush_start().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Paused,
                target: TaskState::PausedFlushing,
            },
        );
        assert_eq!(task.state(), TaskState::PausedFlushing);
        flush_start_receiver.next().await;

        gst_debug!(RUNTIME_CAT, "pause_flush_start: stopping flush");
        assert_eq!(
            task.flush_stop().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::PausedFlushing,
                target: TaskState::Paused,
            },
        );
        assert_eq!(task.state(), TaskState::Paused);
        flush_stop_receiver.next().await;

        // start hook not executed
        started_receiver.try_next().unwrap_err();

        gst_debug!(RUNTIME_CAT, "pause_flush_start: starting after flushing");
        assert_eq!(
            task.start().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Paused,
                target: TaskState::Started,
            },
        );
        assert_eq!(task.state(), TaskState::Started);
        started_receiver.next().await;

        task.stop().unwrap();
        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn pause_flushing_start() {
        gst::init().unwrap();

        struct TaskFlushTest {
            started_sender: mpsc::Sender<()>,
            flush_start_sender: mpsc::Sender<()>,
            flush_stop_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskFlushTest {
            fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_flushing_start: started");
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
                    gst_debug!(RUNTIME_CAT, "pause_flushing_start: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_flushing_start: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("pause_flushing_start", 2).unwrap();

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

        gst_debug!(RUNTIME_CAT, "pause_flushing_start: pausing");
        task.pause().unwrap();

        gst_debug!(RUNTIME_CAT, "pause_flushing_start: starting flush");
        task.flush_start().unwrap();
        assert_eq!(task.state(), TaskState::PausedFlushing);
        flush_start_receiver.next().await;

        gst_debug!(RUNTIME_CAT, "pause_flushing_start: starting while flushing");
        assert_eq!(
            task.start().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::PausedFlushing,
                target: TaskState::Flushing,
            },
        );
        assert_eq!(task.state(), TaskState::Flushing);

        // start hook not executed
        started_receiver.try_next().unwrap_err();

        gst_debug!(RUNTIME_CAT, "pause_flushing_start: stopping flush");
        assert_eq!(
            task.flush_stop().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Flushing,
                target: TaskState::Started,
            },
        );
        assert_eq!(task.state(), TaskState::Started);
        flush_stop_receiver.next().await;
        started_receiver.next().await;

        task.stop().unwrap();
        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn flush_concurrent_start() {
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
                    gst_debug!(RUNTIME_CAT, "flush_concurrent_start: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "flush_concurrent_start: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
                    Ok(())
                }
                .boxed()
            }
        }

        let context = Context::acquire("flush_concurrent_start", 2).unwrap();

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

        let oob_context = Context::acquire("flush_concurrent_start_oob", 2).unwrap();
        let task_clone = task.clone();

        task.pause().unwrap();

        // Launch flush_start // start
        let (ready_sender, ready_receiver) = oneshot::channel();
        gst_debug!(RUNTIME_CAT, "flush_concurrent_start: spawning flush_start");
        let flush_start_handle = oob_context.spawn(async move {
            gst_debug!(RUNTIME_CAT, "flush_concurrent_start: // flush_start");
            ready_sender.send(()).unwrap();
            let res = task_clone.flush_start().unwrap();
            match res {
                TransitionStatus::Async {
                    transition: Transition::FlushStart,
                    origin: TaskState::Paused,
                } => (),
                TransitionStatus::Async {
                    transition: Transition::FlushStart,
                    origin: TaskState::Started,
                } => (),
                other => unreachable!("{:?}", other),
            }
            Context::drain_sub_tasks().await.unwrap();
            flush_start_receiver.next().await.unwrap();
        });

        gst_debug!(
            RUNTIME_CAT,
            "flush_concurrent_start: awaiting for oob_context"
        );
        ready_receiver.await.unwrap();

        gst_debug!(RUNTIME_CAT, "flush_concurrent_start: // start");
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
            other => unreachable!("{:?}", other),
        }

        flush_start_handle.await.unwrap();

        gst_debug!(RUNTIME_CAT, "flush_concurrent_start: requesting flush_stop");
        assert_eq!(
            task.flush_stop().unwrap(),
            TransitionStatus::Complete {
                origin: TaskState::Flushing,
                target: TaskState::Started,
            },
        );

        assert_eq!(task.state(), TaskState::Started);
        flush_stop_receiver.next().await;

        task.stop().unwrap();
        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn start_timer() {
        // Purpose: make sure a Timer initialized in a transition is
        // available when iterating in the loop.
        gst::init().unwrap();

        struct TaskTimerTest {
            timer: Option<tokio::time::Delay>,
            timer_elapsed_sender: Option<oneshot::Sender<()>>,
        }

        impl TaskImpl for TaskTimerTest {
            fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
                async move {
                    self.timer = Some(tokio::time::delay_for(Duration::from_millis(50)));
                    gst_debug!(RUNTIME_CAT, "start_timer: started");
                    Ok(())
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "start_timer: awaiting timer");
                    self.timer.take().unwrap().await;
                    gst_debug!(RUNTIME_CAT, "start_timer: timer elapsed");

                    if let Some(timer_elapsed_sender) = self.timer_elapsed_sender.take() {
                        timer_elapsed_sender.send(()).unwrap();
                    }

                    Err(gst::FlowError::Eos)
                }
                .boxed()
            }
        }

        let context = Context::acquire("start_timer", 2).unwrap();

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

        gst_debug!(RUNTIME_CAT, "start_timer: start");
        task.start().unwrap();

        timer_elapsed_receiver.await.unwrap();
        gst_debug!(RUNTIME_CAT, "start_timer: timer elapsed received");

        task.stop().unwrap();
        task.unprepare().unwrap();
    }
}
