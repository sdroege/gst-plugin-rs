// Copyright (C) 2026, Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use clap::Parser;
use gst::glib;
use gst::prelude::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn parse_ssrc(s: &str) -> Result<u32, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).map_err(|e| e.to_string())
    } else {
        s.parse::<u32>().map_err(|e| e.to_string())
    }
}

#[derive(Parser, Debug)]
#[clap(version)]
#[clap(about = "Play back one RTP stream from a PCAP/PCAPNG file, selected by SSRC")]
struct Args {
    /// SSRC (decimal or 0x-prefixed hexadecimal) of the RTP stream to play
    /// back. If not given, the first stream found in the capture is played.
    #[clap(long, value_parser = parse_ssrc)]
    ssrc: Option<u32>,

    /// Path to the PCAP or PCAPNG capture file.
    #[clap(long)]
    location: String,

    /// Caps of the RTP packets in the capture file.
    #[clap(
        long,
        default_value = "application/x-rtp,media=audio,clock-rate=48000,encoding-name=OPUS,payload=96"
    )]
    caps: String,

    #[clap(long)]
    src_ip: Option<String>,

    #[clap(long)]
    dst_ip: Option<String>,

    #[clap(long)]
    src_port: Option<u16>,

    #[clap(long)]
    dst_port: Option<u16>,
}

fn main() {
    let args = Args::parse();

    gst::init().expect("Failed to initialize GStreamer");
    gstpcap::plugin_register_static().expect("Failed to register the pcapparse2 plugin");

    let caps: gst::Caps = args
        .caps
        .parse()
        .unwrap_or_else(|_| panic!("Failed to parse caps '{}'", args.caps));

    let is_video = caps.iter().any(
        |structure| matches!(structure.get::<String>("media"), Ok(ref media) if media == "video"),
    );

    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", &args.location)
        .build()
        .expect("Failed to create filesrc");
    let pcapparse = gst::ElementFactory::make("pcapparse2")
        .property("caps", Some(&caps))
        .build()
        .expect("Failed to create pcapparse2");

    if let Some(src_ip) = args.src_ip.as_deref() {
        pcapparse.set_property("src-ip", src_ip);
    }

    if let Some(dst_ip) = args.dst_ip.as_deref() {
        pcapparse.set_property("dst-ip", dst_ip);
    }

    if let Some(src_port) = args.src_port {
        pcapparse.set_property("src-port", u32::from(src_port));
    }

    if let Some(dst_port) = args.dst_port {
        pcapparse.set_property("dst-port", u32::from(dst_port));
    }

    let rtpssrcdemux = gst::ElementFactory::make("rtpssrcdemux")
        .build()
        .expect("Failed to create rtpssrcdemux");

    let pipeline = gst::Pipeline::new();
    pipeline
        .add_many([&filesrc, &pcapparse, &rtpssrcdemux])
        .expect("Failed to add elements to pipeline");

    gst::Element::link_many([&filesrc, &pcapparse, &rtpssrcdemux])
        .expect("Failed to link filesrc ! pcapparse2 ! rtpssrcdemux");

    let ssrcs_seen: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let ssrcs_seen_cb = ssrcs_seen.clone();

    // SSRC to play back. If no explicit `--ssrc` is given, the first pad/SSRC
    // demuxed by rtpssrcdexmux will be used.
    let play_ssrc: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(args.ssrc));
    let play_ssrc_cb = play_ssrc.clone();

    let found_target_ssrc = Arc::new(AtomicBool::new(false));
    let found_target_ssrc_cb = found_target_ssrc.clone();

    let pipeline_weak = pipeline.downgrade();
    rtpssrcdemux.connect("new-ssrc-pad", false, move |values| {
        let pipeline = pipeline_weak.upgrade()?;
        let ssrc = values[1].get::<u32>().expect("ssrc argument");
        let pad = values[2].get::<gst::Pad>().expect("pad argument");

        ssrcs_seen_cb.lock().unwrap().push(ssrc);
        eprintln!("Found SSRC {ssrc:#010x}");

        let mut play_ssrc = play_ssrc_cb.lock().unwrap();
        let target_ssrc = match *play_ssrc {
            Some(target_ssrc) => target_ssrc,
            None => {
                *play_ssrc = Some(ssrc);
                ssrc
            }
        };
        drop(play_ssrc);

        if ssrc != target_ssrc {
            let fakesink = gst::ElementFactory::make("fakesink")
                .name(format!("drop-{ssrc:#x}"))
                .build()
                .expect("Failed to create fakesink");
            pipeline
                .add(&fakesink)
                .expect("Failed to add fakesink to pipeline");
            pad.link(&fakesink.static_pad("sink").unwrap())
                .expect("Failed to link pad to fakesink");

            fakesink
                .sync_state_with_parent()
                .expect("Failed to sync fakesink state");

            return None;
        }

        found_target_ssrc_cb.store(true, Ordering::Relaxed);
        eprintln!("Playing back stream with SSRC {ssrc:#010x}");

        let decodebin = gst::ElementFactory::make("decodebin")
            .name(format!("decode-{ssrc:#x}"))
            .build()
            .expect("Failed to create decodebin");
        let queue = gst::ElementFactory::make("queue")
            .name(format!("queue-{ssrc:#x}"))
            .build()
            .expect("Failed to create queue");

        let convert = gst::ElementFactory::make(if is_video {
            "videoconvert"
        } else {
            "audioconvert"
        })
        .name(format!("convert-{ssrc:#x}"))
        .build()
        .expect("Failed to create audioconvert/videoconvert");

        let sink = gst::ElementFactory::make(if is_video {
            "autovideosink"
        } else {
            "autoaudiosink"
        })
        .name(format!("sink-{ssrc:#x}"))
        .build()
        .expect("Failed to create autoaudiosink/autovideosink");

        let queue_sink_pad = queue.static_pad("sink").unwrap().clone();
        decodebin.connect_pad_added(move |_decodebin, pad| {
            pad.link(&queue_sink_pad)
                .expect("Failed to link decodebin output to queue");
        });

        pipeline
            .add_many([&decodebin, &queue, &convert, &sink])
            .expect("Failed to add playback elements to pipeline");

        gst::Element::link_many([&queue, &convert, &sink])
            .expect("Failed to link queue ! convert ! sink");

        pad.link(&decodebin.static_pad("sink").unwrap())
            .expect("Failed to link SSRC pad to decodebin");

        for element in [&decodebin, &queue, &convert, &sink] {
            element
                .sync_state_with_parent()
                .expect("Failed to sync playback elements to playing");
        }

        None
    });

    let main_loop = glib::MainLoop::new(None, false);
    let main_loop_bus = main_loop.clone();

    let _bus_watch = pipeline
        .bus()
        .unwrap()
        .add_watch(move |_bus, msg| {
            use gst::MessageView::*;

            match msg.view() {
                Error(err) => {
                    eprintln!(
                        "Error from {}: {} ({})",
                        msg.src()
                            .map(|s| s.path_string())
                            .unwrap_or_else(|| "unknown".into()),
                        err.error(),
                        err.debug().unwrap_or_default()
                    );
                    main_loop_bus.quit();
                }
                Eos(_) => {
                    main_loop_bus.quit();
                }
                _ => (),
            }
            glib::ControlFlow::Continue
        })
        .expect("Failed to add bus watch");

    ctrlc::set_handler({
        let main_loop = main_loop.clone();
        move || {
            main_loop.quit();
        }
    })
    .expect("Failed to set Ctrl+C handler");

    pipeline
        .set_state(gst::State::Playing)
        .expect("Failed to set pipeline to Playing");

    match args.ssrc {
        Some(ssrc) => eprintln!("Playing back SSRC {ssrc:#010x} from {}", args.location),
        None => eprintln!("Playing back the first stream found in {}", args.location),
    }

    main_loop.run();

    pipeline
        .set_state(gst::State::Null)
        .expect("Failed to set pipeline to Null");

    let ssrcs_seen = ssrcs_seen.lock().unwrap();
    match (args.ssrc, found_target_ssrc.load(Ordering::Relaxed)) {
        (Some(requested), false) => {
            eprintln!(
                "Requested SSRC {requested:#010x} was not found. SSRCs in {}: {}",
                args.location,
                ssrcs_seen
                    .iter()
                    .map(|s| format!("{s:#010x}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        _ => {
            eprintln!(
                "SSRCs found: {}",
                ssrcs_seen
                    .iter()
                    .map(|s| format!("{s:#010x}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
}
