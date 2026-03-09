// SPDX-License-Identifier: MPL-2.0

use clap::Parser;
use futures::prelude::*;
use gst::glib;
use gst::prelude::*;
use itertools::Itertools as _;

use std::sync::atomic::{AtomicUsize, Ordering};

const RTP_ID: &str = "example-rtp-id";
const BASE_PORT: i32 = 5000;
const AUDIO_PT: u8 = 11;

const LATENCY: gst::ClockTime = gst::ClockTime::from_mseconds(200);

#[derive(Parser, Debug)]
#[clap(version)]
#[clap(
    about = "Receives audio streams from n separate RTP sessions. Can use `ts-rtpbin2-send` as a sender"
)]
pub struct Args {
    /// Number of RTP session to process.
    #[clap(short, long, default_value_t = 1)]
    pub session_nb: usize,

    /// Number of threadshare contexts / groups. Sessions will be distributed in that many groups.
    #[clap(short, long, default_value_t = 1)]
    pub groups: usize,

    /// Address of the sender.
    #[clap(long, default_value = "127.0.0.1")]
    pub sender_addr: String,
}

fn main() {
    let args = Args::parse();

    unsafe {
        std::env::set_var("GST_RTPBIN2_RT_WORKER_THREADS", "1");
        std::env::set_var("GST_RTPBIN2_RT_MAX_BLOCKING_THREADS", "1024");
        std::env::set_var("GST_RTPBIN2_RT_THREAD_KEEP_ALIVE", "600000"); // 5mn
    }

    gst::init().unwrap();
    gstrsrtp::plugin_register_static().unwrap();
    gstthreadshare::plugin_register_static().unwrap();

    let main_context = glib::MainContext::default();
    let _guard = main_context.acquire().unwrap();

    let pipeline = gst::Pipeline::new();

    let rtprecv = gst::ElementFactory::make("rtprecv")
        .property("rtp-id", RTP_ID)
        .property("latency", LATENCY.mseconds() as u32)
        .build()
        .unwrap();
    pipeline.add(&rtprecv).unwrap();

    let rtpsend = gst::ElementFactory::make("rtpsend")
        .property("rtp-id", RTP_ID)
        .build()
        .unwrap();
    pipeline.add(&rtpsend).unwrap();

    for session_id in 0..args.session_nb {
        let ctx_id = session_id % args.groups;
        let ctx = format!("ts-ctx-{ctx_id}");
        let base_port = BASE_PORT + session_id as i32 * 4;

        let rtp_src = gst::ElementFactory::make("ts-udpsrc")
            .name(format!("udpsrc-rtp-{session_id}"))
            .property("port", base_port)
            .property(
                "caps",
                gst::Caps::builder("application/x-rtp")
                    .field("media", "audio")
                    .field("payload", AUDIO_PT as i32)
                    .field("clock-rate", 44_100i32)
                    .field("encoding-name", "L16")
                    .build(),
            )
            .property("context", &ctx)
            .property("context-wait", 20u32)
            .build()
            .unwrap();
        pipeline.add(&rtp_src).unwrap();
        let queue_rtp = gst::ElementFactory::make("ts-queue")
            .name(format!("queue-rtp-{session_id}"))
            .property("max-size-bytes", 0u32)
            .property("max-size-time", LATENCY + 30.mseconds())
            .property("max-size-buffers", 0u32)
            .property("context", &ctx)
            .property("context-wait", 20u32)
            .build()
            .unwrap();
        pipeline.add(&queue_rtp).unwrap();
        rtp_src.link(&queue_rtp).unwrap();
        queue_rtp
            .link_pads(
                Some("src"),
                &rtprecv,
                Some(&format!("rtp_sink_{session_id}")),
            )
            .unwrap();

        let rtcp_src = gst::ElementFactory::make("ts-udpsrc")
            .name(format!("udpsrc-rtcp-{session_id}"))
            .property("port", base_port + 1)
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

        // RTCP to peer
        let rtcp_sink = gst::ElementFactory::make("ts-udpsink")
            .name(format!("udpsink-rtcp-{session_id}"))
            .property("sync", false)
            .property("clients", format!("{}:{}", args.sender_addr, base_port + 3))
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
    }

    let left_to_add = AtomicUsize::new(args.session_nb);
    rtprecv.connect_pad_added(move |recv, pad| {
        let pad_name = pad.name();
        if !pad_name.starts_with("rtp_src_") {
            return;
        }

        let (_prefix, _direction, session_id, _pt, _ssrc) =
            pad_name.split('_').collect_tuple().unwrap();
        let session_id = session_id.parse::<usize>().unwrap();

        let parent = recv.parent().unwrap();
        let pipeline = parent.downcast_ref::<gst::Pipeline>().unwrap();

        let elems = if session_id == 0 {
            // Let's use this stream to monitor what's going on
            let depay = gst::ElementFactory::make("rtpL16depay")
                .name("depay-0")
                .build()
                .unwrap();
            let queue_sink = gst::ElementFactory::make("queue")
                .name("queue-sink-0")
                .property("max-size-bytes", 0u32)
                .property("max-size-time", 150.mseconds())
                .property("max-size-buffers", 0u32)
                // Make sure not to block other streams because of this one
                .property_from_str("leaky", "downstream")
                .build()
                .unwrap();
            let sink = gst::ElementFactory::make("autoaudiosink")
                .name("sink-0")
                .build()
                .unwrap();
            vec![depay, queue_sink, sink]
        } else {
            // Regular stream
            let sink = gst::ElementFactory::make("fakesink")
                .name(format!("sink-{session_id}"))
                .property("drop-out-of-segment", false)
                .property("enable-last-sample", false)
                .build()
                .unwrap();
            vec![sink]
        };

        pipeline.add_many(&elems).unwrap();
        pad.link(&elems[0].static_pad("sink").unwrap()).unwrap();
        gst::Element::link_many(&elems).unwrap();

        for elem in elems.into_iter().rev() {
            elem.sync_state_with_parent().unwrap();
        }

        let left_to_add = left_to_add.fetch_sub(1, Ordering::SeqCst) - 1;
        if left_to_add.is_multiple_of(100) {
            println!("{} sessions", args.session_nb - left_to_add);
        }
    });

    let l = glib::MainLoop::new(None, false);

    let mut bus_recv_stream = pipeline.bus().unwrap().stream();

    main_context.spawn_local({
        let pipeline = pipeline.clone();
        async move {
            while let Some(msg) = bus_recv_stream.next().await {
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

    pipeline.debug_to_dot_file(gst::DebugGraphDetails::empty(), "rtpbin2-recv-stopping");
    pipeline.set_state(gst::State::Null).unwrap();

    // This is needed by some tracers to write their log file
    unsafe {
        gst::deinit();
    }
}
