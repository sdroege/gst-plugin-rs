// Copyright (C) 2020 François Laignel <fengalin@free.fr>
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

//! Wrappers for the underlying runtime specific time related Futures.

use futures::prelude::*;
use futures::stream::StreamExt;

use std::time::Duration;

use super::Context;

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
pub async fn delay_for(delay: Duration) {
    if delay > Duration::ZERO {
        tokio::time::delay_for(delay).map(drop).await;
    }
}

/// Wait until at least the given `delay` has elapsed.
///
/// This must be called from within the target runtime environment.
///
/// See [`delay_for`] for details. This method won't return before
/// the expected delay has elapsed.
pub async fn delay_for_at_least(delay: Duration) {
    if delay > Duration::ZERO {
        tokio::time::delay_for(
            delay + Context::current().map_or(Duration::ZERO, |ctx| ctx.wait_duration() / 2),
        )
        .map(drop)
        .await;
    }
}

/// Builds a `Stream` that yields at `interval.
///
/// This must be called from within the target runtime environment.
pub fn interval(interval: Duration) -> impl Stream<Item = ()> {
    tokio::time::interval(interval).map(drop)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::runtime::{executor, Context};

    const MAX_THROTTLING: Duration = Duration::from_millis(10);
    const DELAY: Duration = Duration::from_millis(12);

    #[test]
    fn delay_for() {
        gst::init().unwrap();

        let context = Context::acquire("delay_for", MAX_THROTTLING).unwrap();

        let elapsed = executor::block_on(context.spawn(async {
            let now = Instant::now();
            crate::runtime::time::delay_for(DELAY).await;
            now.elapsed()
        }))
        .unwrap();

        // Due to throttling, timer may be fired earlier
        assert!(elapsed + MAX_THROTTLING / 2 >= DELAY);
    }

    #[test]
    fn delay_for_at_least() {
        gst::init().unwrap();

        let context = Context::acquire("delay_for_at_least", MAX_THROTTLING).unwrap();

        let elapsed = executor::block_on(context.spawn(async {
            let now = Instant::now();
            crate::runtime::time::delay_for_at_least(DELAY).await;
            now.elapsed()
        }))
        .unwrap();

        // Never returns earlier that DELAY
        assert!(elapsed >= DELAY);
    }
}
