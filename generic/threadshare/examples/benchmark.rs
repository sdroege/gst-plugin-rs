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

use gst::glib;
use gst::prelude::*;

use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const THROUGHPUT_PERIOD: Duration = Duration::from_secs(20);

fn main() {
    gst::init().unwrap();

    #[cfg(debug_assertions)]
    {
        use std::path::Path;

        let mut path = Path::new("target/debug");
        if !path.exists() {
            path = Path::new("../../target/debug");
        }

        gst::Registry::get().scan_path(path);
    }
    #[cfg(not(debug_assertions))]
    {
        use std::path::Path;

        let mut path = Path::new("target/release");
        if !path.exists() {
            path = Path::new("../../target/release");
        }

        gst::Registry::get().scan_path(path);
    }

    let args = env::args().collect::<Vec<_>>();
    assert_eq!(args.len(), 6);
    let n_streams: u16 = args[1].parse().unwrap();
    let source = &args[2];
    let n_groups: u32 = args[3].parse().unwrap();
    let wait: u32 = args[4].parse().unwrap();

    let l = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new(None);
    let counter = Arc::new(AtomicU64::new(0));

    for i in 0..n_streams {
        let sink =
            gst::ElementFactory::make("fakesink", Some(format!("sink-{}", i).as_str())).unwrap();
        sink.set_property("sync", &false).unwrap();
        sink.set_property("async", &false).unwrap();

        let counter_clone = Arc::clone(&counter);
        sink.static_pad("sink").unwrap().add_probe(
            gst::PadProbeType::BUFFER,
            move |_pad, _probe_info| {
                let _ = counter_clone.fetch_add(1, Ordering::SeqCst);
                gst::PadProbeReturn::Ok
            },
        );

        let source = match source.as_str() {
            "udpsrc" => {
                let source =
                    gst::ElementFactory::make("udpsrc", Some(format!("source-{}", i).as_str()))
                        .unwrap();
                source
                    .set_property("port", &(40000i32 + (i as i32)))
                    .unwrap();
                source
                    .set_property("retrieve-sender-address", &false)
                    .unwrap();

                source
            }
            "ts-udpsrc" => {
                let source =
                    gst::ElementFactory::make("ts-udpsrc", Some(format!("source-{}", i).as_str()))
                        .unwrap();
                source
                    .set_property("port", &(40000i32 + (i as i32)))
                    .unwrap();
                source
                    .set_property("context", &format!("context-{}", (i as u32) % n_groups))
                    .unwrap();
                source.set_property("context-wait", &wait).unwrap();

                source
            }
            "tcpclientsrc" => {
                let source = gst::ElementFactory::make(
                    "tcpclientsrc",
                    Some(format!("source-{}", i).as_str()),
                )
                .unwrap();
                source.set_property("host", &"127.0.0.1").unwrap();
                source.set_property("port", &40000i32).unwrap();

                source
            }
            "ts-tcpclientsrc" => {
                let source = gst::ElementFactory::make(
                    "ts-tcpclientsrc",
                    Some(format!("source-{}", i).as_str()),
                )
                .unwrap();
                source.set_property("host", &"127.0.0.1").unwrap();
                source.set_property("port", &40000i32).unwrap();
                source
                    .set_property("context", &format!("context-{}", (i as u32) % n_groups))
                    .unwrap();
                source.set_property("context-wait", &wait).unwrap();

                source
            }
            "tonegeneratesrc" => {
                let source = gst::ElementFactory::make(
                    "tonegeneratesrc",
                    Some(format!("source-{}", i).as_str()),
                )
                .unwrap();
                source
                    .set_property("samplesperbuffer", &((wait as i32) * 8000 / 1000))
                    .unwrap();

                sink.set_property("sync", &true).unwrap();

                source
            }
            "ts-tonesrc" => {
                let source =
                    gst::ElementFactory::make("ts-tonesrc", Some(format!("source-{}", i).as_str()))
                        .unwrap();
                source
                    .set_property("samples-per-buffer", &((wait as u32) * 8000 / 1000))
                    .unwrap();
                source
                    .set_property("context", &format!("context-{}", (i as u32) % n_groups))
                    .unwrap();
                source.set_property("context-wait", &wait).unwrap();

                source
            }
            _ => unimplemented!(),
        };

        pipeline.add_many(&[&source, &sink]).unwrap();
        source.link(&sink).unwrap();
    }

    let bus = pipeline.bus().unwrap();
    let l_clone = l.clone();
    bus.add_watch(move |_, msg| {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => l_clone.quit(),
            MessageView::Error(err) => {
                println!(
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

    println!("started");

    thread::spawn(move || {
        let throughput_factor = 1_000f32 / (n_streams as f32);
        let mut prev_reset_instant: Option<Instant> = None;
        let mut count;
        let mut reset_instant;

        loop {
            count = counter.fetch_and(0, Ordering::SeqCst);
            reset_instant = Instant::now();

            if let Some(prev_reset_instant) = prev_reset_instant {
                println!(
                    "{:>5.1} / s / stream",
                    (count as f32) * throughput_factor
                        / ((reset_instant - prev_reset_instant).as_millis() as f32)
                );
            }

            if let Some(sleep_duration) = THROUGHPUT_PERIOD.checked_sub(reset_instant.elapsed()) {
                thread::sleep(sleep_duration);
            }

            prev_reset_instant = Some(reset_instant);
        }
    });

    l.run();
}
