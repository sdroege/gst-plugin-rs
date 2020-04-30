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
use std::sync::{Arc, Mutex, MutexGuard};

use super::executor::{block_on_or_add_sub_task, TaskId};
use super::{Context, JoinHandle, RUNTIME_CAT};

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy)]
pub enum TaskState {
    Flushing,
    Paused,
    PausedFlushing,
    PrepareFailed,
    Prepared,
    Preparing,
    Started,
    Stopped,
    Unprepared,
    Unpreparing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskError {
    ActiveTask,
    InactiveTask,
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TaskError::ActiveTask => write!(f, "Task is active"),
            TaskError::InactiveTask => write!(f, "Task is not active"),
        }
    }
}

impl std::error::Error for TaskError {}

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

    /// Handles an error happening during prepare.
    ///
    /// This handler also catches errors returned by subtasks spawned by `prepare`.
    ///
    /// Implementation might use `gst::Element::post_error_message`.
    fn handle_prepare_error(&mut self, err: gst::ErrorMessage) -> BoxFuture<'_, ()> {
        async move {
            gst_error!(
                RUNTIME_CAT,
                "TaskImpl default handle_prepare_error received {:?}",
                err
            );
        }
        .boxed()
    }

    fn unprepare(&mut self) -> BoxFuture<'_, ()> {
        future::ready(()).boxed()
    }

    fn start(&mut self) -> BoxFuture<'_, ()> {
        future::ready(()).boxed()
    }

    /// Executes an iteration in `TaskState::Started`.
    fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>>;

    fn pause(&mut self) -> BoxFuture<'_, ()> {
        future::ready(()).boxed()
    }

    fn flush_start(&mut self) -> BoxFuture<'_, ()> {
        future::ready(()).boxed()
    }

    fn flush_stop(&mut self) -> BoxFuture<'_, ()> {
        future::ready(()).boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, ()> {
        future::ready(()).boxed()
    }
}

#[derive(Clone, Copy, Debug)]
enum TransitionKind {
    FlushStart,
    FlushStop,
    Pause,
    Prepare,
    Start,
    Stop,
    Unprepare,
}

struct Transition {
    kind: TransitionKind,
    ack_tx: Option<oneshot::Sender<Transition>>,
}

impl Transition {
    fn new(kind: TransitionKind) -> (Self, oneshot::Receiver<Transition>) {
        let (ack_tx, ack_rx) = oneshot::channel();
        let req = Transition {
            kind,
            ack_tx: Some(ack_tx),
        };

        (req, ack_rx)
    }

    fn send_ack(mut self) {
        if let Some(ack_tx) = self.ack_tx.take() {
            let _ = ack_tx.send(self);
        }
    }
}

impl fmt::Debug for Transition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transition")
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
    transition_tx: Option<async_mpsc::Sender<Transition>>,
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
    fn switch_to_state(&mut self, state: TaskState, transition: Transition) {
        self.state = state;
        transition.send_ack();
    }

    fn skip_transition(&mut self, transition: Transition) {
        transition.send_ack();
    }

    fn push_transition(
        &mut self,
        kind: TransitionKind,
    ) -> Result<oneshot::Receiver<Transition>, TaskError> {
        if self.transition_tx.is_none() {
            return Err(TaskError::InactiveTask);
        }

        let (transition, ack_rx) = Transition::new(kind);

        gst_log!(RUNTIME_CAT, "Requesting {:?}", transition);

        self.transition_tx
            .as_mut()
            .unwrap()
            .try_send(transition)
            .or(Err(TaskError::InactiveTask))?;

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

    pub fn prepare(&self, task_impl: impl TaskImpl, context: Context) -> Result<(), TaskError> {
        let mut inner = self.0.lock().unwrap();

        match inner.state {
            TaskState::Unprepared => (),
            TaskState::Prepared => {
                gst_debug!(RUNTIME_CAT, "Task already prepared");
                return Err(TaskError::ActiveTask);
            }
            TaskState::Preparing => {
                gst_warning!(RUNTIME_CAT, "Task already preparing");
                return Err(TaskError::ActiveTask);
            }
            TaskState::Unpreparing => {
                gst_warning!(RUNTIME_CAT, "Task unpreparing");
                return Err(TaskError::InactiveTask);
            }
            state => {
                gst_warning!(
                    RUNTIME_CAT,
                    "Attempt to prepare a task in state {:?}",
                    state
                );
                return Err(TaskError::ActiveTask);
            }
        }

        assert!(inner.state_machine_handle.is_none());

        inner.state = TaskState::Preparing;

        gst_log!(RUNTIME_CAT, "Starting task state machine");

        // FIXME allow configuration of the channel buffer size,
        // this determines the contention on the Task.
        let (transition_tx, transition_rx) = async_mpsc::channel(4);
        let state_machine = StateMachine::new(Box::new(task_impl), transition_rx);
        let (transition, _) = Transition::new(TransitionKind::Prepare);
        inner.state_machine_handle = Some(inner.state_machine_context.spawn(state_machine.run(
            Arc::clone(&self.0),
            context.clone(),
            transition,
        )));

        inner.transition_tx = Some(transition_tx);
        inner.context = Some(context);

        gst_log!(RUNTIME_CAT, "Task state machine started");

        Ok(())
    }

    pub fn unprepare(&self) -> Result<(), TaskError> {
        let mut inner = self.0.lock().unwrap();

        match inner.state {
            TaskState::Stopped
            | TaskState::Prepared
            | TaskState::PrepareFailed
            | TaskState::Preparing => (),
            TaskState::Unprepared => {
                gst_debug!(RUNTIME_CAT, "Task already unprepared");
                return Err(TaskError::InactiveTask);
            }
            TaskState::Unpreparing => {
                gst_debug!(RUNTIME_CAT, "Task already unpreparing");
                return Err(TaskError::InactiveTask);
            }
            state => {
                gst_warning!(
                    RUNTIME_CAT,
                    "Attempt to unprepare a task in state  {:?}",
                    state
                );
                return Err(TaskError::ActiveTask);
            }
        }

        gst_debug!(RUNTIME_CAT, "Unpreparing task");

        inner.state = TaskState::Unpreparing;

        if let Some(loop_abort_handle) = inner.loop_abort_handle.take() {
            loop_abort_handle.abort();
        }

        let _ = inner.push_transition(TransitionKind::Unprepare).unwrap();
        let transition_tx = inner.transition_tx.take().unwrap();

        let state_machine_handle = inner.state_machine_handle.take();
        let context = inner.context.take().unwrap();

        if let Some(prepare_abort_handle) = inner.prepare_abort_handle.take() {
            prepare_abort_handle.abort();
        }

        drop(inner);

        if let Some(state_machine_handle) = state_machine_handle {
            // We can block on the state_machine_handle since `unprepare` is never called
            // from the state machine Context, but from a spawned Task Context subtask.
            gst_log!(
                RUNTIME_CAT,
                "Synchronously waiting for the state machine {:?}",
                state_machine_handle,
            );
            let _ = block_on_or_add_sub_task(state_machine_handle);
        }

        drop(transition_tx);
        drop(context);

        gst_debug!(RUNTIME_CAT, "Task unprepared");

        Ok(())
    }

    /// `Starts` the `Task`.
    ///
    /// The execution occurs on the `Task` context.
    pub fn start(&self) {
        let mut inner = self.0.lock().unwrap();

        let ack_rx = match inner.push_transition(TransitionKind::Start) {
            Ok(ack_rx) => ack_rx,
            Err(err) => {
                gst_warning!(RUNTIME_CAT, "Error attempting to Start task: {:?}", err);
                return;
            }
        };

        if let TaskState::Started = inner.state {
            // Don't await ack_rx because TaskImpl::iterate might be pending
            return;
        }

        Self::await_ack(inner, ack_rx, TransitionKind::Start);
    }

    /// Requests the `Task` loop to pause.
    ///
    /// If an iteration is in progress, it will run to completion,
    /// then no more iteration will be executed before `start` is called again.
    /// Therefore, it is not guaranteed that `Paused` is reached when `pause` returns.
    pub fn pause(&self) {
        let mut inner = self.0.lock().unwrap();

        if let Err(err) = inner.push_transition(TransitionKind::Pause) {
            gst_warning!(RUNTIME_CAT, "Error attempting to Pause task: {:?}", err);
        }
    }

    pub fn flush_start(&self) {
        let mut inner = self.0.lock().unwrap();

        if let Some(loop_abort_handle) = inner.loop_abort_handle.take() {
            loop_abort_handle.abort();
        }

        Self::push_and_await_transition(inner, TransitionKind::FlushStart);
    }

    pub fn flush_stop(&self) {
        let mut inner = self.0.lock().unwrap();

        if let Some(loop_abort_handle) = inner.loop_abort_handle.take() {
            loop_abort_handle.abort();
        }

        Self::push_and_await_transition(inner, TransitionKind::FlushStop);
    }

    /// Stops the `Started` `Task` and wait for it to finish.
    pub fn stop(&self) {
        let mut inner = self.0.lock().unwrap();

        if let Some(loop_abort_handle) = inner.loop_abort_handle.take() {
            loop_abort_handle.abort();
        }

        Self::push_and_await_transition(inner, TransitionKind::Stop);
    }

    fn push_and_await_transition(
        mut inner: MutexGuard<TaskInner>,
        transition_kind: TransitionKind,
    ) {
        match inner.push_transition(transition_kind) {
            Ok(ack_rx) => Self::await_ack(inner, ack_rx, transition_kind),
            Err(err) => {
                gst_warning!(
                    RUNTIME_CAT,
                    "Error attempting to {:?} task: {:?}",
                    transition_kind,
                    err
                );
            }
        }
    }

    fn await_ack(
        inner: MutexGuard<TaskInner>,
        ack_rx: oneshot::Receiver<Transition>,
        transition_kind: TransitionKind,
    ) {
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
                        "Transitionning to {:?} from loop or transition hook, not waiting",
                        transition_kind,
                    );
                    return;
                }
            }
        }

        drop(inner);

        block_on_or_add_sub_task(async move {
            gst_trace!(RUNTIME_CAT, "Awaiting ack for {:?}", transition_kind);
            if let Ok(transition) = ack_rx.await {
                gst_log!(RUNTIME_CAT, "Received ack for {:?}", transition);
            } else {
                gst_log!(
                    RUNTIME_CAT,
                    "Transition to {:?} was dropped",
                    transition_kind
                );
            }
        });
    }
}

struct StateMachine {
    task_impl: Box<dyn TaskImpl>,
    transition_rx: async_mpsc::Receiver<Transition>,
    pending_transition: Option<Transition>,
}

// Make sure the Context doesn't throttle otherwise we end up  with long delays
// executing transition in a pipeline with many elements. This is because pipeline
// serializes the transitions and the Context's scheduler gets a chance to reach its
// throttling state between 2 elements.

macro_rules! spawn_hook {
    ($hook:ident, $self:ident, $task_inner:expr, $context:expr) => {{
        let hook_fut = async move {
            $self.task_impl.$hook().await;
            $self
        };

        let join_handle = {
            let mut task_inner = $task_inner.lock().unwrap();
            let join_handle = $context.awake_and_spawn(hook_fut);
            task_inner.spawned_task_id = Some(join_handle.task_id());

            join_handle
        };

        join_handle.map(|res| res.unwrap())
    }};
}

impl StateMachine {
    // Use dynamic dispatch for TaskImpl as it reduces memory usage compared to monomorphization
    // without inducing any significant performance penalties.
    fn new(task_impl: Box<dyn TaskImpl>, transition_rx: async_mpsc::Receiver<Transition>) -> Self {
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
        transition: Transition,
    ) {
        gst_trace!(RUNTIME_CAT, "Preparing task");

        self = StateMachine::spawn_prepare(self, &task_inner, &context, transition).await;

        loop {
            let transition = match self.pending_transition.take() {
                Some(pending_transition) => pending_transition,
                None => self
                    .transition_rx
                    .next()
                    .await
                    .expect("transition_rx dropped"),
            };

            gst_trace!(RUNTIME_CAT, "State machine popped {:?}", transition);

            match transition.kind {
                TransitionKind::Start => {
                    {
                        let mut task_inner = task_inner.lock().unwrap();
                        match task_inner.state {
                            TaskState::Stopped | TaskState::Paused | TaskState::Prepared => (),
                            TaskState::PausedFlushing => {
                                task_inner.switch_to_state(TaskState::Flushing, transition);
                                gst_trace!(RUNTIME_CAT, "Switched from PausedFlushing to Flushing");
                                continue;
                            }
                            state => {
                                task_inner.skip_transition(transition);
                                gst_trace!(RUNTIME_CAT, "Skipped Pause in state {:?}", state);
                                continue;
                            }
                        }
                    }

                    self = StateMachine::spawn_loop(self, &task_inner, &context, transition).await;
                    // next/pending transition handled in next iteration
                }
                TransitionKind::Pause => {
                    let target_state = {
                        let mut task_inner = task_inner.lock().unwrap();
                        match task_inner.state {
                            TaskState::Started | TaskState::Stopped | TaskState::Prepared => {
                                TaskState::Paused
                            }
                            TaskState::Flushing => TaskState::PausedFlushing,
                            state => {
                                task_inner.skip_transition(transition);
                                gst_trace!(RUNTIME_CAT, "Skipped Pause in state {:?}", state);
                                continue;
                            }
                        }
                    };

                    self = spawn_hook!(pause, self, &task_inner, &context).await;
                    task_inner
                        .lock()
                        .unwrap()
                        .switch_to_state(target_state, transition);
                }
                TransitionKind::Stop => {
                    {
                        let mut task_inner = task_inner.lock().unwrap();
                        match task_inner.state {
                            TaskState::Started
                            | TaskState::Paused
                            | TaskState::PausedFlushing
                            | TaskState::Flushing => (),
                            state => {
                                task_inner.skip_transition(transition);
                                gst_trace!(RUNTIME_CAT, "Skipped Stop in state {:?}", state);
                                continue;
                            }
                        }
                    }

                    self = spawn_hook!(stop, self, &task_inner, &context).await;
                    task_inner
                        .lock()
                        .unwrap()
                        .switch_to_state(TaskState::Stopped, transition);
                    gst_trace!(RUNTIME_CAT, "Task loop stopped");
                }
                TransitionKind::FlushStart => {
                    let target_state = {
                        let mut task_inner = task_inner.lock().unwrap();
                        match task_inner.state {
                            TaskState::Started => TaskState::Flushing,
                            TaskState::Paused => TaskState::PausedFlushing,
                            state => {
                                task_inner.skip_transition(transition);
                                gst_trace!(RUNTIME_CAT, "Skipped FlushStart in state {:?}", state);
                                continue;
                            }
                        }
                    };

                    self = spawn_hook!(flush_start, self, &task_inner, &context).await;
                    task_inner
                        .lock()
                        .unwrap()
                        .switch_to_state(target_state, transition);
                    gst_trace!(RUNTIME_CAT, "Task flush started");
                }
                TransitionKind::FlushStop => {
                    let state = task_inner.lock().unwrap().state;
                    match state {
                        TaskState::Flushing => (),
                        TaskState::PausedFlushing => {
                            self = spawn_hook!(flush_stop, self, &task_inner, &context).await;
                            task_inner
                                .lock()
                                .unwrap()
                                .switch_to_state(TaskState::Paused, transition);
                            gst_trace!(RUNTIME_CAT, "Switched from PausedFlushing to Paused");
                            continue;
                        }
                        state => {
                            task_inner.lock().unwrap().skip_transition(transition);
                            gst_trace!(RUNTIME_CAT, "Skipped FlushStop in state {:?}", state);
                            continue;
                        }
                    }

                    self = spawn_hook!(flush_stop, self, &task_inner, &context).await;
                    self = StateMachine::spawn_loop(self, &task_inner, &context, transition).await;
                    // next/pending transition handled in next iteration
                }
                TransitionKind::Unprepare => {
                    StateMachine::spawn_unprepare(self, context).await;
                    task_inner
                        .lock()
                        .unwrap()
                        .switch_to_state(TaskState::Unprepared, transition);

                    break;
                }
                _ => unreachable!("State machine handler {:?}", transition),
            }
        }

        gst_trace!(RUNTIME_CAT, "Task state machine terminated");
    }

    async fn prepare(&mut self) -> Result<(), gst::ErrorMessage> {
        self.task_impl.prepare().await?;

        gst_trace!(RUNTIME_CAT, "Draining subtasks");
        while Context::current_has_sub_tasks() {
            Context::drain_sub_tasks().await.map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to drain substasks while preparing: {:?}", err]
                )
            })?;
        }

        Ok(())
    }

    async fn run_loop(&mut self, task_inner: Arc<Mutex<TaskInner>>) -> Result<(), gst::FlowError> {
        gst_trace!(RUNTIME_CAT, "Task loop started");

        loop {
            // Check if there is any pending transition
            while let Ok(Some(transition)) = self.transition_rx.try_next() {
                gst_trace!(RUNTIME_CAT, "Task loop popped {:?}", transition);

                match transition.kind {
                    TransitionKind::Start => {
                        task_inner.lock().unwrap().skip_transition(transition);
                        gst_trace!(RUNTIME_CAT, "Skipped Start in state Started");
                    }
                    _ => {
                        gst_trace!(
                            RUNTIME_CAT,
                            "Task loop handing {:?} to state machine",
                            transition,
                        );
                        self.pending_transition = Some(transition);
                        return Ok(());
                    }
                }
            }

            // Run the iteration
            self.task_impl.iterate().await.map_err(|err| {
                gst_log!(RUNTIME_CAT, "Task loop iterate impl returned {:?}", err);
                err
            })?;
        }
    }

    async fn spawn_prepare(
        mut self,
        task_inner: &Arc<Mutex<TaskInner>>,
        context: &Context,
        transition: Transition,
    ) -> Self {
        let task_inner_clone = Arc::clone(&task_inner);
        let prepare_fut = async move {
            let (abortable_prepare, abort_handle) = abortable(self.prepare());
            task_inner_clone.lock().unwrap().prepare_abort_handle = Some(abort_handle);

            let res = abortable_prepare.await.unwrap_or_else(|_| {
                Err(gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Task preparation aborted"]
                ))
            });

            match res {
                Ok(()) => {
                    task_inner_clone
                        .lock()
                        .unwrap()
                        .switch_to_state(TaskState::Prepared, transition);
                    gst_log!(RUNTIME_CAT, "Task prepared");
                }
                Err(err) => {
                    {
                        let mut task_inner = task_inner_clone.lock().unwrap();
                        task_inner.skip_transition(transition);
                        task_inner.state = TaskState::PrepareFailed;
                    }

                    gst_error!(RUNTIME_CAT, "Task preparation failed: {:?}", err);
                    self.task_impl.handle_prepare_error(err).await;

                    gst_log!(
                        RUNTIME_CAT,
                        "Waiting for Unprepare due to task preparation failure"
                    );
                    loop {
                        let transition = self
                            .transition_rx
                            .next()
                            .await
                            .expect("transition_rx dropped");

                        if let TransitionKind::Unprepare = transition.kind {
                            self.pending_transition = Some(transition);
                            break;
                        } else {
                            gst_log!(
                                RUNTIME_CAT,
                                "Skipping {:?} since task preparation failed",
                                transition,
                            );

                            task_inner_clone.lock().unwrap().skip_transition(transition);
                        }
                    }
                }
            }

            self
        };

        let join_handle = {
            let mut task_inner = task_inner.lock().unwrap();
            let join_handle = context.awake_and_spawn(prepare_fut);
            task_inner.spawned_task_id = Some(join_handle.task_id());

            join_handle
        };

        join_handle.await.unwrap()
    }

    async fn spawn_loop(
        mut self,
        task_inner: &Arc<Mutex<TaskInner>>,
        context: &Context,
        transition: Transition,
    ) -> Self {
        let task_inner_clone = Arc::clone(&task_inner);
        let loop_fut = async move {
            gst_trace!(RUNTIME_CAT, "Starting task loop");
            self.task_impl.start().await;

            let abortable_task_loop = {
                let (abortable_task_loop, loop_abort_handle) =
                    abortable(self.run_loop(Arc::clone(&task_inner_clone)));

                let mut task_inner = task_inner_clone.lock().unwrap();
                task_inner.loop_abort_handle = Some(loop_abort_handle);
                task_inner.switch_to_state(TaskState::Started, transition);

                abortable_task_loop
            };

            match abortable_task_loop.await {
                Ok(Ok(())) => (),
                Ok(Err(gst::FlowError::Flushing)) => {
                    gst_trace!(RUNTIME_CAT, "Starting task flush");
                    self.task_impl.flush_start().await;
                    task_inner_clone.lock().unwrap().state = TaskState::Flushing;
                    gst_trace!(RUNTIME_CAT, "Started task flush");
                }
                Ok(Err(err)) => {
                    // Note: this includes EOS
                    gst_trace!(RUNTIME_CAT, "Stopping task due to {:?}", err);
                    self.task_impl.stop().await;
                    task_inner_clone.lock().unwrap().state = TaskState::Stopped;
                    gst_trace!(RUNTIME_CAT, "Stopped task due to {:?}", err);
                }
                Err(Aborted) => gst_trace!(RUNTIME_CAT, "Task loop aborted"),
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

        join_handle.await.unwrap()
    }

    async fn spawn_unprepare(mut self, context: Context) {
        // Unprepare is not joined by an ack_rx but by joining the state machine
        // handle, so we don't need to keep track of the spwaned_task_id
        context
            .awake_and_spawn(async move {
                self.task_impl.unprepare().await;
            })
            .await
            .unwrap()
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

            fn start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "task_iterate: started");
                    self.started_sender.send(()).await.unwrap();
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
                        gst_debug!(RUNTIME_CAT, "task_iterate: received true => keep looping");
                    } else {
                        gst_debug!(
                            RUNTIME_CAT,
                            "task_iterate: received false => cancelling loop"
                        );
                    }

                    res
                }
                .boxed()
            }

            fn pause(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "task_iterate: paused");
                    self.paused_sender.send(()).await.unwrap();
                }
                .boxed()
            }

            fn stop(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "task_iterate: stopped");
                    self.stopped_sender.send(()).await.unwrap();
                }
                .boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "task_iterate: stopped");
                    self.flush_start_sender.send(()).await.unwrap();
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
        task.prepare(
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

        gst_debug!(RUNTIME_CAT, "task_iterate: starting (initial)");
        task.start();

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
        task.start();
        assert_eq!(task.state(), TaskState::Started);

        gst_debug!(RUNTIME_CAT, "task_iterate: pause (initial)");
        task.pause();

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
        task.start();

        assert_eq!(task.state(), TaskState::Started);
        // Paused -> Started
        let _ = started_receiver.next().await;

        gst_debug!(RUNTIME_CAT, "task_iterate: stopping (initial)");
        task.stop();

        assert_eq!(task.state(), TaskState::Stopped);
        let _ = stopped_receiver.next().await;

        // purge remaining iteration received before stop if any
        let _ = iterate_receiver.try_next();

        gst_debug!(RUNTIME_CAT, "task_iterate: starting (after stop)");
        task.start();
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
        task.start();
        let _ = started_receiver.next().await;

        gst_debug!(RUNTIME_CAT, "task_iterate: req. iterate to return Error");
        iterate_receiver.next().await.unwrap();
        complete_iterate_sender
            .send(Err(gst::FlowError::Error))
            .await
            .unwrap();

        gst_debug!(RUNTIME_CAT, "task_iterate: awaiting stop hook ack");
        stopped_receiver.next().await.unwrap();

        // Wait for state machine to reach Stopped
        while TaskState::Stopped != task.state() {
            tokio::time::delay_for(Duration::from_millis(2)).await;
        }

        gst_debug!(RUNTIME_CAT, "task_iterate: starting (after Error)");
        task.start();

        assert_eq!(task.state(), TaskState::Started);
        gst_debug!(RUNTIME_CAT, "task_iterate: awaiting start hook ack");
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

        gst_debug!(RUNTIME_CAT, "task_iterate: stopping (final)");
        task.stop();

        assert_eq!(task.state(), TaskState::Stopped);
        let _ = stopped_receiver.next().await;

        task.unprepare().unwrap();

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
                        gst::ResourceError::OpenRead,
                        ["prepare_error: intentional prepare error"]
                    ))
                }
                .boxed()
            }

            fn handle_prepare_error(&mut self, _err: gst::ErrorMessage) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "prepare_error: handling prepare error");
                    self.prepare_error_sender.send(()).await.unwrap();
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

        gst_debug!(
            RUNTIME_CAT,
            "prepare_error: await prepare error notification"
        );
        prepare_error_receiver.next().await.unwrap();

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

            fn handle_prepare_error(&mut self, _err: gst::ErrorMessage) -> BoxFuture<'_, ()> {
                unreachable!("prepare_start_ok: handle_prepare_error");
            }

            fn start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "prepare_start_ok: started");
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
            ready_sender.send(()).unwrap();
            task_clone.start();
            Context::drain_sub_tasks().await.unwrap();

            assert_eq!(task_clone.state(), TaskState::Started);

            task_clone.stop();
            Context::drain_sub_tasks().await.unwrap();

            task_clone.unprepare().unwrap();
            Context::drain_sub_tasks().await.unwrap();
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
                        gst::ResourceError::OpenRead,
                        ["prepare_start_error: intentional prepare error"]
                    ))
                }
                .boxed()
            }

            fn handle_prepare_error(&mut self, _err: gst::ErrorMessage) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "prepare_start_error: handling prepare error");
                    self.prepare_error_sender.send(()).await.unwrap();
                }
                .boxed()
            }

            fn start(&mut self) -> BoxFuture<'_, ()> {
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
            ready_sender.send(()).unwrap();
            task_clone.start();
            Context::drain_sub_tasks().await.unwrap();

            assert_eq!(task_clone.state(), TaskState::PrepareFailed);
            task_clone.unprepare().unwrap();
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

            fn pause(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_start: paused");
                    self.paused_sender.send(()).await.unwrap();
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
        task.start();
        assert_eq!(task.state(), TaskState::Started);

        gst_debug!(RUNTIME_CAT, "pause_start: awaiting 1st iteration");
        iterate_receiver.next().await.unwrap();

        gst_debug!(RUNTIME_CAT, "pause_start: pausing (1)");
        task.pause();

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

        task.start();
        assert_eq!(task.state(), TaskState::Started);

        gst_debug!(RUNTIME_CAT, "pause_start: awaiting 2d iteration");
        iterate_receiver.next().await.unwrap();

        gst_debug!(RUNTIME_CAT, "pause_start: sending 2d iteration completion");
        complete_sender.try_send(()).unwrap();

        task.stop();
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
        task.start();

        gst_debug!(RUNTIME_CAT, "successive_pause_start: awaiting iteration 1");
        iterate_receiver.next().await.unwrap();

        gst_debug!(RUNTIME_CAT, "successive_pause_start: pause and start");
        task.pause();
        task.start();

        assert_eq!(task.state(), TaskState::Started);

        gst_debug!(RUNTIME_CAT, "successive_pause_start: awaiting iteration 2");
        iterate_receiver.next().await.unwrap();

        gst_debug!(RUNTIME_CAT, "successive_pause_start: stopping");
        task.stop();
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

            fn flush_start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "flush_regular_sync: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "flush_regular_sync: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
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
        task.start();

        gst_debug!(RUNTIME_CAT, "flush_regular_sync: starting flush");
        task.flush_start();
        assert_eq!(task.state(), TaskState::Flushing);

        flush_start_receiver.next().await.unwrap();

        gst_debug!(RUNTIME_CAT, "flush_regular_sync: stopping flush");
        task.flush_stop();
        assert_eq!(task.state(), TaskState::Started);

        flush_stop_receiver.next().await.unwrap();

        task.pause();
        task.stop();
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

            fn flush_start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(
                        RUNTIME_CAT,
                        "flush_regular_different_context: started flushing"
                    );
                    self.flush_start_sender.send(()).await.unwrap();
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(
                        RUNTIME_CAT,
                        "flush_regular_different_context: stopped flushing"
                    );
                    self.flush_stop_sender.send(()).await.unwrap();
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
        task.start();

        let oob_context = Context::acquire("flush_regular_different_context_oob", 2).unwrap();

        let task_clone = task.clone();
        let flush_handle = oob_context.spawn(async move {
            task_clone.flush_start();
            Context::drain_sub_tasks().await.unwrap();
            assert_eq!(task_clone.state(), TaskState::Flushing);
            flush_start_receiver.next().await.unwrap();

            task_clone.flush_stop();
            Context::drain_sub_tasks().await.unwrap();
            assert_eq!(task_clone.state(), TaskState::Started);
        });

        flush_handle.await.unwrap();
        flush_stop_receiver.next().await.unwrap();

        task.stop();
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

            fn flush_start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "flush_regular_same_context: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "flush_regular_same_context: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
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

        task.start();

        let task_clone = task.clone();
        let flush_handle = task.context().as_ref().unwrap().spawn(async move {
            task_clone.flush_start();
            Context::drain_sub_tasks().await.unwrap();
            assert_eq!(task_clone.state(), TaskState::Flushing);
            flush_start_receiver.next().await.unwrap();

            task_clone.flush_stop();
            Context::drain_sub_tasks().await.unwrap();
            assert_eq!(task_clone.state(), TaskState::Started);
        });

        flush_handle.await.unwrap();
        flush_stop_receiver.next().await.unwrap();

        task.stop();
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
                    self.task.flush_start();
                    Context::drain_sub_tasks().await
                }
                .boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "flush_from_loop: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
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

        task.start();

        gst_debug!(
            RUNTIME_CAT,
            "flush_from_loop: awaiting flush_start notification"
        );
        flush_start_receiver.next().await.unwrap();

        task.stop();
        task.unprepare().unwrap();
    }

    #[tokio::test]
    async fn start_from_loop() {
        // Purpose: make sure a start transition triggered from an iteration doesn't block.
        // E.g. an auto pause cancellation after a delay.
        gst::init().unwrap();

        struct TaskStartTest {
            task: Task,
            iterate_sender: mpsc::Sender<()>,
        }

        impl TaskImpl for TaskStartTest {
            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                async move {
                    gst_debug!(RUNTIME_CAT, "start_from_loop: entering iteration");
                    self.iterate_sender.send(()).await.unwrap();

                    tokio::time::delay_for(Duration::from_millis(50)).await;

                    gst_debug!(RUNTIME_CAT, "start_from_loop: start from iteration");
                    self.task.start();
                    Context::drain_sub_tasks().await
                }
                .boxed()
            }
        }

        let context = Context::acquire("start_from_loop", 2).unwrap();

        let task = Task::default();

        let (iterate_sender, mut iterate_receiver) = mpsc::channel(1);
        task.prepare(
            TaskStartTest {
                task: task.clone(),
                iterate_sender,
            },
            context,
        )
        .unwrap();

        task.start();
        iterate_receiver.next().await.unwrap();
        task.pause();

        gst_debug!(RUNTIME_CAT, "start_from_loop: awaiting start notification");
        iterate_receiver.next().await.unwrap();

        task.stop();
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

            fn flush_start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(
                        RUNTIME_CAT,
                        "transition_from_hook: flush_start triggering flush_stop"
                    );
                    self.task.flush_stop();
                    Context::drain_sub_tasks().await.unwrap();
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "transition_from_hook: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
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

        task.start();
        task.flush_start();

        gst_debug!(
            RUNTIME_CAT,
            "transition_from_hook: awaiting flush_stop notification"
        );
        flush_stop_receiver.next().await.unwrap();

        task.stop();
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
            fn start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_flush_start: started");
                    self.started_sender.send(()).await.unwrap();
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::pending::<Result<(), gst::FlowError>>().boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_flush_start: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_flush_start: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
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
        task.pause();

        gst_debug!(RUNTIME_CAT, "pause_flush_start: starting flush");
        task.flush_start();
        assert_eq!(task.state(), TaskState::PausedFlushing);
        flush_start_receiver.next().await;

        gst_debug!(RUNTIME_CAT, "pause_flush_start: stopping flush");
        task.flush_stop();
        assert_eq!(task.state(), TaskState::Paused);
        flush_stop_receiver.next().await;

        // start hook not executed
        started_receiver.try_next().unwrap_err();

        gst_debug!(RUNTIME_CAT, "pause_flush_start: starting after flushing");
        task.start();
        assert_eq!(task.state(), TaskState::Started);
        started_receiver.next().await;

        task.stop();
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
            fn start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_flushing_start: started");
                    self.started_sender.send(()).await.unwrap();
                }
                .boxed()
            }

            fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
                future::pending::<Result<(), gst::FlowError>>().boxed()
            }

            fn flush_start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_flushing_start: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "pause_flushing_start: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
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
        task.pause();

        gst_debug!(RUNTIME_CAT, "pause_flushing_start: starting flush");
        task.flush_start();
        assert_eq!(task.state(), TaskState::PausedFlushing);
        flush_start_receiver.next().await;

        gst_debug!(RUNTIME_CAT, "pause_flushing_start: starting while flushing");
        task.start();
        assert_eq!(task.state(), TaskState::Flushing);

        // start hook not executed
        started_receiver.try_next().unwrap_err();

        gst_debug!(RUNTIME_CAT, "pause_flushing_start: stopping flush");
        task.flush_stop();
        assert_eq!(task.state(), TaskState::Started);
        flush_stop_receiver.next().await;
        started_receiver.next().await;

        task.stop();
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

            fn flush_start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "flush_concurrent_start: started flushing");
                    self.flush_start_sender.send(()).await.unwrap();
                }
                .boxed()
            }

            fn flush_stop(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    gst_debug!(RUNTIME_CAT, "flush_concurrent_start: stopped flushing");
                    self.flush_stop_sender.send(()).await.unwrap();
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

        task.start();
        task.pause();

        // Launch flush_start // start
        let (ready_sender, ready_receiver) = oneshot::channel();
        gst_debug!(RUNTIME_CAT, "flush_concurrent_start: spawning flush_start");
        let flush_start_handle = oob_context.spawn(async move {
            gst_debug!(RUNTIME_CAT, "flush_concurrent_start: // flush_start");
            ready_sender.send(()).unwrap();
            task_clone.flush_start();
            Context::drain_sub_tasks().await.unwrap();
            flush_start_receiver.next().await.unwrap();
        });

        gst_debug!(
            RUNTIME_CAT,
            "flush_concurrent_start: awaiting for oob_context"
        );
        ready_receiver.await.unwrap();

        gst_debug!(RUNTIME_CAT, "flush_concurrent_start: // start");
        task.start();

        flush_start_handle.await.unwrap();

        gst_debug!(RUNTIME_CAT, "flush_concurrent_start: requesting flush_stop");
        task.flush_stop();

        assert_eq!(task.state(), TaskState::Started);
        flush_stop_receiver.next().await;

        task.stop();
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
            fn start(&mut self) -> BoxFuture<'_, ()> {
                async move {
                    self.timer = Some(tokio::time::delay_for(Duration::from_millis(50)));
                    gst_debug!(RUNTIME_CAT, "start_timer: started");
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
        task.start();

        timer_elapsed_receiver.await.unwrap();
        gst_debug!(RUNTIME_CAT, "start_timer: timer elapsed received");

        task.stop();
        task.unprepare().unwrap();
    }
}
