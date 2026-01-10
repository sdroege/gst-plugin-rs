use anyhow::Error;
use gst::prelude::*;
use std::collections::VecDeque;

struct DynamicStream {
    elements: VecDeque<gst::Element>,
}

fn main() -> Result<(), Error> {
    gst::init()?;

    // Create a very simple webrtc producer, offering a single video stream
    let pipeline = gst::Pipeline::builder().build();

    let videotestsrc = gst::ElementFactory::make("videotestsrc")
        .property_from_str("pattern", "ball")
        .property("is-live", true)
        .build()?;
    let queue = gst::ElementFactory::make("queue").build()?;
    let webrtcsink = gst::ElementFactory::make("webrtcsink")
        .property("run-signalling-server", true)
        .property_from_str("video-caps", "video/x-h264")
        .build()?;

    pipeline.add_many([&videotestsrc, &queue, &webrtcsink])?;
    gst::Element::link_many([&videotestsrc, &queue])?;

    let pad = webrtcsink.request_pad_simple("video_%u").unwrap();

    queue.static_pad("src").unwrap().link(&pad).unwrap();

    let pipeline_weak = pipeline.downgrade();
    webrtcsink.connect("consumer-added", true, move |_| {
        eprintln!("Consumer added, starting renegotiation loop");

        let pipeline_weak = pipeline_weak.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(5000));

                let Some(pipeline) = pipeline_weak.upgrade() else {
                    break;
                };

                let _ = pipeline.post_message(
                    gst::message::Application::builder(
                        gst::Structure::builder("application/timeout").build(),
                    )
                    .build(),
                );
            }
        });

        None
    });

    // Now we simply run the pipeline to completion

    pipeline.set_state(gst::State::Playing)?;

    let bus = pipeline.bus().expect("Pipeline should have a bus");

    let mut dynamic_stream: Option<DynamicStream> = None;

    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => {
                println!("EOS");
                break;
            }
            MessageView::Latency(l) => {
                if l.src() == Some(pipeline.upcast_ref()) {
                    let _ = pipeline.recalculate_latency();
                }
            }
            MessageView::Error(err) => {
                eprintln!(
                    "Got error from {}: {} ({})",
                    msg.src()
                        .map(|s| String::from(s.path_string()))
                        .unwrap_or_else(|| "None".into()),
                    err.error(),
                    err.debug().unwrap_or_else(|| "".into()),
                );
                pipeline.debug_to_dot_file(gst::DebugGraphDetails::all(), "pipeline-error");
                pipeline.set_state(gst::State::Null)?;
                break;
            }
            MessageView::Application(m) => {
                if m.structure()
                    .map(|s| s.has_name("application/timeout"))
                    .unwrap_or(false)
                {
                    eprintln!("Timeout !");

                    match dynamic_stream.take() {
                        Some(stream) => {
                            let tail = stream.elements.back().unwrap();
                            let old_pad = tail.static_pad("src").unwrap().peer().unwrap();
                            for element in &stream.elements {
                                element.set_locked_state(true);
                                let _ = element.set_state(gst::State::Null);
                            }

                            tail.static_pad("src").unwrap().unlink(&old_pad).unwrap();

                            old_pad
                                .parent()
                                .unwrap()
                                .downcast_ref::<gst::Element>()
                                .unwrap()
                                .release_request_pad(&old_pad);

                            for element in &stream.elements {
                                pipeline.remove(element).unwrap();
                            }
                        }
                        _ => {
                            let videotestsrc = gst::ElementFactory::make("videotestsrc")
                                .property_from_str("pattern", "snow")
                                .property("is-live", true)
                                .build()
                                .unwrap();
                            let queue = gst::ElementFactory::make("queue").build().unwrap();

                            pipeline.add_many([&videotestsrc, &queue]).unwrap();

                            videotestsrc.link(&queue).unwrap();

                            let new_pad = webrtcsink.request_pad_simple("video_%u").unwrap();

                            queue.static_pad("src").unwrap().link(&new_pad).unwrap();

                            queue.sync_state_with_parent().unwrap();
                            videotestsrc.sync_state_with_parent().unwrap();

                            dynamic_stream = Some(DynamicStream {
                                elements: VecDeque::from([videotestsrc, queue]),
                            });
                        }
                    }
                }
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null)?;

    Ok(())
}
