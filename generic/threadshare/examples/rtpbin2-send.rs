// SPDX-License-Identifier: MPL-2.0

use clap::Parser;
use futures::prelude::*;
use gst::glib;
use gst::prelude::*;

const RTP_ID: &str = "example-rtp-id";
const BASE_PORT: i32 = 5000;
const AUDIO_PT: u8 = 11;

#[derive(Parser, Debug)]
#[clap(version)]
#[clap(
    about = "Sends audio streams in n separate RTP sessions. Can use `ts-rtpbin2-recv` as a receiver"
)]
pub struct Args {
    /// Number of RTP session to process.
    #[clap(short, long, default_value_t = 1)]
    pub session_nb: usize,

    /// Number of threadshare contexts / groups. Sessions will be distributed in that many groups.
    #[clap(short, long, default_value_t = 1)]
    pub groups: usize,

    /// Address of the receiver.
    #[clap(long, default_value = "127.0.0.1")]
    pub receiver_addr: String,
}

fn main() {
    let args = Args::parse();

    gst::init().unwrap();
    gstrsrtp::plugin_register_static().unwrap();
    gstthreadshare::plugin_register_static().unwrap();

    let main_context = glib::MainContext::default();
    let _guard = main_context.acquire().unwrap();

    let pipeline = gst::Pipeline::new();

    let rtpsend = gst::ElementFactory::make("rtpsend")
        .property("rtp-id", RTP_ID)
        .build()
        .unwrap();
    pipeline.add(&rtpsend).unwrap();

    let rtprecv = gst::ElementFactory::make("rtprecv")
        .property("rtp-id", RTP_ID)
        .build()
        .unwrap();
    pipeline.add(&rtprecv).unwrap();

    let audio_src = gst::ElementFactory::make("ts-audiotestsrc")
        .name("audio-src")
        .property("is-live", true)
        .property("volume", 0.03f64)
        .property("context", "ts-ctx-main")
        .property("context-wait", 20u32)
        .build()
        .unwrap();
    let audio_pay = gst::ElementFactory::make("rtpL16pay")
        .name("pay")
        .property("pt", AUDIO_PT as u32)
        .build()
        .unwrap();
    let tee = gst::ElementFactory::make("tee")
        .name("tee")
        .build()
        .unwrap();
    let elems = [&audio_src, &audio_pay, &tee];
    pipeline.add_many(elems).unwrap();
    gst::Element::link_many(elems).unwrap();

    for session_id in 0..args.session_nb {
        let ctx = format!("ts-ctx-{}", session_id % args.groups);
        let base_port = BASE_PORT + session_id as i32 * 4;

        tee.link_pads(
            Some("src_%u"),
            &rtpsend,
            Some(&format!("rtp_sink_{session_id}")),
        )
        .unwrap();

        // RTP / RTCP to peer
        let rtp_queue = gst::ElementFactory::make("ts-queue")
            .name(format!("queue-rtp-{session_id}"))
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 100.mseconds())
            .property("max-size-buffers", 0u32)
            .property("context", &ctx)
            .property("context-wait", 20u32)
            .build()
            .unwrap();
        pipeline.add(&rtp_queue).unwrap();

        let rtp_sink = gst::ElementFactory::make("ts-udpsink")
            .name(format!("udpsink-rtp-{session_id}"))
            // sync of the RTP packets is handled before rtpsend
            .property("sync", false)
            .property("clients", format!("{}:{base_port}", args.receiver_addr))
            .property("context", &ctx)
            .property("context-wait", 20u32)
            .build()
            .unwrap();
        pipeline.add(&rtp_sink).unwrap();
        rtpsend
            .link_pads(
                Some(&format!("rtp_src_{session_id}")),
                &rtp_queue,
                Some("sink"),
            )
            .unwrap();
        rtp_queue.link(&rtp_sink).unwrap();

        let rtcp_sink = gst::ElementFactory::make("ts-udpsink")
            .name(format!("udpsink-rtcp-{session_id}"))
            .property("sync", false)
            .property(
                "clients",
                format!("{}:{}", args.receiver_addr, base_port + 1),
            )
            .property("context", &ctx)
            .property("context-wait", 20u32)
            .build()
            .unwrap();
        pipeline.add(&rtcp_sink).unwrap();
        rtpsend
            .link_pads(
                Some(&format!("rtcp_src_{session_id}")),
                &rtcp_sink,
                Some("sink"),
            )
            .unwrap();

        // RTCP from peer
        let rtcp_src = gst::ElementFactory::make("ts-udpsrc")
            .name(format!("udpsrc-rtcp-{session_id}"))
            .property("port", base_port + 3)
            .property("caps", gst::Caps::new_empty_simple("application/x-rtcp"))
            .property("context", &ctx)
            .property("context-wait", 20u32)
            .build()
            .unwrap();
        pipeline.add(&rtcp_src).unwrap();
        rtcp_src
            .link_pads(
                Some("src"),
                &rtprecv,
                Some(&format!("rtcp_sink_{session_id}")),
            )
            .unwrap();
    }

    let l = glib::MainLoop::new(None, false);

    let mut bus_send_stream = pipeline.bus().unwrap().stream();

    main_context.spawn_local({
        let pipeline = pipeline.clone();
        async move {
            while let Some(msg) = bus_send_stream.next().await {
                use gst::MessageView::*;
                match msg.view() {
                    Latency(_) => {
                        let _ = pipeline.recalculate_latency();
                    }
                    Error(err) => {
                        eprintln!("recv pipeline: {err:?}");
                        break;
                    }
                    _ => (),
                }
            }
        }
    });

    pipeline.set_state(gst::State::Playing).unwrap();
    println!("Playing");

    ctrlc::set_handler({
        let l = l.clone();
        move || {
            eprintln!("\nShutting down due to user request");
            l.quit();
        }
    })
    .unwrap();

    l.run();

    pipeline.debug_to_dot_file(gst::DebugGraphDetails::empty(), "rtpbin2-send-stopping");
    pipeline.set_state(gst::State::Null).unwrap();

    // This is needed by some tracers to write their log file
    unsafe {
        gst::deinit();
    }
}
