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

use futures::channel::oneshot;
use futures::prelude::*;
use futures::ready;

use pin_project::pin_project;

use std::pin::Pin;
use std::task::{self, Poll};

/// Builds a [`Waitable`] `Future` from the provided `Future`.
///
/// See [`WaitHandle`].
///
/// [`Waitable`]: struct.Waitable.html
/// [`WaitHandle`]: struct.WaitHandle.html
pub fn waitable<Fut: Future>(future: Fut) -> (Waitable<Fut>, WaitHandle) {
    Waitable::new(future)
}

/// A `Waitable` `Future`.
///
/// See [`WaitHandle`].
///
/// [`WaitHandle`]: struct.WaitHandle.html
#[pin_project]
#[derive(Debug)]
pub struct Waitable<Fut> {
    #[pin]
    inner: Fut,
    sender: Option<oneshot::Sender<()>>,
}

impl<Fut> Waitable<Fut> {
    pub fn new(inner: Fut) -> (Waitable<Fut>, WaitHandle) {
        let (sender, receiver) = oneshot::channel();

        (
            Waitable {
                inner,
                sender: Some(sender),
            },
            WaitHandle::new(receiver),
        )
    }
}

impl<Fut: Future> Future for Waitable<Fut> {
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Self::Output> {
        let this = self.project();
        let output = ready!(this.inner.poll(cx));
        if let Some(sender) = this.sender.take() {
            let _ = sender.send(());
        }
        Poll::Ready(output)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum WaitError {
    Cancelled,
}

/// A handle to a [`Waitable`] `Future`.
///
/// The handle allows checking for the `Future` completion and waiting until the `Future`
/// completes.
///
/// [`Waitable`]: struct.Waitable.html
#[derive(Debug)]
pub struct WaitHandle {
    receiver: oneshot::Receiver<()>,
    is_terminated: bool,
    is_cancelled: bool,
}

impl WaitHandle {
    fn new(receiver: oneshot::Receiver<()>) -> WaitHandle {
        WaitHandle {
            receiver,
            is_terminated: false,
            is_cancelled: false,
        }
    }

    pub fn is_terminated(&mut self) -> bool {
        self.check_state();
        self.is_terminated
    }

    pub fn is_cancelled(&mut self) -> bool {
        self.check_state();
        self.is_cancelled
    }

    pub async fn wait(self) -> Result<(), WaitError> {
        if self.is_terminated {
            if self.is_cancelled {
                return Err(WaitError::Cancelled);
            }
            return Ok(());
        }

        self.receiver.await.map_err(|_| WaitError::Cancelled)
    }

    fn check_state(&mut self) {
        if self.is_terminated {
            // no need to check state
            return;
        }

        let res = self.receiver.try_recv();

        match res {
            Ok(None) => (),
            Ok(Some(())) => self.is_terminated = true,
            Err(_) => {
                self.is_terminated = true;
                self.is_cancelled = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::channel::{mpsc, oneshot};
    use futures::future;

    use std::time::Duration;

    use tokio::future::FutureExt;

    use super::*;

    #[derive(Debug, PartialEq)]
    enum State {
        Released,
        Terminated,
        Triggered,
    }

    #[tokio::test]
    async fn wait() {
        let (trigger_sender, trigger_receiver) = oneshot::channel::<()>();

        let (mut state_sender_wait, mut state_receiver_wait) = mpsc::channel(1);
        let (shared, mut handle) = waitable(async move {
            let _ = trigger_receiver.await;
            state_sender_wait.send(State::Triggered).await.unwrap();
            let _ = future::pending::<()>()
                .timeout(Duration::from_secs(1))
                .await;
            state_sender_wait.send(State::Released).await.unwrap();
        });

        let (mut state_sender_spawn, mut state_receiver_spawn) = mpsc::channel(1);
        tokio::spawn(async move {
            let _ = shared.await;
            state_sender_spawn.send(State::Terminated).await.unwrap();
        });

        drop(trigger_sender);

        assert_eq!(state_receiver_wait.next().await, Some(State::Triggered));
        assert!(!handle.is_terminated());

        assert_eq!(handle.wait().await, Ok(()));
        assert_eq!(state_receiver_wait.next().await, Some(State::Released));

        assert_eq!(state_receiver_spawn.next().await, Some(State::Terminated));
        assert_eq!(state_receiver_wait.next().await, None);
    }

    #[tokio::test]
    async fn no_wait() {
        let (trigger_sender, trigger_receiver) = oneshot::channel::<()>();

        let (mut state_sender_wait, mut state_receiver_wait) = mpsc::channel(1);
        let (shared, mut handle) = waitable(async move {
            let _ = trigger_receiver.await;
            state_sender_wait.send(State::Triggered).await.unwrap();
            state_sender_wait.send(State::Released).await.unwrap();
        });

        let (mut state_sender_spawn, mut state_receiver_spawn) = mpsc::channel(1);
        tokio::spawn(async move {
            let _ = shared.await;
            state_sender_spawn.send(State::Terminated).await.unwrap();
        });

        drop(trigger_sender);

        assert_eq!(state_receiver_wait.next().await, Some(State::Triggered));
        assert!(!handle.is_terminated());

        assert_eq!(state_receiver_wait.next().await, Some(State::Released));

        assert_eq!(state_receiver_spawn.next().await, Some(State::Terminated));
        assert_eq!(state_receiver_wait.next().await, None);

        assert!(handle.is_terminated());
        assert!(!handle.is_cancelled());
    }
}
