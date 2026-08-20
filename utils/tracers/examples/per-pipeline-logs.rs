/// This example demonstrates filtering of logs per pipeline, and forwarding to a
/// companion log server application.
///
/// Start the log server first:
///
/// ```
/// cargo run --example per-pipeline-logs-server
/// ```
///
/// Then start this application with the per-pipeline-logs tracer enabled:
///
/// ```
/// GST_TRACERS='per-pipeline-logs(pipelines-thresholds="5@my-pipeline",silent=true)' time cargo run --example per-pipeline-logs
/// ```
///
/// When opening the path printed by the logs server, you should find the
/// logs for my-pipeline.
///
/// Try experimenting with the pipeline thresholds, for instance `0,*src*:5@my-pipeline` to
/// set a named threshold, or `9@my-pipeline-2`, with this set you should still observe a
/// few logs getting written to the log file, as we make the choice in this example to forward
/// logs that couldn't be associated with a top-level pipeline.
///
/// The logs are forwarded to the server over TCP, the protocol is a simplified version
/// of the journald protocol.
use anyhow::{Error, anyhow};
use futures::prelude::*;
use gst::prelude::*;
use std::io::Write;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

fn main() -> Result<(), Error> {
    gst::init()?;

    let tracers = gst::active_tracers();

    let my_pipeline = gst::Pipeline::builder().name("my-pipeline").build();

    let mut found_tracer = false;

    for tracer in &tracers {
        if tracer.type_().name() == "GstPerPipelineLogs" {
            found_tracer = true;
            let stream = Arc::new(Mutex::new(Some(TcpStream::connect("127.0.0.1:5555")?)));
            let my_pipeline_clone = my_pipeline.downgrade();
            tracer.connect("new-log", false, move |args| {
                let level = args[2].get::<gst::DebugLevel>().unwrap();
                let file = args[3].get::<String>().unwrap();
                let function = args[4].get::<String>().unwrap();
                let line = args[5].get::<u32>().unwrap();
                let object_name = args[6].get::<Option<String>>().unwrap();
                let category_name = args[7].get::<String>().unwrap();
                let message = args[8].get::<String>().unwrap();

                let mut buf = Vec::with_capacity(1024);

                if !file.is_empty() {
                    writeln!(&mut buf, "CODE_FILE={}", file).unwrap();
                }

                writeln!(&mut buf, "CODE_LINE={}", line).unwrap();
                writeln!(&mut buf, "_LEVEL={}", level.name().trim_end()).unwrap();
                if !function.is_empty() {
                    writeln!(&mut buf, "CODE_FUNC={}", function).unwrap();
                }
                if let Some(object_name) = object_name {
                    writeln!(&mut buf, "_OBJECT_NAME={}", object_name).unwrap();
                }
                writeln!(&mut buf, "_CATEGORY_NAME={}", category_name).unwrap();
                writeln!(&mut buf, "MESSAGE=").unwrap();
                buf.extend_from_slice(&[0; 8]);
                let start = buf.len();
                write!(&mut buf, "{}", message).unwrap();
                let end = buf.len();
                buf[start - 8..start].copy_from_slice(&((end - start) as u64).to_le_bytes());
                buf.push(b'\n');

                let mut locked_stream = stream.lock().unwrap();
                if let Some(ref mut stream_handle) = *locked_stream
                    && let Err(err) = stream_handle.write_all(&buf)
                {
                    drop(locked_stream);

                    // Don't try to send again
                    let _ = stream.lock().unwrap().take();

                    if let Some(pipeline) = my_pipeline_clone.upgrade() {
                        pipeline.post_error_message(gst::error_msg!(
                            gst::CoreError::Failed,
                            ["Error writing logs: {err:?}"]
                        ));
                    }
                    return None;
                }

                None
            });
        }
    }

    if !found_tracer {
        return Err(anyhow!("Per pipeline logs tracer was not enabled!"));
    }

    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 100_000)
        .build()?;
    let sink = gst::ElementFactory::make("fakesink").build()?;

    my_pipeline.add_many([&src, &sink])?;

    src.link(&sink)?;

    my_pipeline.set_state(gst::State::Playing)?;

    let ctx = gst::glib::MainContext::default();

    ctx.block_on(async {
        let bus = my_pipeline.bus().unwrap();
        let pipeline_weak = my_pipeline.downgrade();
        let mut bus_stream = bus.stream();

        while let Some(bus_msg) = bus_stream.next().await {
            use gst::MessageView::*;

            let Some(pipeline) = pipeline_weak.upgrade() else {
                break;
            };

            match bus_msg.view() {
                Error(msg) => {
                    let err = msg.error();
                    eprintln!("Pipeline errored out: {err:?}");

                    pipeline.debug_to_dot_file_with_ts(
                        gst::DebugGraphDetails::ALL,
                        format!("{}-error", pipeline.name().as_str()),
                    );

                    break;
                }
                StateChanged(sc) => {
                    if sc.src() == Some(pipeline.upcast_ref()) {
                        pipeline.debug_to_dot_file_with_ts(
                            gst::DebugGraphDetails::ALL,
                            format!("{}-{}-{}", pipeline.name().as_str(), sc.old(), sc.current()),
                        );
                    }
                }
                Latency(_) => {
                    pipeline
                        .call_async_future(|pipeline| {
                            let _ = pipeline.recalculate_latency();
                        })
                        .await;
                }
                Eos(_) => {
                    eprintln!("pipeline {} is EOS", pipeline.name().as_str());
                    break;
                }
                _ => (),
            }
        }
    });

    let _ = my_pipeline.set_state(gst::State::Null);

    Ok(())
}
