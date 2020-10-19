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

/// Wait until the given `delay` has elapsed.
///
/// This must be called from within the target runtime environment.
pub async fn delay_for(delay: Duration) {
    if delay > Duration::from_nanos(0) {
        tokio::time::delay_for(delay).map(drop).await;
    }
}

/// Builds a `Stream` that yields at `interval.
///
/// This must be called from within the target runtime environment.
pub fn interval(interval: Duration) -> impl Stream<Item = ()> {
    tokio::time::interval(interval).map(drop)
}
