// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// Sends audio and/or video streams with `sapsink` and announces them via SAP.
// The streams are sent to a multicast group by default. Can be received with
// the `sap-recv` example or any other SAP receiver. The announced session ID
// and the source address of the announcements are logged by sapsink at INFO
// level.
//
// cargo run -p gst-plugin-sdp --example sap-send -- --stream av --rtcp-mode sendrecv --session-id 1234

use clap::{Parser, ValueEnum};
use futures::prelude::*;
use gst::{glib, prelude::*};

// Some IPv4 domain-scoped multicast address from 239.0.0.0/8 (RFC 2365)
const DEFAULT_DEST_IP: &str = "239.1.2.1";
const DEFAULT_DEST_PORT: u16 = 5004;
const AUDIO_PT: u32 = 111;
const VIDEO_PT: u32 = 96;
// Both streams last about the same time: audiotestsrc is configured to
// produce 100 buffers per second, videotestsrc runs at its default 25 fps
const STREAM_DURATION_SECS: i32 = 20;
const AUDIO_RATE: i32 = 48000;
const AUDIO_SAMPLES_PER_BUFFER: i32 = 480;
const VIDEO_FPS: i32 = 25;
// Audio occupies (port, port + 1 for non-multiplexed RTCP), video starts at
// the next even port
const VIDEO_PORT_OFFSET: u16 = 2;

type CountMap = std::sync::Mutex<
    std::collections::BTreeMap<String, std::sync::Arc<std::sync::atomic::AtomicU64>>,
>;

/// Counts buffers arriving on `pad`, stored in `counts` under `label`
fn count_buffers(pad: &gst::Pad, counts: &CountMap, label: &str) {
    let count = std::sync::Arc::<std::sync::atomic::AtomicU64>::default();
    counts
        .lock()
        .unwrap()
        .insert(label.to_string(), count.clone());
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        if matches!(info.data.as_ref(), Some(gst::PadProbeData::Buffer(_))) {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        gst::PadProbeReturn::Ok
    });
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum RtcpMode {
    #[value(name = "inactive")]
    Inactive,
    #[value(name = "recvonly")]
    ReceiveOnly,
    #[value(name = "sendonly")]
    SendOnly,
    #[value(name = "sendrecv")]
    SendReceive,
}

impl RtcpMode {
    fn nick(self) -> &'static str {
        match self {
            Self::Inactive => "inactive",
            Self::ReceiveOnly => "recvonly",
            Self::SendOnly => "sendonly",
            Self::SendReceive => "sendrecv",
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum Stream {
    Audio,
    Av,
    Video,
}

#[derive(Parser, Debug)]
#[clap(version)]
#[clap(
    about = "Sends audio and/or video streams with sapsink and announces them via SAP. Can be received with the `sap-recv` example"
)]
struct Args {
    /// Local address used for the SAP announcements
    ///
    /// Derived from the multicast interface or the routing to the destination
    /// address by default
    #[clap(long)]
    source_address: Option<String>,

    /// RTP/RTCP destination address of all streams
    #[clap(long, default_value = DEFAULT_DEST_IP)]
    dest_ip: String,

    /// Base RTP port: audio is sent to this port, video to port + 2.
    /// RTCP is sent to port + 1 / port + 3 unless --rtcp-mux is used
    #[clap(long, default_value_t = DEFAULT_DEST_PORT)]
    dest_port: u16,

    /// Name of the announced session
    #[clap(long, default_value = "SAP sink example")]
    session_name: String,

    /// Session ID to announce in the SDP o= line, random by default
    ///
    /// Receivers can select the session with this ID, e.g. `sap-recv
    /// --session-id`
    #[clap(long)]
    session_id: Option<u64>,

    /// Streams to send and announce
    #[clap(long, value_enum, default_value = "audio")]
    stream: Stream,

    /// Duration of each stream in seconds
    #[clap(long, default_value_t = STREAM_DURATION_SECS)]
    duration: i32,

    /// RTCP mode of the session
    ///
    /// The end of each stream is signalled with an RTCP BYE. Receivers only
    /// notice it if they receive RTCP themselves, see the `sap-recv` example.
    #[clap(long, value_enum, default_value = "inactive")]
    rtcp_mode: RtcpMode,

    /// Multiplex RTP and RTCP on the same UDP port (a=rtcp-mux)
    #[clap(long)]
    rtcp_mux: bool,

    /// Disable multicast loopback, e.g. when sending to a different host
    #[clap(long)]
    no_multicast_loopback: bool,

    /// Comma-separated list of network interfaces to use for multicast
    #[clap(long)]
    multicast_iface: Option<String>,
}

/// Adds `elements` to the pipeline, links them in order and connects the
/// last one's src pad to the next requested `sink_%u` pad of sapsink
fn link_stream(
    pipeline: &gst::Pipeline,
    sapsink: &gst::Element,
    args: &Args,
    port: u16,
    label: &str,
    counts: &CountMap,
    elements: &[gst::Element],
) {
    pipeline.add_many(elements).unwrap();
    gst::Element::link_many(elements).unwrap();

    let pad = sapsink
        .request_pad_simple("sink_%u")
        .expect("Failed to request sapsink pad");
    pad.set_property("dest-ip", args.dest_ip.as_str());
    pad.set_property("dest-port", u32::from(port));
    count_buffers(&pad, counts, label);
    elements
        .last()
        .unwrap()
        .static_pad("src")
        .unwrap()
        .link(&pad)
        .expect("Failed to link payloader to sapsink");
}

fn add_audio(pipeline: &gst::Pipeline, sapsink: &gst::Element, args: &Args, counts: &CountMap) {
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .property("volume", 0.6f64)
        .property("samplesperbuffer", AUDIO_SAMPLES_PER_BUFFER)
        .property(
            "num-buffers",
            args.duration * (AUDIO_RATE / AUDIO_SAMPLES_PER_BUFFER),
        )
        .build()
        .unwrap();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("rate", AUDIO_RATE)
                .field("channels", 1i32)
                .build(),
        )
        .build()
        .unwrap();
    let encoder = gst::ElementFactory::make("opusenc").build().unwrap();
    let payloader = gst::ElementFactory::make("rtpopuspay2")
        .property("pt", AUDIO_PT)
        .build()
        .unwrap();

    link_stream(
        pipeline,
        sapsink,
        args,
        args.dest_port,
        "audio",
        counts,
        &[src, capsfilter, encoder, payloader],
    );
}

fn add_video(pipeline: &gst::Pipeline, sapsink: &gst::Element, args: &Args, counts: &CountMap) {
    let src = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .property("num-buffers", args.duration * VIDEO_FPS)
        .build()
        .unwrap();
    let encoder = gst::ElementFactory::make("vp8enc")
        .property("deadline", 1i64)
        // Insert a keyframe every second so that receivers joining late or
        // having lost packets can start decoding quickly
        .property("keyframe-max-dist", VIDEO_FPS)
        .build()
        .unwrap();
    let payloader = gst::ElementFactory::make("rtpvp8pay2")
        .property("pt", VIDEO_PT)
        .property_from_str("picture-id-mode", "7-bit")
        .build()
        .unwrap();

    let port = args
        .dest_port
        .checked_add(VIDEO_PORT_OFFSET)
        .expect("Video port out of range");
    link_stream(
        pipeline,
        sapsink,
        args,
        port,
        "video",
        counts,
        &[src, encoder, payloader],
    );
}

fn main() -> Result<(), anyhow::Error> {
    let args = Args::parse();

    gst::init()?;

    let pipeline = gst::Pipeline::default();

    let sapsink = gst::ElementFactory::make("sapsink")
        .property("session-name", args.session_name.as_str())
        .property("multicast-loopback", !args.no_multicast_loopback)
        .build()?;
    if let Some(source_address) = args.source_address.as_deref() {
        sapsink.set_property("source-address", source_address);
    }
    sapsink.set_property_from_str("rtcp-mode", args.rtcp_mode.nick());
    sapsink.set_property("rtcp-mux", args.rtcp_mux);
    if let Some(session_id) = args.session_id {
        sapsink.set_property("session-id", session_id);
    }
    if let Some(iface) = args.multicast_iface.as_deref() {
        sapsink.set_property_from_str("multicast-iface", iface);
    }
    pipeline.add(&sapsink)?;

    let sent = CountMap::default();
    if matches!(args.stream, Stream::Audio | Stream::Av) {
        add_audio(&pipeline, &sapsink, &args, &sent);
    }
    if matches!(args.stream, Stream::Video | Stream::Av) {
        add_video(&pipeline, &sapsink, &args, &sent);
    }

    let context = glib::MainContext::default();
    pipeline.set_state(gst::State::Playing)?;

    let mut failed = None;
    context.block_on(async {
        let bus = pipeline.bus().expect("Pipeline should have a bus");
        let mut messages = bus.stream();

        while let Some(msg) = messages.next().await {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(..) => break,
                MessageView::Latency(..) => {
                    pipeline.call_async(|pipeline| {
                        let _ = pipeline.recalculate_latency();
                    });
                }
                MessageView::Error(err) => {
                    println!(
                        "Error from {:?}: {}",
                        err.src().map(|s| s.path_string()),
                        err
                    );
                    failed = Some(err.error());
                    break;
                }
                _ => (),
            }
        }
    });

    let _ = pipeline.set_state(gst::State::Null);

    if let Some(err) = failed {
        return Err(err.into());
    }

    for (name, count) in sent.lock().unwrap().iter() {
        println!(
            "{name}: {} buffers sent",
            count.load(std::sync::atomic::Ordering::Relaxed)
        );
    }
    Ok(())
}
