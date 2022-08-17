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
    sink::register(plugin)?;

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

use clap::Parser;

#[derive(Parser, Debug)]
#[clap(version)]
#[clap(about = "Standalone pipeline threadshare runtime test")]
struct Args {
    /// Parallel streams to process.
    #[clap(short, long, default_value_t = 100)]
    streams: u32,

    /// Threadshare groups.
    #[clap(short, long, default_value_t = 2)]
    groups: u32,

    /// Threadshare Context wait in ms (max throttling duration).
    #[clap(short, long, default_value_t = 20)]
    wait: u32,

    /// Number of buffers per stream to output before sending EOS (-1 = unlimited).
    #[clap(short, long, default_value_t = 6000)]
    num_buffers: i32,

    /// Enables statistics logging (use GST_DEBUG=ts-standalone*:4).
    #[clap(short, long)]
    log_stats: bool,
}

fn main() {
    use gst::prelude::*;
    use std::time::Instant;

    gst::init().unwrap();
    self::plugin_register_static().unwrap();

    #[cfg(debug_assertions)]
    gst::warning!(CAT, "RUNNING DEBUG BUILD");

    let args = Args::parse();

    let pipeline = gst::Pipeline::new(None);

    for i in 0..args.streams {
        let ctx_name = format!("standalone {}", i % args.groups);

        let src = gst::ElementFactory::make(
            "ts-standalone-test-src",
            Some(format!("src-{}", i).as_str()),
        )
        .unwrap();
        src.set_property("context", &ctx_name);
        src.set_property("context-wait", args.wait);
        src.set_property("num-buffers", args.num_buffers);

        let sink = gst::ElementFactory::make(
            "ts-standalone-test-sink",
            Some(format!("sink-{}", i).as_str()),
        )
        .unwrap();
        sink.set_property("context", &ctx_name);
        sink.set_property("context-wait", args.wait);
        if i == 0 && args.log_stats {
            sink.set_property("must-log-stats", true);
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
            MessageView::Eos(..) => {
                gst::info!(CAT, "Shuting down");
                let stop = Instant::now();
                pipeline_clone.set_state(gst::State::Null).unwrap();
                gst::info!(CAT, "Shuting down took {:.2?}", stop.elapsed());
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

    gst::info!(CAT, "Starting");
    let start = Instant::now();
    pipeline.set_state(gst::State::Playing).unwrap();
    gst::info!(CAT, "Starting took {:.2?}", start.elapsed());

    l.run();
}
