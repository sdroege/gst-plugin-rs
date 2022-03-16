// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use bytes::{buf::BufMut, Bytes, BytesMut};
use futures::{future, Future, FutureExt, TryFutureExt, TryStreamExt};
use once_cell::sync::Lazy;
use rusoto_core::RusotoError::{HttpDispatch, Unknown};
use rusoto_core::{ByteStream, HttpDispatchError, RusotoError};
use std::sync::Mutex;
use std::time::Duration;
use tokio::runtime;

use gst::gst_warning;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rusotos3utils",
        gst::DebugColorFlags::empty(),
        Some("Amazon S3 utilities"),
    )
});

static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("gst-rusoto-runtime")
        .build()
        .unwrap()
});

pub enum WaitError<E> {
    Cancelled,
    FutureError(E),
}

fn make_timeout<F, T, E>(
    timeout: Duration,
    future: F,
) -> impl Future<Output = Result<T, RusotoError<E>>>
where
    E: std::fmt::Debug,
    F: Future<Output = Result<T, RusotoError<E>>>,
{
    tokio::time::timeout(timeout, future).map(|v| match v {
        // Future resolved succesfully
        Ok(Ok(v)) => Ok(v),
        // Future resolved with an error
        Ok(Err(e)) => Err(e),
        // Timeout elapsed
        // Use an HttpDispatch error so the caller doesn't have to deal with this separately from
        // other HTTP dispatch errors
        _ => Err(HttpDispatch(HttpDispatchError::new("Timeout".to_owned()))),
    })
}

fn make_retry<F, T, E, Fut>(
    timeout: Option<Duration>,
    mut future: F,
) -> impl Future<Output = Result<T, RusotoError<E>>>
where
    E: std::fmt::Debug,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, RusotoError<E>>>,
{
    backoff::future::retry(
        backoff::ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_millis(500))
            .with_multiplier(1.5)
            .with_max_elapsed_time(timeout)
            .build(),
        move || {
            future().map_err(|err| match err {
                HttpDispatch(_) => {
                    gst_warning!(CAT, "Error waiting for operation ({:?}), retrying", err);
                    backoff::Error::transient(err)
                }
                Unknown(ref response) => {
                    gst_warning!(
                        CAT,
                        "Unknown error waiting for operation ({:?}), retrying",
                        response
                    );

                    // Retry on 5xx errors
                    if response.status.is_server_error() {
                        backoff::Error::transient(err)
                    } else {
                        backoff::Error::permanent(err)
                    }
                }
                _ => backoff::Error::permanent(err),
            })
        },
    )
}

pub fn wait_retry<F, T, E, Fut>(
    canceller: &Mutex<Option<future::AbortHandle>>,
    req_timeout: Option<Duration>,
    retry_timeout: Option<Duration>,
    mut future: F,
) -> Result<T, WaitError<RusotoError<E>>>
where
    E: std::fmt::Debug,
    F: FnMut() -> Fut,
    Fut: Send + Future<Output = Result<T, RusotoError<E>>>,
    Fut::Output: Send,
    T: Send,
    E: Send,
{
    let mut canceller_guard = canceller.lock().unwrap();
    let (abort_handle, abort_registration) = future::AbortHandle::new_pair();

    canceller_guard.replace(abort_handle);
    drop(canceller_guard);

    let res = {
        let _enter = RUNTIME.enter();

        futures::executor::block_on(async {
            // The order of this future stack matters: the innermost future is the supplied future
            // generator closure. We wrap that in a timeout to bound how long we wait. This, in
            // turn, is wrapped in a retrying future which will make multiple attempts until it
            // ultimately fails.
            // The timeout must be created within the tokio executor
            let res = match req_timeout {
                None => {
                    let retry_future = make_retry(retry_timeout, future);
                    future::Abortable::new(retry_future, abort_registration).await
                }
                Some(t) => {
                    let timeout_future = || make_timeout(t, future());
                    let retry_future = make_retry(retry_timeout, timeout_future);
                    future::Abortable::new(retry_future, abort_registration).await
                }
            };

            match res {
                // Future resolved successfully
                Ok(Ok(res)) => Ok(res),
                // Future resolved with an error
                Ok(Err(err)) => Err(WaitError::FutureError(err)),
                // Canceller called before future resolved
                Err(future::Aborted) => Err(WaitError::Cancelled),
            }
        })
    };

    /* Clear out the canceller */
    canceller_guard = canceller.lock().unwrap();
    *canceller_guard = None;

    res
}

pub fn wait<F, T, E>(
    canceller: &Mutex<Option<future::AbortHandle>>,
    future: F,
) -> Result<T, WaitError<E>>
where
    F: Send + Future<Output = Result<T, E>>,
    F::Output: Send,
    T: Send,
    E: Send,
{
    let mut canceller_guard = canceller.lock().unwrap();
    let (abort_handle, abort_registration) = future::AbortHandle::new_pair();

    canceller_guard.replace(abort_handle);
    drop(canceller_guard);

    let abortable_future = future::Abortable::new(future, abort_registration);

    // FIXME: add a timeout as well

    let res = {
        let _enter = RUNTIME.enter();
        futures::executor::block_on(async {
            match abortable_future.await {
                // Future resolved successfully
                Ok(Ok(res)) => Ok(res),
                // Future resolved with an error
                Ok(Err(err)) => Err(WaitError::FutureError(err)),
                // Canceller called before future resolved
                Err(future::Aborted) => Err(WaitError::Cancelled),
            }
        })
    };

    /* Clear out the canceller */
    canceller_guard = canceller.lock().unwrap();
    *canceller_guard = None;

    res
}

pub fn wait_stream(
    canceller: &Mutex<Option<future::AbortHandle>>,
    stream: &mut ByteStream,
) -> Result<Bytes, WaitError<std::io::Error>> {
    wait(canceller, async move {
        let mut collect = BytesMut::new();

        // Loop over the stream and collect till we're done
        while let Some(item) = stream.try_next().await? {
            collect.put(item)
        }

        Ok::<Bytes, std::io::Error>(collect.freeze())
    })
}
