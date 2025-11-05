use anyhow::Error;
use gst::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct DynamicStream {
    queue: gst::Element,
    convert: gst::Element,
}

fn main() -> Result<(), Error> {
    gst::init()?;

    let pipeline = gst::Pipeline::builder().build();

    pipeline.set_property("message-forward", true);

    let webrtcsrc = gst::ElementFactory::make("webrtcsrc")
        .property("connect-to-first-producer", true)
        .build()?;

    pipeline.add(&webrtcsrc)?;

    let queue = gst::ElementFactory::make("queue").build().unwrap();
    let videoconvert = gst::ElementFactory::make("videoconvert").build().unwrap();
    let sink = gst::ElementFactory::make("glimagesink").build().unwrap();

    pipeline.add_many([&queue, &videoconvert, &sink]).unwrap();
    gst::Element::link_many([&queue, &videoconvert, &sink]).unwrap();

    let dynamic_streams: Arc<Mutex<HashMap<gst::Element, DynamicStream>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let pipeline_weak = pipeline.downgrade();
    let dynamic_streams_clone = dynamic_streams.clone();
    webrtcsrc.connect_pad_added(move |_src, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };

        if !queue.static_pad("sink").unwrap().is_linked() {
            pad.link(&queue.static_pad("sink").unwrap()).unwrap();
        } else {
            let queue = gst::ElementFactory::make("queue").build().unwrap();
            let convert = gst::ElementFactory::make("videoconvert").build().unwrap();
            let sink = gst::ElementFactory::make("glimagesink").build().unwrap();

            pipeline.add_many([&queue, &convert, &sink]).unwrap();
            gst::Element::link_many([&queue, &convert, &sink]).unwrap();
            let _ = queue.sync_state_with_parent();
            let _ = convert.sync_state_with_parent();
            let _ = sink.sync_state_with_parent();
            pad.link(&queue.static_pad("sink").unwrap()).unwrap();

            dynamic_streams_clone
                .lock()
                .unwrap()
                .insert(sink, DynamicStream { queue, convert });
        }

        pipeline.debug_to_dot_file_with_ts(gst::DebugGraphDetails::all(), "pad-added");
    });

    // Now we simply run the pipeline to completion

    pipeline.set_state(gst::State::Playing)?;

    let bus = pipeline.bus().expect("Pipeline should have a bus");

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
            MessageView::Element(e) => {
                let s = e.structure().unwrap();

                if s.has_name("GstBinForwarded") {
                    let m = s.get::<gst::Message>("message").unwrap();

                    if matches!(m.type_(), gst::MessageType::Eos) {
                        if let Some(src) = m.src() {
                            let src = src.downcast_ref::<gst::Element>().unwrap();
                            let mut dynamic_streams = dynamic_streams.lock().unwrap();
                            if let Some(stream) = dynamic_streams.remove(src) {
                                let sink_pad = stream.queue.static_pad("sink").unwrap();
                                let peer = sink_pad.peer().unwrap();

                                peer.unlink(&sink_pad).unwrap();

                                pipeline.remove(src).unwrap();
                                pipeline.remove(&stream.queue).unwrap();
                                pipeline.remove(&stream.convert).unwrap();

                                src.set_state(gst::State::Null).unwrap();
                                stream.queue.set_state(gst::State::Null).unwrap();
                                stream.convert.set_state(gst::State::Null).unwrap();
                            }
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
