// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#[macro_export]
macro_rules! panic_to_error(
    ($element:expr, $panicked:expr, $ret:expr, $code:block) => {{
        use std::panic::{self, AssertUnwindSafe};
        use std::sync::atomic::Ordering;

        if $panicked.load(Ordering::Relaxed) {
            $element.post_error_message(&gst_error_msg!(gst::LibraryError::Failed, ["Panicked"]));
            $ret
        } else {
            let result = panic::catch_unwind(AssertUnwindSafe(|| $code));

            match result {
                Ok(result) => result,
                Err(err) => {
                    $panicked.store(true, Ordering::Relaxed);
                    if let Some(cause) = err.downcast_ref::<&str>() {
                        $element.post_error_message(&gst_error_msg!(gst::LibraryError::Failed, ["Panicked: {}", cause]));
                    } else if let Some(cause) = err.downcast_ref::<String>() {
                        $element.post_error_message(&gst_error_msg!(gst::LibraryError::Failed, ["Panicked: {}", cause]));
                    } else {
                        $element.post_error_message(&gst_error_msg!(gst::LibraryError::Failed, ["Panicked"]));
                    }
                    $ret
                }
            }
        }
    }};
);
