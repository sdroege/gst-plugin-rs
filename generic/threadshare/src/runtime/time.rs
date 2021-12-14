// Copyright (C) 2020-2021 François Laignel <fengalin@free.fr>
//
// Take a look at the license at the top of the repository in the LICENSE file.

//! Wrappers for the underlying runtime specific time related Futures.

use std::time::Duration;

use super::executor::Timer;

/// Wait until the given `delay` has elapsed.
///
/// This must be called from within the target runtime environment.
///
/// When throttling is activated (i.e. when using a non-`0` `wait`
/// duration in `Context::acquire`), timer entries are assigned to
/// the nearest time frame, meaning that the delay might elapse
/// `wait` / 2 ms earlier or later than the expected instant.
///
/// Use [`delay_for_at_least`] when it's preferable not to return
/// before the expected instant.
pub fn delay_for(delay: Duration) -> Timer {
    Timer::after(delay)
}

/// Wait until at least the given `delay` has elapsed.
///
/// This must be called from within the target runtime environment.
///
/// See [`delay_for`] for details. This method won't return before
/// the expected delay has elapsed.
#[track_caller]
pub fn delay_for_at_least(delay: Duration) -> Timer {
    Timer::after_at_least(delay)
}

/// Builds a `Stream` that yields at `interval`.
///
/// This must be called from within the target runtime environment.
pub fn interval(interval: Duration) -> Timer {
    Timer::interval(interval)
}
