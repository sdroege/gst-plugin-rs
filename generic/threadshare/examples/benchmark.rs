// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
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
//
// SPDX-License-Identifier: LGPL-2.1-or-later

use gst::glib;
use gst::prelude::*;
use once_cell::sync::Lazy;

use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const THROUGHPUT_PERIOD: Duration = Duration::from_secs(20);

pub static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-benchmark",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing benchmarking receiver"),
    )
});

fn main() {
    gst::init().unwrap();
    // Register the plugins statically:
    // - The executable can be run from anywhere.
    // - No risk of running against a previous version.
    // - `main` can use features that rely on `static`s or `thread_local`
    //   such as `Context::acquire` which otherwise don't point to
    //   the same `static` or `thread_local`, probably because
    //   the shared object uses its owns and the executable, others.
    gstthreadshare::plugin_register_static().unwrap();

    let args = env::args().collect::<Vec<_>>();
    assert!(args.len() > 4);
    let n_streams: u16 = args[1].parse().unwrap();
    let source = &args[2];
    let n_groups: u32 = args[3].parse().unwrap();
    let wait: u32 = args[4].parse().unwrap();

    // Nb buffers to await before stopping.
    let max_buffers: Option<f32> = if args.len() > 5 {
        args[5].parse().ok()
    } else {
        None
    };
    let is_rtp = args.len() > 6 && (args[6] == "rtp");

    let rtp_caps = gst::Caps::builder("audio/x-rtp")
        .field("media", "audio")
        .field("payload", 8i32)
        .field("clock-rate", 8000)
        .field("encoding-name", "PCMA")
        .build();

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);
    let counter = Arc::new(AtomicU64::new(0));

    for i in 0..n_streams {
        let build_context = || format!("context-{}", (i as u32) % n_groups);

        let sink =
            gst::ElementFactory::make("fakesink", Some(format!("sink-{}", i).as_str())).unwrap();
        sink.set_property("sync", false);
        sink.set_property("async", false);
        sink.set_property("signal-handoffs", true);
        sink.connect(
            "handoff",
            true,
            glib::clone!(@strong counter => move |_| {
                let _ = counter.fetch_add(1, Ordering::SeqCst);
                None
            }),
        );

        let (source, context) = match source.as_str() {
            "udpsrc" => {
                let source =
                    gst::ElementFactory::make("udpsrc", Some(format!("source-{}", i).as_str()))
                        .unwrap();
                source.set_property("port", 40000i32 + i as i32);
                source.set_property("retrieve-sender-address", false);

                (source, None)
            }
            "ts-udpsrc" => {
                let context = build_context();
                let source =
                    gst::ElementFactory::make("ts-udpsrc", Some(format!("source-{}", i).as_str()))
                        .unwrap();
                source.set_property("port", 40000i32 + i as i32);
                source.set_property("context", &context);
                source.set_property("context-wait", wait);

                if is_rtp {
                    source.set_property("caps", &rtp_caps);
                }

                (source, Some(context))
            }
            "tcpclientsrc" => {
                let source = gst::ElementFactory::make(
                    "tcpclientsrc",
                    Some(format!("source-{}", i).as_str()),
                )
                .unwrap();
                source.set_property("host", "127.0.0.1");
                source.set_property("port", 40000i32);

                (source, None)
            }
            "ts-tcpclientsrc" => {
                let context = build_context();
                let source = gst::ElementFactory::make(
                    "ts-tcpclientsrc",
                    Some(format!("source-{}", i).as_str()),
                )
                .unwrap();
                source.set_property("host", "127.0.0.1");
                source.set_property("port", 40000i32);
                source.set_property("context", &context);
                source.set_property("context-wait", wait);

                (source, Some(context))
            }
            "tonegeneratesrc" => {
                let source = gst::ElementFactory::make(
                    "tonegeneratesrc",
                    Some(format!("source-{}", i).as_str()),
                )
                .unwrap();
                source.set_property("samplesperbuffer", (wait as i32) * 8000 / 1000);

                sink.set_property("sync", true);

                (source, None)
            }
            "ts-tonesrc" => {
                let context = build_context();
                let source =
                    gst::ElementFactory::make("ts-tonesrc", Some(format!("source-{}", i).as_str()))
                        .unwrap();
                source.set_property("samples-per-buffer", (wait as u32) * 8000 / 1000);
                source.set_property("context", &context);
                source.set_property("context-wait", wait);

                (source, Some(context))
            }
            _ => unimplemented!(),
        };

        if is_rtp {
            let jb =
                gst::ElementFactory::make("ts-jitterbuffer", Some(format!("jb-{}", i).as_str()))
                    .unwrap();
            if let Some(context) = context {
                jb.set_property("context", &context);
            }
            jb.set_property("context-wait", wait);
            jb.set_property("latency", wait);

            let elements = &[&source, &jb, &sink];
            pipeline.add_many(elements).unwrap();
            gst::Element::link_many(elements).unwrap();
        } else {
            let queue = if let Some(context) = context {
                let queue =
                    gst::ElementFactory::make("ts-queue", Some(format!("queue-{}", i).as_str()))
                        .unwrap();
                queue.set_property("context", &context);
                queue.set_property("context-wait", wait);
                queue
            } else {
                gst::ElementFactory::make("queue2", Some(format!("queue-{}", i).as_str())).unwrap()
            };

            let elements = &[&source, &queue, &sink];
            pipeline.add_many(elements).unwrap();
            gst::Element::link_many(elements).unwrap();
        }
    }

    let bus = pipeline.bus().unwrap();
    let l_clone = l.clone();
    bus.add_watch(move |_, msg| {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => l_clone.quit(),
            MessageView::Error(err) => {
                gst::error!(
                    CAT,
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                l_clone.quit();
            }
            _ => (),
        };

        glib::Continue(true)
    })
    .expect("Failed to add bus watch");

    pipeline.set_state(gst::State::Playing).unwrap();

    gst::info!(CAT, "started");

    let l_clone = l.clone();
    thread::spawn(move || {
        let n_streams_f32 = n_streams as f32;

        let mut total_count = 0.0;
        let mut ramp_up_complete_instant: Option<Instant> = None;

        #[cfg(feature = "tuning")]
        let ctx_0 = gstthreadshare::runtime::Context::acquire(
            "context-0",
            Duration::from_millis(wait as u64),
        )
        .unwrap();
        #[cfg(feature = "tuning")]
        let mut parked_init = Duration::ZERO;

        loop {
            total_count += counter.fetch_and(0, Ordering::SeqCst) as f32 / n_streams_f32;
            if let Some(max_buffers) = max_buffers {
                if total_count > max_buffers {
                    gst::info!(CAT, "Stopping");
                    let stopping_instant = Instant::now();
                    pipeline.set_state(gst::State::Ready).unwrap();
                    gst::info!(CAT, "Stopped. Took {:?}", stopping_instant.elapsed());
                    pipeline.set_state(gst::State::Null).unwrap();
                    gst::info!(CAT, "Unprepared");
                    l_clone.quit();
                    break;
                }
            }

            if let Some(init) = ramp_up_complete_instant {
                let elapsed = init.elapsed();
                gst::info!(
                    CAT,
                    "{:>6.2} / s / stream",
                    total_count * 1_000.0 / elapsed.as_millis() as f32
                );

                #[cfg(feature = "tuning")]
                gst::info!(
                    CAT,
                    "{:>6.2}% parked",
                    (ctx_0.parked_duration() - parked_init).as_nanos() as f32 * 100.0
                        / elapsed.as_nanos() as f32
                );
            } else {
                // Ramp up 30s worth of buffers before following parked
                if total_count > 50.0 * 30.0 {
                    total_count = 0.0;
                    ramp_up_complete_instant = Some(Instant::now());
                    #[cfg(feature = "tuning")]
                    {
                        parked_init = ctx_0.parked_duration();
                    }
                }
            }

            thread::sleep(THROUGHPUT_PERIOD);
        }
    });

    l.run();
}
