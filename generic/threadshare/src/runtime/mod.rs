// Copyright (C) 2019-2021 François Laignel <fengalin@free.fr>
//
// Take a look at the license at the top of the repository in the LICENSE file.

//! A `runtime` for the `threadshare` GStreamer plugins framework.
//!
//! Many `GStreamer` `Element`s internally spawn OS `thread`s. For most applications, this is not an
//! issue. However, in applications which process many `Stream`s in parallel, the high number of
//! `threads` leads to reduced efficiency due to:
//!
//! * context switches,
//! * scheduler overhead,
//! * most of the threads waiting for some resources to be available.
//!
//! The `threadshare` `runtime` is a framework to build `Element`s for such applications. It
//! uses light-weight threading to allow multiple `Element`s share a reduced number of OS `thread`s.
//!
//! See this [talk] ([slides]) for a presentation of the motivations and principles,
//! and this [blog post].
//!
//! Current implementation uses a custom executor mostly based on the [`smol`] ecosystem.
//!
//! Most `Element`s implementations should use the high-level features provided by [`PadSrc`] &
//! [`PadSink`].
//!
//! [talk]: https://gstconf.ubicast.tv/videos/when-adding-more-threads-adds-more-problems-thread-sharing-between-elements-in-gstreamer/
//! [slides]: https://gstreamer.freedesktop.org/data/events/gstreamer-conference/2018/Sebastian%20Dr%C3%B6ge%20-%20When%20adding%20more%20threads%20adds%20more%20problems:%20Thread-sharing%20between%20elements%20in%20GStreamer.pdf
//! [blog post]: https://coaxion.net/blog/2018/04/improving-gstreamer-performance-on-a-high-number-of-network-streams-by-sharing-threads-between-elements-with-rusts-tokio-crate
//! [`smol`]: https://github.com/smol-rs/
//! [`PadSrc`]: pad/struct.PadSrc.html
//! [`PadSink`]: pad/struct.PadSink.html

pub mod executor;
pub use executor::{Async, Context, JoinHandle, SubTaskOutput, Timer};

pub mod pad;
pub use pad::{PadSink, PadSinkRef, PadSinkWeak, PadSrc, PadSrcRef, PadSrcWeak};

pub mod task;
pub use task::{Task, TaskState};

pub mod prelude {
    pub use super::pad::{PadSinkHandler, PadSrcHandler};
    pub use super::task::TaskImpl;
}

pub mod time;
pub use time::{delay_for, delay_for_at_least};

use once_cell::sync::Lazy;

static RUNTIME_CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-runtime",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing Runtime"),
    )
});
