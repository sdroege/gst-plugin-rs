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

use futures::future::{self, AbortHandle, Abortable};
use futures::prelude::*;

use super::{waitable, WaitError, WaitHandle, Waitable};

pub type AbortableWaitable<Fut> = Waitable<Abortable<Fut>>;

/// Builds an [`Abortable`] and [`Waitable`] `Future` from the provided `Future`.
///
/// See [`AbortWaitHandle`].
///
/// [`Abortable`]: https://rust-lang-nursery.github.io/futures-api-docs/0.3.0-alpha.19/futures/future/struct.Abortable.html
/// [`Waitable`]: struct.Waitable.html
/// [`AbortWaitHandle`]: struct.AbortWaitHandle.html
pub fn abortable_waitable<Fut: Future>(future: Fut) -> (AbortableWaitable<Fut>, AbortWaitHandle) {
    let (abortable, abort_handle) = future::abortable(future);
    let (abortable_waitable, wait_handle) = waitable(abortable);

    (
        abortable_waitable,
        AbortWaitHandle::new(abort_handle, wait_handle),
    )
}

/// A handle to an [`Abortable`] and [`Waitable`] `Future`.
///
/// The handle allows checking for the `Future` state, canceling the `Future` and waiting until
///  the `Future` completes.
///
/// [`Abortable`]: https://rust-lang-nursery.github.io/futures-api-docs/0.3.0-alpha.19/futures/future/struct.Abortable.html
/// [`Waitable`]: struct.Waitable.html
#[derive(Debug)]
pub struct AbortWaitHandle {
    abort_handle: AbortHandle,
    wait_handle: WaitHandle,
}

impl AbortWaitHandle {
    fn new(abort_handle: AbortHandle, wait_handle: WaitHandle) -> Self {
        AbortWaitHandle {
            abort_handle,
            wait_handle,
        }
    }

    pub fn is_terminated(&mut self) -> bool {
        self.wait_handle.is_terminated()
    }

    pub fn is_cancelled(&mut self) -> bool {
        self.wait_handle.is_cancelled()
    }

    pub async fn wait(self) -> Result<(), WaitError> {
        self.wait_handle.wait().await
    }

    pub fn abort(&self) {
        self.abort_handle.abort();
    }

    pub async fn abort_and_wait(mut self) -> Result<(), WaitError> {
        if self.wait_handle.is_terminated() {
            if self.wait_handle.is_cancelled() {
                return Err(WaitError::Cancelled);
            }
            return Ok(());
        }

        self.abort_handle.abort();
        self.wait_handle.wait().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::{mpsc, oneshot};

    #[derive(Debug, PartialEq)]
    enum State {
        Released,
        Terminated,
        Triggered,
    }

    #[tokio::test]
    async fn abort_wait_async_non_blocking_task() {
        let (trigger_sender, trigger_receiver) = oneshot::channel::<()>();
        let (_release_sender, release_receiver) = oneshot::channel::<()>();

        let (mut state_sender_abrt, mut state_receiver_abrt) = mpsc::channel(1);
        let (shared, mut handle) = abortable_waitable(async move {
            let _ = trigger_receiver.await;
            state_sender_abrt.send(State::Triggered).await.unwrap();

            let _ = release_receiver.await;
            state_sender_abrt.send(State::Released).await.unwrap();
        });

        let (mut state_sender_spawn, mut state_receiver_spawn) = mpsc::channel(1);
        tokio::spawn(async move {
            let _ = shared.await;
            state_sender_spawn.send(State::Terminated).await.unwrap();
        });

        drop(trigger_sender);

        assert_eq!(state_receiver_abrt.next().await, Some(State::Triggered));
        assert!(!handle.is_terminated());

        assert_eq!(handle.abort_and_wait().await, Ok(()));

        assert_eq!(state_receiver_spawn.next().await, Some(State::Terminated));
        assert_eq!(state_receiver_abrt.next().await, None);
    }

    #[tokio::test]
    async fn abort_wait_blocking_task() {
        let (trigger_sender, trigger_receiver) = oneshot::channel::<()>();
        let (release_sender, release_receiver) = oneshot::channel::<()>();

        let (mut state_sender_abrt, mut state_receiver_abrt) = mpsc::channel(1);
        let (shared, mut handle) = abortable_waitable(async move {
            let _ = trigger_receiver.await;
            state_sender_abrt.send(State::Triggered).await.unwrap();

            let _ = release_receiver.await;
            state_sender_abrt.send(State::Released).await.unwrap();
        });

        let (mut state_sender_spawn, mut state_receiver_spawn) = mpsc::channel(1);
        tokio::spawn(async move {
            let _ = shared.await;
            state_sender_spawn.send(State::Terminated).await.unwrap();
        });

        drop(trigger_sender);
        assert_eq!(state_receiver_abrt.next().await, Some(State::Triggered));
        assert!(!handle.is_terminated());

        drop(release_sender);
        assert_eq!(state_receiver_abrt.next().await, Some(State::Released));

        assert_eq!(handle.abort_and_wait().await, Ok(()));

        assert_eq!(state_receiver_spawn.next().await, Some(State::Terminated));
        assert_eq!(state_receiver_abrt.next().await, None);
    }

    #[tokio::test]
    async fn abort_only() {
        let (trigger_sender, trigger_receiver) = oneshot::channel::<()>();
        let (_release_sender, release_receiver) = oneshot::channel::<()>();

        let (mut state_sender_abrt, mut state_receiver_abrt) = mpsc::channel(1);
        let (shared, mut handle) = abortable_waitable(async move {
            let _ = trigger_receiver.await;
            state_sender_abrt.send(State::Triggered).await.unwrap();

            let _ = release_receiver.await;
            state_sender_abrt.send(State::Released).await.unwrap();
        });

        let (mut state_sender_spawn, mut state_receiver_spawn) = mpsc::channel(1);
        tokio::spawn(async move {
            let _ = shared.await;
            state_sender_spawn.send(State::Terminated).await.unwrap();
        });

        assert!(!handle.is_terminated());
        drop(trigger_sender);

        assert_eq!(state_receiver_abrt.next().await, Some(State::Triggered));
        assert!(!handle.is_terminated());

        handle.abort();

        assert_eq!(state_receiver_spawn.next().await, Some(State::Terminated));
    }
}
