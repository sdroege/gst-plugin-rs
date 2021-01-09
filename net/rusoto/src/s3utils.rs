// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use bytes::{buf::BufMut, Bytes, BytesMut};
use futures::stream::TryStreamExt;
use futures::{future, Future};
use once_cell::sync::Lazy;
use rusoto_core::ByteStream;
use std::sync::Mutex;
use tokio::runtime;

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
