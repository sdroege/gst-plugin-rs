// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use futures::sync::oneshot;
use futures::Future;
use std::sync::Mutex;
use tokio::runtime;

pub fn wait<F, T>(
    canceller: &Mutex<Option<oneshot::Sender<T>>>,
    runtime: &runtime::Runtime,
    future: F,
) -> Result<F::Item, Option<gst::ErrorMessage>>
where
    F: Send + Future<Error = gst::ErrorMessage> + 'static,
    F::Item: Send,
{
    let mut canceller_guard = canceller.lock().unwrap();
    let (sender, receiver) = oneshot::channel::<T>();

    canceller_guard.replace(sender);
    drop(canceller_guard);

    let unlock_error = gst_error_msg!(gst::ResourceError::Busy, ["unlock"]);

    let res = oneshot::spawn(future, &runtime.executor())
        .select(receiver.then(|_| Err(unlock_error.clone())))
        .wait()
        .map(|v| v.0)
        .map_err(|err| {
            if err.0 == unlock_error {
                None
            } else {
                Some(err.0)
            }
        });

    /* Clear out the canceller */
    canceller_guard = canceller.lock().unwrap();
    *canceller_guard = None;

    res
}
