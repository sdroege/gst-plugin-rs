use gst::glib;
use once_cell::sync::Lazy;

mod sink;
mod src;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-standalone-test-main",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing standalone test main"),
    )
});

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    src::register(plugin)?;
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

#[cfg(feature = "clap")]
use clap::Parser;

#[cfg(feature = "clap")]
#[derive(Parser, Debug)]
#[clap(version)]
#[clap(
    about = "Standalone pipeline threadshare runtime test. Use `GST_DEBUG=ts-standalone*:4` for stats"
)]
struct Args {
    /// Parallel streams to process.
    #[clap(short, long, default_value_t = 5000)]
    streams: u32,

    /// Threadshare groups.
    #[clap(short, long, default_value_t = 2)]
    groups: u32,

    /// Threadshare Context wait in ms (max throttling duration).
    #[clap(short, long, default_value_t = 20)]
    wait: u32,

    /// Buffer push period in ms.
    #[clap(short, long, default_value_t = 20)]
    push_period: u32,

    /// Number of buffers per stream to output before sending EOS (-1 = unlimited).
    #[clap(short, long, default_value_t = 5000)]
    num_buffers: i32,

    /// Disables statistics logging.
    #[clap(short, long)]
    disable_stats_log: bool,
}

#[cfg(not(feature = "clap"))]
#[derive(Debug)]
struct Args {
    streams: u32,
    groups: u32,
    wait: u32,
    push_period: u32,
    num_buffers: i32,
    disable_stats_log: bool,
}

#[cfg(not(feature = "clap"))]
impl Default for Args {
    fn default() -> Self {
        Args {
            streams: 5000,
            groups: 2,
            wait: 20,
            push_period: 20,
            num_buffers: 5000,
            disable_stats_log: false,
        }
    }
}

fn args() -> Args {
    #[cfg(feature = "clap")]
    let args = {
        let args = Args::parse();
        gst::info!(CAT, "{:?}", args);

        args
    };

    #[cfg(not(feature = "clap"))]
    let args = {
        if std::env::args().len() > 1 {
            gst::warning!(CAT, "Ignoring command line arguments");
            gst::warning!(CAT, "Build with `--features=clap`");
        }

        let args = Args::default();
        gst::warning!(CAT, "{:?}", args);

        args
    };

    args
}

fn main() {
    use gst::prelude::*;
    use std::time::Instant;

    gst::init().unwrap();
    self::plugin_register_static().unwrap();

    #[cfg(debug_assertions)]
    gst::warning!(CAT, "RUNNING DEBUG BUILD");

    let args = args();

    let pipeline = gst::Pipeline::default();

    for i in 0..args.streams {
        let ctx_name = format!("standalone {}", i % args.groups);

        let src = gst::ElementFactory::make("ts-standalone-test-src")
            .name(format!("src-{}", i).as_str())
            .property("context", &ctx_name)
            .property("context-wait", args.wait)
            .property("push-period", args.push_period)
            .property("num-buffers", args.num_buffers)
            .build()
            .unwrap();

        let sink = gst::ElementFactory::make("ts-standalone-test-sink")
            .name(format!("sink-{}", i).as_str())
            .property("context", &ctx_name)
            .property("context-wait", args.wait)
            .build()
            .unwrap();

        if i == 0 {
            src.set_property("raise-log-level", true);
            sink.set_property("raise-log-level", true);

            if !args.disable_stats_log {
                // Don't use the last 5 secs in stats
                // otherwise we get outliers when reaching EOS.
                // Note that stats don't start before the 20 first seconds
                // and we get 50 buffers per sec.
                const BUFFERS_BEFORE_LOGS: i32 = 20 * 50;
                const BUFFERS_TO_SKIP: i32 = BUFFERS_BEFORE_LOGS + 5 * 50;
                if args.num_buffers > BUFFERS_TO_SKIP {
                    sink.set_property("push-period", args.push_period);
                    sink.set_property("logs-stats", true);
                    let max_buffers = args.num_buffers - BUFFERS_TO_SKIP;
                    sink.set_property("max-buffers", max_buffers);
                } else {
                    gst::warning!(CAT, "Not enough buffers to log, disabling stats");
                }
            }
        }

        let elements = &[&src, &sink];
        pipeline.add_many(elements).unwrap();
        gst::Element::link_many(elements).unwrap();
    }

    let l = glib::MainLoop::new(None, false);

    let bus = pipeline.bus().unwrap();
    let pipeline_clone = pipeline.clone();
    let l_clone = l.clone();
    bus.add_watch(move |_, msg| {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(_) => {
                gst::info!(CAT, "Received eos");
                l_clone.quit();
            }
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
    pipeline_clone.set_state(gst::State::Ready).unwrap();
    gst::info!(CAT, "Switching to Ready took {:.2?}", stop.elapsed());

    gst::info!(CAT, "Shutting down");
    let stop = Instant::now();
    pipeline_clone.set_state(gst::State::Null).unwrap();
    gst::info!(CAT, "Shutting down took {:.2?}", stop.elapsed());
}
