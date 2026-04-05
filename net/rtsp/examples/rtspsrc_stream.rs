use clap::Parser;
use gst::glib;
use gst::prelude::*;

#[derive(Parser, Debug)]
#[command(name = "gst-rtspsrc2")]
#[command(version = "0.1")]
#[command(about = "Code for testing Streams API with rtspsrc2", long_about = None)]
struct Cli {
    #[clap(long, short, default_value_t = false)]
    audio: bool,
    #[clap(long, short, default_value_t = true)]
    video: bool,
    #[clap(long, short, value_name = "URL")]
    url: String,
}

fn main() {
    let cli = Cli::parse();

    gst::init().unwrap();

    let main_loop = glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new();
    pipeline.set_bin_flags(gst::BinFlags::STREAMS_AWARE);

    let rtspsrc = gst::ElementFactory::make("rtspsrc2")
        .property("location", cli.url)
        .build()
        .unwrap();
    let decodebin = gst::ElementFactory::make("decodebin3")
        .name("decodebin3")
        .build()
        .unwrap();

    pipeline.add(&rtspsrc).unwrap();
    pipeline.add(&decodebin).unwrap();

    decodebin.connect_pad_added(glib::clone!(
        #[weak]
        pipeline,
        move |_, pad| {
            if pad.name().starts_with("sink_") {
                return;
            }

            let stream = pad.stream().unwrap();

            let queue = gst::ElementFactory::make("queue").build().unwrap();
            queue.set_property("max-size-buffers", 0u32);
            queue.set_property("max-size-bytes", 0u32);
            queue.set_property("max-size-time", 5 * gst::ClockTime::SECOND);

            let (scale, convert, sink) = if stream.stream_type() == gst::StreamType::VIDEO {
                let videoscale = gst::ElementFactory::make("videoscale").build().unwrap();
                let videoconvert = gst::ElementFactory::make("videoconvert").build().unwrap();
                let autovideosink = gst::ElementFactory::make("autovideosink").build().unwrap();

                (videoscale, videoconvert, autovideosink)
            } else if stream.stream_type() == gst::StreamType::AUDIO {
                let audioresample = gst::ElementFactory::make("audioresample").build().unwrap();
                let audioconvert = gst::ElementFactory::make("audioconvert").build().unwrap();
                let autoaudiosink = gst::ElementFactory::make("autoaudiosink").build().unwrap();

                (audioresample, audioconvert, autoaudiosink)
            } else {
                eprintln!("Ignoring stream {stream:?}");
                return;
            };

            pipeline
                .add_many([&queue, &scale, &convert, &sink])
                .unwrap();
            gst::Element::link_many([&queue, &scale, &convert, &sink]).unwrap();

            let sinkpad = queue.static_pad("sink").unwrap();
            pad.link(&sinkpad).unwrap();

            sink.sync_state_with_parent().unwrap();
            convert.sync_state_with_parent().unwrap();
            scale.sync_state_with_parent().unwrap();
            queue.sync_state_with_parent().unwrap();

            pipeline
                .debug_to_dot_file_with_ts(gst::DebugGraphDetails::all(), "decodebin-pad-added");
        }
    ));

    rtspsrc.connect_pad_added(glib::clone!(
        #[weak]
        pipeline,
        move |_src, pad| {
            pipeline.debug_to_dot_file_with_ts(gst::DebugGraphDetails::all(), "rtspsrc2-pad-added");

            let queue = gst::ElementFactory::make("queue").build().unwrap();
            queue.set_property("max-size-buffers", 0u32);
            queue.set_property("max-size-bytes", 0u32);
            queue.set_property("max-size-time", gst::ClockTime::from_mseconds(500));

            pipeline.add(&queue).unwrap();

            let sinkpad = queue.static_pad("sink").unwrap();
            pad.link(&sinkpad).unwrap();

            let srcpad = queue.static_pad("src").unwrap();

            let decodebin = pipeline.by_name("decodebin3").unwrap();
            let decodebin_sinkpad = decodebin.request_pad_simple("sink_%u").unwrap();

            srcpad.link(&decodebin_sinkpad).unwrap();

            queue.sync_state_with_parent().unwrap();
        }
    ));

    let l_clone = main_loop.clone();
    ctrlc::set_handler(move || {
        l_clone.quit();
    })
    .unwrap();

    let bus = pipeline.bus().unwrap();
    let l_clone = main_loop.clone();
    let _bus_watch = bus
        .add_watch(move |_, msg| {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(..) => {
                    eprintln!("\nReceived End of Stream, quitting...");
                    l_clone.quit();
                }
                MessageView::StreamsSelected(selected) => {
                    eprintln!("Streams selected: {:?}", selected.streams());
                }
                MessageView::Error(err) => {
                    eprintln!(
                        "Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                    l_clone.quit();
                }
                _ => (),
            };
            glib::ControlFlow::Continue
        })
        .expect("Failed to add bus watch");

    pipeline.set_state(gst::State::Playing).unwrap();

    bus.set_sync_handler(move |_, msg| {
        use gst::MessageView;

        if let MessageView::StreamCollection(collection) = msg.view() {
            let mut selected_streams = Vec::new();
            let collection = collection.stream_collection();
            eprintln!("Received StreamCollection: {:?}", collection.debug());

            for stream in collection.iter() {
                match stream.stream_type() {
                    gst::StreamType::VIDEO if cli.video => {
                        if let Some(id) = stream.stream_id() {
                            eprintln!("Selecting video with stream-id: {id}");
                            selected_streams.push(id);
                        }
                    }
                    gst::StreamType::AUDIO if cli.audio => {
                        if let Some(id) = stream.stream_id() {
                            eprintln!("Selecting audio with stream-id: {id}");
                            selected_streams.push(id);
                        }
                    }
                    _ => (),
                }
            }

            let event =
                gst::event::SelectStreams::builder(selected_streams.iter().map(|s| s.as_str()))
                    .build();

            eprintln!("Posting SelectStreams {event:?} to rtspsrc2");

            if !rtspsrc.send_event(event) {
                eprintln!("Failed to send SelectStreams event");
            }
        }

        gst::BusSyncReply::Pass
    });

    main_loop.run();

    pipeline.set_state(gst::State::Null).unwrap();
}
