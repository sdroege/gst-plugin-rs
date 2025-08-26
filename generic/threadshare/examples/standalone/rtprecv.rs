use gst::glib;
use std::sync::LazyLock;

mod args;
use args::*;

#[macro_use]
mod macros;

mod sink;
mod src;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

const DROP_PROBABILITY: f32 = 0.125f32;
const RTPRECV_LATENCY_MS: u32 = 40;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ts-standalone-rtprecv",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing standalone rtprecv test"),
    )
});

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    sink::async_mutex::register(plugin)?;
    sink::sync_mutex::register(plugin)?;
    sink::task::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    threadshare_standalone_test,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "LGPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);

fn main() {
    use gst::prelude::*;
    use std::time::Instant;

    gst::init().unwrap();
    gstthreadshare::plugin_register_static().unwrap();
    gstrsrtp::plugin_register_static().unwrap();
    self::plugin_register_static().unwrap();

    let main_context = glib::MainContext::default();
    let _guard = main_context.acquire().unwrap();

    #[cfg(debug_assertions)]
    gst::warning!(CAT, "RUNNING DEBUG BUILD");

    let args = args();

    let pipeline = gst::Pipeline::default();

    for i in 0..args.streams {
        let ctx_name = format!("standalone {}", i % args.groups);

        let src = gst::ElementFactory::make("ts-audiotestsrc")
            .name(format!("src-{i}").as_str())
            .property("is-live", true)
            .property("context", &ctx_name)
            .property("context-wait", args.wait)
            .property("num-buffers", args.num_buffers)
            .build()
            .unwrap();

        let queue = gst::ElementFactory::make("ts-queue")
            .name(format!("queue-src-{i}").as_str())
            .property("context", &ctx_name)
            .property("context-wait", args.wait)
            .property("max-size-buffers", 10u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .build()
            .unwrap();

        let convert = gst::ElementFactory::make("audioconvert")
            .name(format!("convert-{i}").as_str())
            .build()
            .unwrap();

        let pay = gst::ElementFactory::make("rtpL16pay")
            .name(format!("pay-{i}").as_str())
            .build()
            .unwrap();

        let dropper = gst::ElementFactory::make("identity")
            .name(format!("dropper-{i}").as_str())
            .property("drop-probability", DROP_PROBABILITY)
            .build()
            .unwrap();

        let rtprecv = gst::ElementFactory::make("rtprecv")
            .name(format!("rtprecv-{i}").as_str())
            .property("latency", RTPRECV_LATENCY_MS)
            .build()
            .unwrap();

        rtprecv.connect_pad_added(move |elem, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }

            let depay = gst::ElementFactory::make("rtpL16depay")
                .name(format!("depay-{i}").as_str())
                .build()
                .unwrap();

            let queue = gst::ElementFactory::make("ts-queue")
                .name(format!("queue-sink-{i}").as_str())
                .property("context", &ctx_name)
                .property("context-wait", args.wait)
                .property("max-size-buffers", 0u32)
                .property("max-size-bytes", 0u32)
                .property(
                    "max-size-time",
                    (20u64 + RTPRECV_LATENCY_MS as u64).mseconds(),
                )
                .build()
                .unwrap();

            let sink = gst::ElementFactory::make(args.sink.element_name())
                .name(format!("sink-{i}").as_str())
                .property("context", &ctx_name)
                .property("context-wait", args.wait)
                .build()
                .unwrap();

            if i == 0 {
                sink.set_property("main-elem", true);

                if !args.disable_stats_log {
                    // Don't use the last 5 secs in stats
                    // otherwise we get outliers when reaching EOS.
                    // Note that stats don't start before the 20 first seconds
                    // and we get 50 buffers per sec.
                    const BUFFERS_BEFORE_LOGS: i32 = 20 * 50;
                    const BUFFERS_TO_SKIP: i32 = BUFFERS_BEFORE_LOGS + 5 * 50;
                    let expected_buffers =
                        (args.num_buffers as f32 * (1.0f32 - DROP_PROBABILITY)) as i32;
                    if expected_buffers > BUFFERS_TO_SKIP {
                        sink.set_property("push-period", args.push_period);
                        sink.set_property("logs-stats", true);
                        let max_buffers = expected_buffers - BUFFERS_TO_SKIP;
                        sink.set_property("max-buffers", max_buffers);
                    } else {
                        gst::warning!(CAT, "Not enough buffers to log, disabling stats");
                    }
                }
            }

            let elements = &[&depay, &queue, &sink];
            elem.parent()
                .unwrap()
                .downcast_ref::<gst::Bin>()
                .unwrap()
                .add_many(elements)
                .unwrap();

            pad.link(&depay.static_pad("sink").unwrap()).unwrap();
            gst::Element::link_many(elements).unwrap();

            sink.sync_state_with_parent().unwrap();
            queue.sync_state_with_parent().unwrap();
            depay.sync_state_with_parent().unwrap();
        });

        let elements = &[&src, &queue, &convert, &pay, &dropper, &rtprecv];
        pipeline.add_many(elements).unwrap();
        gst::Element::link_many(elements).unwrap();
    }

    let l = glib::MainLoop::new(None, false);

    let bus = pipeline.bus().unwrap();
    let mut bus_stream = bus.stream();
    let pipeline_weak = pipeline.downgrade();
    let l_clone = l.clone();
    main_context.spawn_local(async move {
        use futures::prelude::*;

        let terminated_count = Arc::new(AtomicU32::new(0));

        while let Some(msg) = bus_stream.next().await {
            use gst::MessageView::*;

            let Some(pipeline) = pipeline_weak.upgrade() else {
                break;
            };

            match msg.view() {
                Eos(_) => {
                    // Actually, we don't post EOS (see sinks impl).
                    gst::info!(CAT, "Received eos");
                    l_clone.quit();

                    break;
                }
                Error(msg) => {
                    if let gst::MessageView::Error(msg) = msg.message().view() {
                        if msg.error().matches(gst::LibraryError::Shutdown) {
                            if terminated_count.fetch_add(1, Ordering::SeqCst) == args.streams - 1 {
                                gst::info!(CAT, "Received all shutdown requests");
                                l_clone.quit();

                                break;
                            } else {
                                continue;
                            }
                        }
                    }

                    gst::error!(
                        CAT,
                        "Error from {:?}: {} ({:?})",
                        msg.src().map(|s| s.name()),
                        msg.error(),
                        msg.debug()
                    );
                    l_clone.quit();

                    break;
                }
                Latency(msg) => {
                    gst::info!(
                        CAT,
                        "Latency requirements have changed for element {}",
                        msg.src()
                            .map(|src| src.name())
                            .as_deref()
                            .unwrap_or("UNKNOWN"),
                    );
                    if let Err(err) = pipeline.recalculate_latency() {
                        gst::error!(CAT, "Error recalculating latency: {err}");
                    }
                }
                _ => (),
            }
        }
    });

    gst::info!(CAT, "Switching to Ready");
    let start = Instant::now();
    pipeline.set_state(gst::State::Ready).unwrap();
    gst::info!(CAT, "Switching to Ready took {:.2?}", start.elapsed());

    gst::info!(CAT, "Switching to Playing");
    let start = Instant::now();
    pipeline.set_state(gst::State::Playing).unwrap();
    gst::info!(CAT, "Switching to Playing took {:.2?}", start.elapsed());

    l.run();

    gst::info!(CAT, "Switching to Ready");
    let stop = Instant::now();
    pipeline.set_state(gst::State::Ready).unwrap();
    gst::info!(CAT, "Switching to Ready took {:.2?}", stop.elapsed());

    gst::info!(CAT, "Shutting down");
    let stop = Instant::now();
    pipeline.set_state(gst::State::Null).unwrap();
    gst::info!(CAT, "Shutting down took {:.2?}", stop.elapsed());
}
