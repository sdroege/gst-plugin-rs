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
//! Current implementation uses the crate [`tokio`].
//!
//! Most `Element`s implementations should use the high-level features provided by [`PadSrc`] &
//! [`PadSink`].
//!
//! [talk]: https://gstconf.ubicast.tv/videos/when-adding-more-threads-adds-more-problems-thread-sharing-between-elements-in-gstreamer/
//! [slides]: https://gstreamer.freedesktop.org/data/events/gstreamer-conference/2018/Sebastian%20Dr%C3%B6ge%20-%20When%20adding%20more%20threads%20adds%20more%20problems:%20Thread-sharing%20between%20elements%20in%20GStreamer.pdf
//! [blog post]: https://coaxion.net/blog/2018/04/improving-gstreamer-performance-on-a-high-number-of-network-streams-by-sharing-threads-between-elements-with-rusts-tokio-crate
//! [`tokio`]: https://crates.io/crates/tokio
//! [`PadSrc`]: pad/struct.PadSrc.html
//! [`PadSink`]: pad/struct.PadSink.html

pub mod executor;
pub use executor::{Context, JoinHandle, SubTaskOutput};

pub mod pad;
pub use pad::{PadSink, PadSinkRef, PadSinkWeak, PadSrc, PadSrcRef, PadSrcWeak};

pub mod task;
pub use task::{Task, TaskState};

pub mod prelude {
    pub use super::pad::{PadSinkHandler, PadSrcHandler};
    pub use super::task::TaskImpl;
}

pub mod time;

use once_cell::sync::Lazy;

static RUNTIME_CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-runtime",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing Runtime"),
    )
});
