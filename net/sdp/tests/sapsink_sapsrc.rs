// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// End-to-end tests running `sapsink` and `sapsrc` against each other on
// localhost: SAP announcement, session build-up, RTP/RTCP transfer and
// session teardown.

use futures::prelude::*;
use gst::{glib, prelude::*};
use std::{
    collections::{BTreeMap, HashSet},
    ops::ControlFlow,
    sync::{
        Arc, LazyLock, Mutex, Once,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "sap-test",
        gst::DebugColorFlags::empty(),
        Some("SAP pair tests"),
    )
});

// Both streams last about the same time, short enough to keep the tests
// fast but long enough to survive the receiver discovery/setup phase
const STREAM_DURATION_SECS: u32 = 2;
const AUDIO_RATE: u32 = 48000;
const AUDIO_SAMPLES_PER_BUFFER: u32 = 480;
const AUDIO_NUM_BUFFERS: u32 = STREAM_DURATION_SECS * (AUDIO_RATE / AUDIO_SAMPLES_PER_BUFFER);
const VIDEO_FPS: u32 = 25;
const VIDEO_NUM_BUFFERS: u32 = STREAM_DURATION_SECS * VIDEO_FPS;
const AUDIO_PT: u32 = 111;
const VIDEO_PT: u32 = 96;
const VIDEO_PORT_OFFSET: u16 = 2;

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsudp::plugin_register_static().unwrap();
        gstrsrtp::plugin_register_static().unwrap();
        gstsdp::plugin_register_static().unwrap();
    });
}

type CountMap = Mutex<BTreeMap<String, Arc<AtomicU64>>>;

fn count_pad(pad: &gst::Pad, counts: &CountMap, name: &str) {
    let count = Arc::<AtomicU64>::default();
    counts
        .lock()
        .unwrap()
        .insert(name.to_string(), count.clone());
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_pad, info| {
            let n = match info.data.as_ref() {
                Some(gst::PadProbeData::Buffer(_)) => 1,
                Some(gst::PadProbeData::BufferList(list)) => list.len() as u64,
                _ => 0,
            };
            count.fetch_add(n, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        },
    );
}

fn total(counts: &CountMap, name: &str) -> u64 {
    counts
        .lock()
        .unwrap()
        .get(name)
        .map(|count| count.load(Ordering::Relaxed))
        .unwrap_or_default()
}

// Takes the RTCP BYE of a stream out of the flow inside sapsrc and re-injects
// it once everything was output: the teardown on BYE otherwise discards the
// end of the stream still held by the jitterbuffer, and on a loaded machine
// the BYE can overtake the last not yet processed RTP packets.
// This is only necessary to have all RTP packets be processed and to allow
// asserting an exact count in the end.
fn hold_bye(element: &gst::Element, stream: u32, sent: Arc<CountMap>, received: Arc<CountMap>) {
    let Some(pad) = element.static_pad("src") else {
        return;
    };
    let probe_pad = pad.downgrade();
    let element = element.downgrade();
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_pad, info| {
            let (Some(element), Some(pad)) = (element.upgrade(), probe_pad.upgrade()) else {
                return gst::PadProbeReturn::Ok;
            };
            if matches!(info.data.as_ref(), Some(gst::PadProbeData::Buffer(buffer)) if contains_bye(buffer))
            {
                let bye = info.take_buffer().unwrap();
                reinject_bye(&element, pad, bye, Arc::clone(&sent), Arc::clone(&received), stream);
                return gst::PadProbeReturn::Drop;
            }
            let Some(mut list) = info.take_buffer_list() else {
                return gst::PadProbeReturn::Ok;
            };
            let mut byes = Vec::new();
            list.make_mut().foreach_mut(|buffer, _| {
                if contains_bye(&buffer) {
                    byes.push(buffer);
                    ControlFlow::Continue(None)
                } else {
                    ControlFlow::Continue(Some(buffer))
                }
            });
            for bye in byes {
                reinject_bye(
                    &element,
                    pad.clone(),
                    bye,
                    Arc::clone(&sent),
                    Arc::clone(&received),
                    stream,
                );
            }
            if list.is_empty() {
                return gst::PadProbeReturn::Drop;
            }
            info.data = Some(gst::PadProbeData::BufferList(list));
            gst::PadProbeReturn::Ok
        },
    );
}

fn reinject_bye(
    element: &gst::Element,
    pad: gst::Pad,
    bye: gst::Buffer,
    sent: Arc<CountMap>,
    received: Arc<CountMap>,
    stream: u32,
) {
    element.call_async(move |_obj| {
        wait_drained(&sent, &received, stream);
        // Chained on the peer pad, our probe on the udpsrc2 pad would capture
        // the BYE again. Nothing else can be in flight, the sender is done
        // after sending its BYE
        if let Some(peer) = pad.peer() {
            let _ = peer.chain(bye);
        }
    });
}

// Re-injecting the BYE on timeout only fails the count assertions later
fn wait_drained(sent: &Arc<CountMap>, received: &Arc<CountMap>, stream: u32) {
    let sent_name = format!("stream{stream}");
    let received_name = format!("src_{stream}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while total(received, &received_name) < total(sent, &sent_name) {
        if Instant::now() >= deadline {
            gst::warning!(CAT, "Stream {stream} did not fully drain");
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// Checks for a BYE in an RTCP compound. With a=rtcp-mux the RTP of the test
// streams arrives on the same socket and is skipped by its payload type
fn contains_bye(buffer: &gst::BufferRef) -> bool {
    let Ok(data) = buffer.map_readable() else {
        return false;
    };

    if data.len() >= 2 && (data[1] & 0x7f == AUDIO_PT as u8 || data[1] & 0x7f == VIDEO_PT as u8) {
        return false;
    }

    rtcp_types::Compound::parse(&data).is_ok_and(|mut compound| {
        compound.any(|packet| matches!(packet, Ok(rtcp_types::Packet::Bye(_))))
    })
}

fn build_sender(
    session_id: u64,
    base_port: u16,
    rtcp_mode: &str,
    rtcp_mux: bool,
    video: bool,
) -> (
    gst::Pipeline,
    Arc<CountMap>,
    Vec<(gst::Pad, gst::PadProbeId)>,
) {
    let pipeline = gst::Pipeline::default();
    let sapsink = gst::ElementFactory::make("sapsink")
        .property("source-address", "127.0.0.1")
        .property("multicast-loopback", true)
        .property("session-id", session_id)
        .property("session-name", "pair test")
        .build()
        .unwrap();
    sapsink.set_property_from_str("rtcp-mode", rtcp_mode);
    sapsink.set_property("rtcp-mux", rtcp_mux);
    pipeline.add(&sapsink).unwrap();

    let sent = Arc::<CountMap>::default();
    // sapsink's pads blocked until the receiver is set up, see wait_receiver()
    let mut blocked = Vec::new();

    let mut add_stream = |pad_id: u32, port: u16, chain: Vec<gst::Element>| {
        pipeline.add_many(chain.iter()).unwrap();
        gst::Element::link_many(chain.iter()).unwrap();

        let pad = sapsink
            .request_pad_simple(&format!("sink_{pad_id}"))
            .unwrap();
        pad.set_property("dest-ip", "127.0.0.1");
        pad.set_property("dest-port", u32::from(port));
        let pay_src = chain.last().unwrap().static_pad("src").unwrap();
        count_pad(&pay_src, &sent, &format!("stream{pad_id}"));
        pay_src.link(&pad).unwrap();

        // Blocks only data: the caps events announcing the session pass through
        let block = pad
            .add_probe(
                gst::PadProbeType::BUFFER
                    | gst::PadProbeType::BUFFER_LIST
                    | gst::PadProbeType::BLOCK,
                |_pad, _info| gst::PadProbeReturn::Ok,
            )
            .unwrap();
        blocked.push((pad.clone(), block));
    };

    add_stream(
        0,
        base_port,
        vec![
            gst::ElementFactory::make("audiotestsrc")
                .property("is-live", true)
                .property("volume", 0.03f64)
                .property("samplesperbuffer", AUDIO_SAMPLES_PER_BUFFER as i32)
                .property("num-buffers", AUDIO_NUM_BUFFERS as i32)
                .build()
                .unwrap(),
            gst::ElementFactory::make("capsfilter")
                .property(
                    "caps",
                    gst::Caps::builder("audio/x-raw")
                        .field("rate", AUDIO_RATE as i32)
                        .field("channels", 1i32)
                        .build(),
                )
                .build()
                .unwrap(),
            gst::ElementFactory::make("opusenc").build().unwrap(),
            gst::ElementFactory::make("rtpopuspay2")
                .property("pt", AUDIO_PT)
                .build()
                .unwrap(),
        ],
    );
    if video {
        add_stream(
            1,
            base_port
                .checked_add(VIDEO_PORT_OFFSET)
                .expect("video port out of range"),
            vec![
                gst::ElementFactory::make("videotestsrc")
                    .property("is-live", true)
                    .property("num-buffers", VIDEO_NUM_BUFFERS as i32)
                    .build()
                    .unwrap(),
                gst::ElementFactory::make("vp8enc")
                    .property("deadline", 1i64)
                    .build()
                    .unwrap(),
                gst::ElementFactory::make("rtpvp8pay2")
                    .property("pt", VIDEO_PT)
                    .property_from_str("picture-id-mode", "7-bit")
                    .build()
                    .unwrap(),
            ],
        );
    }

    (pipeline, sent, blocked)
}

fn build_receiver(session_id: u64, rtcp_mode: &str) -> (gst::Pipeline, Arc<CountMap>) {
    let pipeline = gst::Pipeline::default();
    let sapsrc = gst::ElementFactory::make("sapsrc")
        .property("session-id", session_id.to_string())
        .property("multicast-loopback", true)
        .build()
        .unwrap();
    sapsrc.set_property_from_str("rtcp-mode", rtcp_mode);
    pipeline.add(&sapsrc).unwrap();

    let received = Arc::<CountMap>::default();
    let counts = received.clone();
    let weak = pipeline.downgrade();
    sapsrc.connect_pad_added(move |_, pad| {
        let Some(pipeline) = weak.upgrade() else {
            return;
        };
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        if pipeline
            .add(&sink)
            .and_then(|()| sink.sync_state_with_parent())
            .is_err()
        {
            gst::error!(CAT, obj = pad, "Failed to add fakesink for discovered pad");
            return;
        }

        let name = pad.name().to_string();
        count_pad(pad, &counts, &name);
        pad.link(&sink.static_pad("sink").unwrap()).unwrap();
    });

    (pipeline, received)
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Eos,
    Error,
    Timeout,
}

// Pumps the bus until EOS or an error (logged with `name`) or `secs` pass.
// `on_msg` sees every other message and may end the wait early
async fn wait_bus(
    pipeline: &gst::Pipeline,
    name: &str,
    secs: u32,
    mut on_msg: impl FnMut(&gst::Message) -> Option<Outcome>,
) -> Outcome {
    let bus = pipeline.bus().unwrap();
    let mut messages = bus.stream();
    let pump = async {
        while let Some(msg) = messages.next().await {
            match msg.view() {
                gst::MessageView::Eos(..) => return Outcome::Eos,
                gst::MessageView::Error(err) => {
                    gst::error!(CAT, obj = pipeline, "Error on {name} bus: {err}");
                    return Outcome::Error;
                }
                _ => {
                    if let Some(outcome) = on_msg(&msg) {
                        return outcome;
                    }
                }
            }
        }
        Outcome::Error
    };
    futures::pin_mut!(pump);
    match futures::future::select(glib::timeout_future_seconds(secs), pump).await {
        futures::future::Either::Left(_) => Outcome::Timeout,
        futures::future::Either::Right((outcome, _)) => outcome,
    }
}

// Waits for sapsrc's udpsrc2 children to reach PLAYING, installs hold_bye()
// on them and unblocks the sender's pads then, i.e. when their sockets are
// bound and the receiver would not lose packets anymore. If the setup never
// completes, the sender runs into its own wait_bus() timeout.
async fn wait_receiver(
    pipeline: &gst::Pipeline,
    expected_udpsrcs: usize,
    sent: Arc<CountMap>,
    received: Arc<CountMap>,
    mut blocked: Vec<(gst::Pad, gst::PadProbeId)>,
    secs: u32,
) -> Outcome {
    let mut ready = HashSet::new();
    wait_bus(pipeline, "receiver", secs, |msg| {
        let gst::MessageView::StateChanged(sc) = msg.view() else {
            return None;
        };
        let src = msg
            .src()
            .and_then(|src| src.downcast_ref::<gst::Object>())?;
        let name = src.name();
        if sc.current() != gst::State::Playing
            || !name.starts_with("udpsrc2-")
            || !ready.insert(name.to_string())
        {
            return None;
        }

        // Element name, e.g. "udpsrc2-rtcp-1": the stream is the trailing
        // mline index
        let stream = name
            .rsplit('-')
            .next()
            .and_then(|id| id.parse::<u32>().ok());
        if let (Some(element), Some(stream)) = (src.downcast_ref::<gst::Element>(), stream) {
            hold_bye(element, stream, Arc::clone(&sent), Arc::clone(&received));
        }

        if ready.len() >= expected_udpsrcs {
            gst::info!(CAT, obj = pipeline, "Receiver is set up, unblocking sender");
            for (pad, id) in blocked.drain(..) {
                pad.remove_probe(id);
            }
        }
        None
    })
    .await
}

struct CaseResult {
    sender: Outcome,
    receiver: Outcome,
    sent_audio: u64,
    sent_video: u64,
    received_audio: u64,
    received_video: u64,
}

// Runs a `sapsink` pipeline against a `sapsrc` pipeline until both are done
// or timeouts hit. `receiver_timeout` only applies to the receiver.
fn run_case(
    session_id: u64,
    base_port: u16,
    rtcp_mode: &str,
    rtcp_mux: bool,
    video: bool,
    receiver_timeout: u32,
) -> CaseResult {
    init();
    let (sender, sent, blocked) = build_sender(session_id, base_port, rtcp_mode, rtcp_mux, video);
    let (receiver, received) = build_receiver(session_id, rtcp_mode);

    // One udpsrc2 per stream for RTP, plus one for RTCP unless inactive or
    // muxed onto the RTP port
    let expected_udpsrcs =
        (1 + usize::from(video)) * (1 + usize::from(rtcp_mode != "inactive" && !rtcp_mux));

    let wait_sent = Arc::clone(&sent);
    let wait_received = Arc::clone(&received);

    // A private main context per test keeps them independent of each other
    let context = glib::MainContext::new();
    let (sender_outcome, receiver_outcome) = context.block_on(async move {
        // The receiver has to be listening before the first announcement is
        // sent, and both need an iterated main context for their event loops
        receiver.set_state(gst::State::Playing).unwrap();
        sender.set_state(gst::State::Playing).unwrap();

        let (sender_outcome, receiver_outcome) = futures::future::join(
            wait_bus(&sender, "sender", 10, |_| None),
            wait_receiver(
                &receiver,
                expected_udpsrcs,
                wait_sent,
                wait_received,
                blocked,
                receiver_timeout,
            ),
        )
        .await;
        sender.set_state(gst::State::Null).unwrap();
        receiver.set_state(gst::State::Null).unwrap();
        (sender_outcome, receiver_outcome)
    });

    CaseResult {
        sender: sender_outcome,
        receiver: receiver_outcome,
        sent_audio: total(&sent, "stream0"),
        sent_video: total(&sent, "stream1"),
        received_audio: total(&received, "src_0"),
        received_video: total(&received, "src_1"),
    }
}

fn assert_all(sent: u64, received: u64, stream: &str) {
    assert!(sent > 0, "{stream}: no buffers were sent");
    assert_eq!(
        received, sent,
        "{stream}: received {received} of {sent} buffers"
    );
}

// The process ID keeps concurrent or stale runs apart on the host-global SAP
// multicast. sapsink generates a random session ID for 0, so the cases must
// be non-zero (RFC 8866 §5.2: session IDs are numeric)
fn session_id(case: u32) -> u64 {
    u64::from(std::process::id()) * 10 + u64::from(case)
}

// Without RTCP there is no way to signal the end of the stream to the
// receiver, so it keeps running after the sender is done.
#[test]
fn test_rtcp_inactive() {
    // A (hypothetical) EOS would arrive at the same latency as in the
    // RTCP-enabled tests, a few seconds beyond the end of the stream
    let result = run_case(session_id(1), 15004, "inactive", false, true, 5);
    assert_eq!(result.sender, Outcome::Eos);
    assert_eq!(result.receiver, Outcome::Timeout);
    assert_all(result.sent_audio, result.received_audio, "audio");
    assert_all(result.sent_video, result.received_video, "video");
}

#[test]
fn test_rtcp_sendrecv() {
    let result = run_case(session_id(2), 15014, "sendrecv", false, false, 30);
    assert_eq!(result.sender, Outcome::Eos);
    assert_eq!(result.receiver, Outcome::Eos);
    assert_all(result.sent_audio, result.received_audio, "audio");
}

#[test]
fn test_rtcp_sendrecv_mux() {
    let result = run_case(session_id(3), 15024, "sendrecv", true, true, 30);
    assert_eq!(result.sender, Outcome::Eos);
    assert_eq!(result.receiver, Outcome::Eos);
    assert_all(result.sent_audio, result.received_audio, "audio");
    assert_all(result.sent_video, result.received_video, "video");
}
