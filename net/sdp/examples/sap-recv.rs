// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// Receives a session announced with SAP via `sapsrc`, depayloads and decodes
// the streams with decodebin3 and renders them. Also counts the buffers of
// every stream. Complements the `sap-send` example. Start it before
// `sap-send`. One of --session-id (the same value as the --session-id of
// `sap-send`) or --originating-source has to be given to select the session,
// see the documentation of the arguments.
//
// cargo run -p gst-plugin-sdp --example sap-send -- --stream av --rtcp-mode sendrecv --session-id 1234
// cargo run -p gst-plugin-sdp --example sap-recv -- --session-id 1234 --rtcp-mode sendrecv

use anyhow::Context as _;
use clap::{Parser, ValueEnum};
use futures::prelude::*;
use gst::{glib, prelude::*};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

type CountMap = Mutex<BTreeMap<String, Arc<AtomicU64>>>;

// Stream ID and caps of a sapsrc pad, captured from the stream-start and
// caps events
type StreamInfo = Arc<Mutex<(Option<String>, Option<gst::Caps>)>>;

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

#[derive(Parser, Debug)]
#[clap(version)]
#[clap(
    about = "Receives a session announced with SAP via sapsrc, depayloads and decodes the streams with decodebin3 and renders them. To be used together with the `sap-send` example"
)]
struct Args {
    /// Only handle sessions announced by this originating address
    ///
    /// This is the address the sender announced from: `sapsink` derives it
    /// from the routing to the media destination unless its source-address
    /// is configured explicitly, so it is usually the sender's LAN address
    /// and not 127.0.0.1
    ///
    /// Either this or --session-id has to be given
    #[clap(long)]
    originating_source: Option<String>,

    /// Only handle sessions with this session ID (session ID of the SDP o= line)
    ///
    /// Either this or --originating-source has to be given
    #[clap(long)]
    session_id: Option<String>,

    /// RTCP mode of the session
    ///
    /// Without RTCP reception (`inactive`) the end of a stream can not be
    /// detected: the sender signals it with an RTCP BYE, so the pipeline only
    /// reaches EOS with `recvonly` or `sendrecv`.
    #[clap(long, value_enum, default_value = "inactive")]
    rtcp_mode: RtcpMode,

    /// Disable multicast loopback, e.g. when receiving from a different host
    #[clap(long)]
    no_multicast_loopback: bool,

    /// Comma-separated list of network interfaces to use for multicast
    #[clap(long)]
    multicast_iface: Option<String>,

    /// Use fakesinks instead of rendering to audio/video sinks
    #[clap(long)]
    fakesink: bool,
}

fn main() -> Result<(), anyhow::Error> {
    let args = Args::parse();

    gst::init()?;

    let pipeline = gst::Pipeline::default();
    pipeline.set_bin_flags(gst::BinFlags::STREAMS_AWARE);

    let sapsrc = gst::ElementFactory::make("sapsrc")
        .property("multicast-loopback", !args.no_multicast_loopback)
        .build()
        .context("Failed to instantiate sapsrc")?;
    sapsrc.set_property_from_str("rtcp-mode", args.rtcp_mode.nick());
    if let Some(originating_source) = args.originating_source.as_deref() {
        sapsrc.set_property_from_str("originating-source", originating_source);
    }
    if let Some(session_id) = args.session_id.as_deref() {
        sapsrc.set_property_from_str("session-id", session_id);
    }
    if let Some(iface) = args.multicast_iface.as_deref() {
        sapsrc.set_property_from_str("multicast-iface", iface);
    }
    pipeline.add(&sapsrc)?;

    let decodebin = gst::ElementFactory::make("decodebin3")
        .build()
        .context("Failed to instantiate decodebin3")?;
    pipeline.add(&decodebin)?;

    // Render every decoded audio/video stream that decodebin3 exposes
    let fakesink = args.fakesink;
    let weak = pipeline.downgrade();
    decodebin.connect_pad_added(move |_, pad| {
        let Some(pipeline) = weak.upgrade() else {
            return;
        };

        // The sink pads are requested in the sapsrc pad-added handler below
        if pad.name().starts_with("sink_") {
            return;
        }

        let Some(stream) = pad.stream() else {
            eprintln!("Pad {} has no stream", pad.name());
            return;
        };

        // Per-stream name prefix for the queues below: the pad name keeps it
        // unique even for several streams of the same type
        let (label, converters, sink_name) = if fakesink {
            (format!("fake-{}", pad.name()), &[][..], "fakesink")
        } else if stream.stream_type().contains(gst::StreamType::VIDEO) {
            (
                format!("video-{}", pad.name()),
                &["videoscale", "videoconvert"][..],
                "autovideosink",
            )
        } else if stream.stream_type().contains(gst::StreamType::AUDIO) {
            (
                format!("audio-{}", pad.name()),
                &["audioconvert", "audioresample"][..],
                "autoaudiosink",
            )
        } else {
            let stream_id = stream.stream_id().unwrap_or_else(|| "<none>".into());
            eprintln!("Ignoring stream {stream_id}");
            return;
        };

        let mut elements = Vec::new();

        let decode_queue = gst::ElementFactory::make("queue")
            .name(format!("{label}-decode-queue"))
            .property("max-size-bytes", 0u32)
            // Add enough buffering to cope with differences in branch latency
            // and transitions
            .property("max-size-time", 250.mseconds())
            .property("max-size-buffers", 0u32)
            .build()
            .unwrap();
        elements.push(decode_queue);

        for name in converters {
            match gst::ElementFactory::make(name).build() {
                Ok(element) => elements.push(element),
                Err(err) => {
                    eprintln!("Failed to instantiate element {name}: {err}");
                    return;
                }
            }
        }

        let sink_queue = gst::ElementFactory::make("queue")
            .name(format!("{label}-sink-queue"))
            .property("max-size-bytes", 0u32)
            .property("max-size-time", gst::ClockTime::ZERO)
            .property("max-size-buffers", 1u32)
            .build()
            .unwrap();
        elements.push(sink_queue);

        let sink = match gst::ElementFactory::make(sink_name).build() {
            Ok(sink) => sink,
            Err(err) => {
                eprintln!("Failed to instantiate sink {sink_name}: {err}");
                return;
            }
        };
        if fakesink {
            sink.set_property("sync", false);
            sink.set_property("async", false);
        } else {
            // Make sure this branch doesn't block the other streams if the
            // sink can't produce its first buffer immediately, e.g. while
            // waiting for a keyframe
            sink.downcast_ref::<gst::Bin>()
                .expect("auto sink is a bin")
                .connect_deep_element_added(|_sink, _sub_bin, elem| {
                    if elem.has_property("async") {
                        elem.set_property("async", false);
                    }
                });
        }
        elements.push(sink);

        if let Err(err) = pipeline.add_many(&elements) {
            eprintln!("Failed to add elements to pipeline: {err}");
            return;
        }
        if let Err(err) = pad.link(&elements[0].static_pad("sink").unwrap()) {
            eprintln!("Failed to link pad {}: {err}", pad.name());
            return;
        }
        if let Err(err) = gst::Element::link_many(&elements) {
            eprintln!(
                "Failed to link sink chain for stream on pad {}: {err}",
                pad.name()
            );
            return;
        }
        // Sync the states downstream first so that the pads are activated in
        // the right order
        for element in elements.iter().rev() {
            if let Err(err) = element.sync_state_with_parent() {
                eprintln!("Failed to sync state of element {}: {err}", element.name());
                return;
            }
        }
    });

    let received = Arc::<CountMap>::default();
    let received_for_pads = received.clone();
    let decodebin = decodebin.downgrade();
    sapsrc.connect_pad_added(move |_, pad| {
        let Some(decodebin) = decodebin.upgrade() else {
            return;
        };

        // decodebin3 depayloads and decodes the RTP stream of the pad and
        // exposes a pad for every stream it selects
        let Some(sink_pad) = decodebin.request_pad_simple("sink_%u") else {
            eprintln!("Failed to request sink pad on decodebin3");
            return;
        };
        if let Err(err) = pad.link(&sink_pad) {
            eprintln!("Failed to link pad {}: {err}", pad.name());
            return;
        }

        // Print the stream ID and caps of sapsrc once they arrive and count
        // the buffers of every stream
        let name = pad.name().to_string();
        let stream = StreamInfo::default();
        let count = Arc::<AtomicU64>::default();
        received_for_pads
            .lock()
            .unwrap()
            .insert(name.clone(), count.clone());
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_pad, info| {
                let mut stream = stream.lock().unwrap();
                match info.data.as_ref() {
                    Some(gst::PadProbeData::Event(event)) => match event.view() {
                        gst::EventView::StreamStart(stream_start) => {
                            stream.0 = Some(stream_start.stream_id().into());
                        }
                        gst::EventView::Caps(caps) => stream.1 = Some(caps.caps_owned()),
                        _ => (),
                    },
                    Some(gst::PadProbeData::Buffer(_)) => {
                        let count = count.fetch_add(1, Ordering::Relaxed);
                        if count == 0 {
                            let stream_id = stream.0.clone().unwrap_or_else(|| "<none>".into());
                            let caps = stream
                                .1
                                .as_ref()
                                .map(|caps| caps.to_string())
                                .unwrap_or_else(|| "<none>".into());
                            println!("Received stream {name}: stream-id={stream_id} caps={caps}");
                        }
                        if count.is_multiple_of(100) {
                            println!("{name}: {count} buffers");
                        }
                    }
                    _ => (),
                }
                gst::PadProbeReturn::Ok
            },
        );
    });

    let context = glib::MainContext::default();
    pipeline.set_state(gst::State::Playing)?;

    context.block_on(async {
        let bus = pipeline.bus().expect("Pipeline should have a bus");
        let mut messages = bus.stream();

        let mut ret = Ok(());
        while let Some(msg) = messages.next().await {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(..) => {
                    println!("EOS");
                    for (name, count) in received.lock().unwrap().iter() {
                        println!("{name}: {} buffers received", count.load(Ordering::Relaxed));
                    }
                    break;
                }
                MessageView::Latency(..) => {
                    pipeline.call_async(|pipeline| {
                        let _ = pipeline.recalculate_latency();
                    });
                }
                MessageView::Error(err) => {
                    // No EOS will ever arrive after an error
                    ret = Err(anyhow::anyhow!(
                        "Error from {:?}: {}",
                        err.src().map(|s| s.path_string()),
                        err
                    ));
                    break;
                }
                _ => (),
            }
        }
        ret
    })?;

    let _ = pipeline.set_state(gst::State::Null);
    Ok(())
}
