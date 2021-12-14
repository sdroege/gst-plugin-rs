// This is based on https://github.com/smol-rs/async-io
// with adaptations by:
//
// Copyright (C) 2021 François Laignel <fengalin@free.fr>
//
// Take a look at the license at the top of the repository in the LICENSE file.

use futures::stream::Stream;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use super::Reactor;

/// A future or stream that emits timed events.
///
/// Timers are futures that output a single [`Instant`] when they fire.
///
/// Timers are also streams that can output [`Instant`]s periodically.
#[derive(Debug)]
pub struct Timer {
    /// This timer's ID and last waker that polled it.
    ///
    /// When this field is set to `None`, this timer is not registered in the reactor.
    id_and_waker: Option<(usize, Waker)>,

    /// The next instant at which this timer fires.
    when: Instant,

    /// The period.
    period: Duration,
}

impl Timer {
    /// Creates a timer that emits an event once after the given duration of time.
    ///
    /// When throttling is activated (i.e. when using a non-`0` `wait`
    /// duration in `Context::acquire`), timer entries are assigned to
    /// the nearest time frame, meaning that the delay might elapse
    /// `wait` / 2 ms earlier or later than the expected instant.
    ///
    /// Use [`Timer::after_at_least`] when it's preferable not to return
    /// before the expected instant.
    pub fn after(duration: Duration) -> Timer {
        Timer::at(Instant::now() + duration)
    }

    /// Creates a timer that emits an event once after the given duration of time.
    ///
    /// See [`Timer::after`] for details. The event won't be emitted before
    /// the expected delay has elapsed.
    #[track_caller]
    pub fn after_at_least(duration: Duration) -> Timer {
        Timer::at_least_at(Instant::now() + duration)
    }

    /// Creates a timer that emits an event once at the given time instant.
    ///
    /// When throttling is activated (i.e. when using a non-`0` `wait`
    /// duration in `Context::acquire`), timer entries are assigned to
    /// the nearest time frame, meaning that the delay might elapse
    /// `wait` / 2 ms earlier or later than the expected instant.
    ///
    /// Use [`Timer::at_least_at`] when it's preferable not to return
    /// before the expected instant.
    pub fn at(instant: Instant) -> Timer {
        Timer::interval_at(instant, Duration::MAX)
    }

    /// Creates a timer that emits an event once at the given time instant.
    ///
    /// See [`Timer::at`] for details. The event won't be emitted before
    /// the expected delay has elapsed.
    #[track_caller]
    pub fn at_least_at(instant: Instant) -> Timer {
        Timer::interval_at_least_at(instant, Duration::MAX)
    }

    /// Creates a timer that emits events periodically.
    pub fn interval(period: Duration) -> Timer {
        Timer::interval_at(Instant::now() + period, period)
    }

    /// Creates a timer that emits events periodically, starting at `start`.
    ///
    /// When throttling is activated (i.e. when using a non-`0` `wait`
    /// duration in `Context::acquire`), timer entries are assigned to
    /// the nearest time frame, meaning that the delay might elapse
    /// `wait` / 2 ms earlier or later than the expected instant.
    ///
    /// Use [`Timer::interval_at_least_at`] when it's preferable not to return
    /// before the expected instant.
    pub fn interval_at(start: Instant, period: Duration) -> Timer {
        Timer {
            id_and_waker: None,
            when: start,
            period,
        }
    }

    /// Creates a timer that emits events periodically, starting at `start`.
    ///
    /// See [`Timer::interval_at`] for details. The event won't be emitted before
    /// the expected delay has elapsed.
    #[track_caller]
    pub fn interval_at_least_at(start: Instant, period: Duration) -> Timer {
        Timer {
            id_and_waker: None,
            when: start + Reactor::with(|reactor| reactor.half_max_throttling()),
            period,
        }
    }

    /// Sets the timer to emit an en event once after the given duration of time.
    ///
    /// Note that resetting a timer is different from creating a new timer because
    /// [`set_after()`][`Timer::set_after()`] does not remove the waker associated with the task
    /// that is polling the timer.
    pub fn set_after(&mut self, duration: Duration) {
        self.set_at(Instant::now() + duration);
    }

    /// Sets the timer to emit an event once at the given time instant.
    ///
    /// Note that resetting a timer is different from creating a new timer because
    /// [`set_at()`][`Timer::set_at()`] does not remove the waker associated with the task
    /// that is polling the timer.
    pub fn set_at(&mut self, instant: Instant) {
        Reactor::with_mut(|reactor| {
            if let Some((id, _)) = self.id_and_waker.as_ref() {
                // Deregister the timer from the reactor.
                reactor.remove_timer(self.when, *id);
            }

            // Update the timeout.
            self.when = instant;

            if let Some((id, waker)) = self.id_and_waker.as_mut() {
                // Re-register the timer with the new timeout.
                *id = reactor.insert_timer(self.when, waker);
            }
        })
    }

    /// Sets the timer to emit events periodically.
    ///
    /// Note that resetting a timer is different from creating a new timer because
    /// [`set_interval()`][`Timer::set_interval()`] does not remove the waker associated with the
    /// task that is polling the timer.
    pub fn set_interval(&mut self, period: Duration) {
        self.set_interval_at(Instant::now() + period, period);
    }

    /// Sets the timer to emit events periodically, starting at `start`.
    ///
    /// Note that resetting a timer is different from creating a new timer because
    /// [`set_interval_at()`][`Timer::set_interval_at()`] does not remove the waker associated with
    /// the task that is polling the timer.
    pub fn set_interval_at(&mut self, start: Instant, period: Duration) {
        // Note: the timer might have been registered on an Executor and then transfered to another.
        Reactor::with_mut(|reactor| {
            if let Some((id, _)) = self.id_and_waker.as_ref() {
                // Deregister the timer from the reactor.
                reactor.remove_timer(self.when, *id);
            }

            self.when = start;
            self.period = period;

            if let Some((id, waker)) = self.id_and_waker.as_mut() {
                // Re-register the timer with the new timeout.
                *id = reactor.insert_timer(self.when, waker);
            }
        })
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some((id, _)) = self.id_and_waker.take() {
            Reactor::with_mut(|reactor| {
                reactor.remove_timer(self.when, id);
            });
        }
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.poll_next(cx) {
            Poll::Ready(Some(_)) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => unreachable!(),
        }
    }
}

impl Stream for Timer {
    type Item = ();

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Reactor::with_mut(|reactor| {
            if Instant::now() + reactor.half_max_throttling() >= self.when {
                if let Some((id, _)) = self.id_and_waker.take() {
                    // Deregister the timer from the reactor.
                    reactor.remove_timer(self.when, id);
                }
                let when = self.when;
                if let Some(next) = when.checked_add(self.period) {
                    self.when = next;
                    // Register the timer in the reactor.
                    let id = reactor.insert_timer(self.when, cx.waker());
                    self.id_and_waker = Some((id, cx.waker().clone()));
                }

                Poll::Ready(Some(()))
            } else {
                match &self.id_and_waker {
                    None => {
                        // Register the timer in the reactor.
                        let id = reactor.insert_timer(self.when, cx.waker());
                        self.id_and_waker = Some((id, cx.waker().clone()));
                    }
                    Some((id, w)) if !w.will_wake(cx.waker()) => {
                        // Deregister the timer from the reactor to remove the old waker.
                        reactor.remove_timer(self.when, *id);

                        // Register the timer in the reactor with the new waker.
                        let id = reactor.insert_timer(self.when, cx.waker());
                        self.id_and_waker = Some((id, cx.waker().clone()));
                    }
                    Some(_) => {}
                }

                Poll::Pending
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::Timer;
    use crate::runtime::executor::Scheduler;

    const MAX_THROTTLING: Duration = Duration::from_millis(10);
    const DELAY: Duration = Duration::from_millis(12);

    #[test]
    fn delay_for() {
        gst::init().unwrap();

        let handle = Scheduler::start("delay_for", MAX_THROTTLING);

        let elapsed = futures::executor::block_on(handle.spawn(async {
            let now = Instant::now();
            Timer::after(DELAY).await;
            now.elapsed()
        }))
        .unwrap();

        // Due to throttling, timer may be fired earlier
        assert!(elapsed + MAX_THROTTLING / 2 >= DELAY);
    }

    #[test]
    fn delay_for_at_least() {
        gst::init().unwrap();

        let handle = Scheduler::start("delay_for_at_least", MAX_THROTTLING);

        let elapsed = futures::executor::block_on(handle.spawn(async {
            let now = Instant::now();
            Timer::after_at_least(DELAY).await;
            now.elapsed()
        }))
        .unwrap();

        // Never returns earlier than DELAY
        assert!(elapsed >= DELAY);
    }

    #[test]
    fn interval() {
        use futures::prelude::*;

        gst::init().unwrap();

        let handle = Scheduler::start("interval", MAX_THROTTLING);

        let join_handle = handle.spawn(async move {
            let start = Instant::now();
            let mut interval = Timer::interval(DELAY);

            interval.next().await;
            // Due to throttling, timer may be fired earlier
            assert!(start.elapsed() + MAX_THROTTLING / 2 >= DELAY);

            interval.next().await;
            // Due to throttling, timer may be fired earlier
            assert!(start.elapsed() + MAX_THROTTLING >= 2 * DELAY);
        });

        futures::executor::block_on(join_handle).unwrap();
    }
}
