// Copyright (C) 2025 François Laignel <francois@centricular.com>
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

use futures::channel::oneshot;
use futures::prelude::*;
use gst::prelude::*;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare inter test");
    });
}

#[test]
fn one_to_one_down_first() {
    init();

    let pipe_up = gst::Pipeline::with_name("upstream::one_to_one_down_first");
    let audiotestsrc = gst::ElementFactory::make("audiotestsrc")
        .name("testsrc::one_to_one_down_first")
        .property("num-buffers", 20i32)
        .property("is-live", true)
        .build()
        .unwrap();
    let intersink = gst::ElementFactory::make("ts-intersink")
        .name("intersink::one_to_one_down_first")
        .property("inter-context", "inter::one_to_one_down_first")
        .build()
        .unwrap();

    let pipe_down = gst::Pipeline::with_name("downstream::one_to_one_down_first");
    let intersrc = gst::ElementFactory::make("ts-intersrc")
        .name("intersrc::one_to_one_down_first")
        .property("inter-context", "inter::one_to_one_down_first")
        .property("context", "inter::test")
        .property("context-wait", 20u32)
        .build()
        .unwrap();
    let appsink = gst_app::AppSink::builder()
        .name("appsink::one_to_one_down_first")
        .build();

    let upstream_elems = [&audiotestsrc, &intersink];
    pipe_up.add_many(upstream_elems).unwrap();
    gst::Element::link_many(upstream_elems).unwrap();

    pipe_down
        .add_many([&intersrc, appsink.upcast_ref()])
        .unwrap();
    intersrc.link(&appsink).unwrap();

    pipe_up.set_base_time(gst::ClockTime::ZERO);
    pipe_up.set_start_time(gst::ClockTime::NONE);
    pipe_down.set_base_time(gst::ClockTime::ZERO);
    pipe_down.set_start_time(gst::ClockTime::NONE);

    let samples = Arc::new(AtomicU32::new(0));
    let (eos_tx, mut eos_rx) = oneshot::channel::<()>();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample({
                let samples = samples.clone();
                move |appsink| {
                    let _ = appsink.pull_sample().unwrap();
                    samples.fetch_add(1, Ordering::SeqCst);

                    Ok(gst::FlowSuccess::Ok)
                }
            })
            .eos({
                let mut eos_tx = Some(eos_tx);
                move |_| eos_tx.take().unwrap().send(()).unwrap()
            })
            .build(),
    );

    // Starting downstream first => we will get all buffers
    pipe_down.set_state(gst::State::Playing).unwrap();
    pipe_up.set_state(gst::State::Playing).unwrap();

    let mut got_eos_evt = false;
    let mut got_eos_msg = false;

    futures::executor::block_on(async {
        use gst::MessageView::*;

        let mut bus_up_stream = pipe_up.bus().unwrap().stream();
        let mut bus_down_stream = pipe_down.bus().unwrap().stream();

        loop {
            futures::select! {
                _ = eos_rx => {
                    println!("inter::one_to_one_down_first got eos notif");
                    got_eos_evt = true;
                    if got_eos_msg {
                        break;
                    }
                }
                msg = bus_down_stream.next() => {
                    let Some(msg) = msg else { continue };
                    match msg.view() {
                        Latency(_) => {
                            let _ = pipe_down.recalculate_latency();
                        }
                        Eos(_) => {
                            println!("inter::one_to_one_down_first got eos msg");
                            got_eos_msg = true;
                            if got_eos_evt {
                                break;
                            }
                        }
                        Error(err) => unreachable!("inter::one_to_one_down_first {err:?}"),
                        _ => (),
                    }
                }
                msg = bus_up_stream.next() => {
                    let Some(msg) = msg else { continue };
                    match msg.view() {
                        Latency(_) => {
                            let _ = pipe_up.recalculate_latency();
                        }
                        Error(err) => unreachable!("inter::one_to_one_down_first {err:?}"),
                        _ => (),
                    }
                }
            };
        }
    });

    assert_eq!(samples.load(Ordering::SeqCst), 20);

    pipe_up.set_state(gst::State::Null).unwrap();
    pipe_down.set_state(gst::State::Null).unwrap();
}

#[test]
fn one_to_one_up_first() {
    init();

    let pipe_up = gst::Pipeline::with_name("upstream::one_to_one_up_first");
    let audiotestsrc = gst::ElementFactory::make("audiotestsrc")
        .name("testsrc::one_to_one_up_first")
        .property("is-live", true)
        .build()
        .unwrap();
    let opusenc = gst::ElementFactory::make("opusenc")
        .name("opusenc::one_to_one_down_first")
        .build()
        .unwrap();
    let intersink = gst::ElementFactory::make("ts-intersink")
        .name("intersink::one_to_one_up_first")
        .property("inter-context", "inter::one_to_one_up_first")
        .build()
        .unwrap();

    let pipe_down = gst::Pipeline::with_name("downstream::one_to_one_up_first");
    let intersrc = gst::ElementFactory::make("ts-intersrc")
        .name("intersrc::one_to_one_up_first")
        .property("inter-context", "inter::one_to_one_up_first")
        .property("context", "inter::one_to_one_up_first")
        .property("context-wait", 20u32)
        .build()
        .unwrap();
    let appsink = gst_app::AppSink::builder()
        .name("appsink::one_to_one_up_first")
        .build();

    let upstream_elems = [&audiotestsrc, &opusenc, &intersink];
    pipe_up.add_many(upstream_elems).unwrap();
    gst::Element::link_many(upstream_elems).unwrap();

    let downstream_elems = [&intersrc, appsink.upcast_ref()];
    pipe_down.add_many(downstream_elems).unwrap();
    gst::Element::link_many(downstream_elems).unwrap();

    pipe_up.set_base_time(gst::ClockTime::ZERO);
    pipe_up.set_start_time(gst::ClockTime::NONE);
    pipe_down.set_base_time(gst::ClockTime::ZERO);
    pipe_down.set_start_time(gst::ClockTime::NONE);

    let (eos_tx, mut eos_rx) = oneshot::channel::<()>();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample({
                let mut samples = 0;
                let mut eos_tx = Some(eos_tx);
                move |appsink| {
                    let _ = appsink.pull_sample().unwrap();
                    samples += 1;
                    if samples == 100 {
                        eos_tx.take().unwrap().send(()).unwrap();
                        return Err(gst::FlowError::Eos);
                    }

                    Ok(gst::FlowSuccess::Ok)
                }
            })
            .build(),
    );

    // Starting upstream first
    pipe_up.set_state(gst::State::Playing).unwrap();
    pipe_down.set_state(gst::State::Playing).unwrap();

    futures::executor::block_on(async {
        use gst::MessageView::*;

        let mut bus_up_stream = pipe_up.bus().unwrap().stream();
        let mut bus_down_stream = pipe_down.bus().unwrap().stream();

        loop {
            futures::select! {
                _ = eos_rx => {
                    println!("inter::one_to_one_up_first got eos notif");
                    break;
                }
                msg = bus_up_stream.next() => {
                    let Some(msg) = msg else { continue };
                    match msg.view() {
                        Latency(_) => {
                            let _ = pipe_up.recalculate_latency();
                        }
                        Error(err) => unreachable!("inter::one_to_one_up_first {err:?}"),
                        _ => (),
                    }
                }
                msg = bus_down_stream.next() => {
                    let Some(msg) = msg else { continue };
                    match msg.view() {
                        Latency(_) => {
                            let _ = pipe_down.recalculate_latency();
                        }
                        Error(err) => unreachable!("inter::one_to_one_up_first {err:?}"),
                        _ => (),
                    }
                }
            };
        }
    });

    let mut q = gst::query::Latency::new();
    assert!(pipe_up.query(&mut q));
    // Upstream latency: testsrc (20ms) + opusenc (20ms)
    let up_latency = q.result().1;
    assert!(
        up_latency >= 40.mseconds(),
        "unexpected upstream latency {up_latency}"
    );

    let mut q = gst::query::Latency::new();
    assert!(pipe_down.query(&mut q));
    // Downstream latency: upstream latency + intersrc (context-wait 20ms) + appsink
    let down_latency = q.result().1;
    assert!(
        down_latency > up_latency + 20.mseconds(),
        "unexpected downstream latency {down_latency}"
    );

    pipe_down.set_state(gst::State::Null).unwrap();
    pipe_up.set_state(gst::State::Null).unwrap();
}

#[test]
fn one_to_many_up_first() {
    use gstthreadshare::runtime::executor as ts_executor;

    init();

    fn build_pipe_down(
        i: u32,
        num_buf: impl Into<Option<u32>>,
    ) -> (
        gst::Pipeline,
        Arc<AtomicU32>,
        Option<futures::channel::oneshot::Receiver<()>>,
    ) {
        let num_buf = num_buf.into();

        let pipe_down =
            gst::Pipeline::with_name(&format!("downstream-{i:02}::one_to_many_up_first"));
        let intersrc = gst::ElementFactory::make("ts-intersrc")
            .name(format!("intersrc-{i:02}::one_to_many_up_first"))
            .property("inter-context", "inter::one_to_many_up_first")
            .property("context", "inter::one_to_many_up_first")
            .property("context-wait", 20u32)
            .build()
            .unwrap();
        let appsink = gst_app::AppSink::builder()
            .name(format!("appsink-{i:02}::one_to_many_up_first"))
            .sync(false)
            .build();

        pipe_down
            .add_many([&intersrc, appsink.upcast_ref()])
            .unwrap();
        intersrc.link(&appsink).unwrap();

        pipe_down.set_base_time(gst::ClockTime::ZERO);
        pipe_down.set_start_time(gst::ClockTime::NONE);

        let samples = Arc::new(AtomicU32::new(0));
        let (mut eos_tx, eos_rx) = if num_buf.is_some() {
            let (eos_tx, eos_rx) = oneshot::channel::<()>();
            (Some(eos_tx), Some(eos_rx))
        } else {
            (None, None)
        };
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample({
                    let samples = samples.clone();
                    move |appsink| {
                        let _ = appsink.pull_sample().unwrap();
                        let cur = samples.fetch_add(1, Ordering::SeqCst);
                        if let Some(num_buf) = num_buf {
                            if cur + 1 == num_buf {
                                eos_tx.take().unwrap().send(()).unwrap();
                                return Err(gst::FlowError::Eos);
                            }
                        }
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .build(),
        );

        (pipe_down, samples, eos_rx)
    }

    let pipe_up = gst::Pipeline::with_name("upstream::one_to_many_up_first");
    let audiotestsrc = gst::ElementFactory::make("audiotestsrc")
        .name("testsrc::one_to_many_up_first")
        .property("is-live", true)
        .build()
        .unwrap();
    let intersink = gst::ElementFactory::make("ts-intersink")
        .name("intersink::one_to_many_up_first")
        .property("inter-context", "inter::one_to_many_up_first")
        .build()
        .unwrap();

    let upstream_elems = [&audiotestsrc, &intersink];
    pipe_up.add_many(upstream_elems).unwrap();
    gst::Element::link_many(upstream_elems).unwrap();

    pipe_up.set_base_time(gst::ClockTime::ZERO);
    pipe_up.set_start_time(gst::ClockTime::NONE);

    // Starting upstream first
    pipe_up.set_state(gst::State::Playing).unwrap();

    let (pipe_down_1, samples_1, eos_rx_1) = build_pipe_down(1, 20);
    let mut eos_rx_1 = eos_rx_1.unwrap();
    pipe_down_1.set_state(gst::State::Playing).unwrap();

    let (pipe_down_2, samples_2, eos_rx_2) = build_pipe_down(2, 20);
    let eos_rx_2 = eos_rx_2.unwrap();
    pipe_down_2.set_state(gst::State::Playing).unwrap();

    futures::executor::block_on(async {
        use gst::MessageView::*;

        let mut bus_down_stream_1 = pipe_down_1.bus().unwrap().stream();
        let mut bus_down_stream_2 = pipe_down_2.bus().unwrap().stream();
        let mut bus_up_stream = pipe_up.bus().unwrap().stream();

        loop {
            futures::select! {
                _ = eos_rx_1 => {
                    println!("inter::one_to_many_up_first got eos notif");
                    break;
                }
                msg_down_1 = bus_down_stream_1.next() => {
                    let Some(msg) = msg_down_1 else { continue };
                    match msg.view() {
                        Latency(_) => {
                            let _ = pipe_down_1.recalculate_latency();
                        }
                        Error(err) => unreachable!("inter::one_to_many_up_first {err:?}"),
                        _ => (),
                    }
                }
                msg_down_2 = bus_down_stream_2.next() => {
                    let Some(msg) = msg_down_2 else { continue };
                    match msg.view() {
                        Latency(_) => {
                            let _ = pipe_down_2.recalculate_latency();
                        }
                        Error(err) => unreachable!("inter::one_to_many_up_first {err:?}"),
                        _ => (),
                    }
                }
                msg_up = bus_up_stream.next() => {
                    let Some(msg) = msg_up else { continue };
                    match msg.view() {
                        Latency(_) => {
                            let _ = pipe_up.recalculate_latency();
                        }
                        Error(err) => unreachable!("inter::one_to_many_up_first {err:?}"),
                        _ => (),
                    }
                }
            }
        }
    });

    pipe_down_1.set_state(gst::State::Null).unwrap();
    // pipe_down_1 was set to stop after 20 buffers
    assert_eq!(samples_1.load(Ordering::SeqCst), 20);

    // Waiting for pipe_down_2 to handle its buffers too
    futures::executor::block_on(eos_rx_2).unwrap();
    pipe_down_2.set_state(gst::State::Null).unwrap();
    assert_eq!(samples_2.load(Ordering::SeqCst), 20);

    pipe_up.set_state(gst::State::Null).unwrap();
    println!("up null");

    let (pipe_down_3, samples_3, _) = build_pipe_down(3, None);
    pipe_down_3.set_state(gst::State::Playing).unwrap();

    let bus_down_3 = pipe_down_3.bus().unwrap();

    ts_executor::block_on(async move {
        let mut bus_down_stream_3 = bus_down_3.stream();

        let mut timer = ts_executor::timer::delay_for(Duration::from_millis(200)).fuse();

        loop {
            futures::select! {
                _ = timer => {
                    break;
                }
                msg_down_3 = bus_down_stream_3.next() => {
                    let Some(msg) = msg_down_3 else { continue };
                    if let gst::MessageView::Error(err) = msg.view() {
                        unreachable!("inter::one_to_many_up_first {err:?}");
                    }
                }
            }
        }
    });

    println!("down3 to null");
    pipe_down_3.set_state(gst::State::Null).unwrap();

    // pipe_down_3 was set to Playing after pipe_up was shutdown
    assert!(samples_3.load(Ordering::SeqCst) == 0);
}
