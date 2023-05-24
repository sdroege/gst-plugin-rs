// SPDX-License-Identifier: MPL-2.0

use anyhow::Context;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_rtp::prelude::*;
use gst_utils::StreamProducer;
use gst_video::subclass::prelude::*;
use gst_webrtc::{WebRTCDataChannel, WebRTCICETransportPolicy};

use futures::prelude::*;

use anyhow::{anyhow, Error};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::ops::Mul;
use std::sync::Mutex;

use super::homegrown_cc::CongestionController;
use super::{WebRTCSinkCongestionControl, WebRTCSinkError, WebRTCSinkMitigationMode};
use crate::signaller::Signaller;
use crate::RUNTIME;
use std::collections::BTreeMap;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtcsink",
        gst::DebugColorFlags::empty(),
        Some("WebRTC sink"),
    )
});

const CUDA_MEMORY_FEATURE: &str = "memory:CUDAMemory";
const GL_MEMORY_FEATURE: &str = "memory:GLMemory";
const NVMM_MEMORY_FEATURE: &str = "memory:NVMM";

const RTP_TWCC_URI: &str =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";

const DEFAULT_STUN_SERVER: Option<&str> = Some("stun://stun.l.google.com:19302");
const DEFAULT_MIN_BITRATE: u32 = 1000;

/* I have found higher values to cause packet loss *somewhere* in
 * my local network, possibly related to chrome's pretty low UDP
 * buffer sizes */
const DEFAULT_MAX_BITRATE: u32 = 8192000;
const DEFAULT_CONGESTION_CONTROL: WebRTCSinkCongestionControl =
    WebRTCSinkCongestionControl::GoogleCongestionControl;
const DEFAULT_DO_FEC: bool = true;
const DEFAULT_DO_RETRANSMISSION: bool = true;
const DEFAULT_ENABLE_DATA_CHANNEL_NAVIGATION: bool = false;
const DEFAULT_ICE_TRANSPORT_POLICY: WebRTCICETransportPolicy = WebRTCICETransportPolicy::All;
const DEFAULT_START_BITRATE: u32 = 2048000;
/* Start adding some FEC when the bitrate > 2Mbps as we found experimentally
 * that it is not worth it below that threshold */
const DO_FEC_THRESHOLD: u32 = 2000000;

#[derive(Debug, Clone, Copy)]
struct CCInfo {
    heuristic: WebRTCSinkCongestionControl,
    min_bitrate: u32,
    max_bitrate: u32,
    start_bitrate: u32,
}

/// User configuration
struct Settings {
    video_caps: gst::Caps,
    audio_caps: gst::Caps,
    turn_servers: gst::Array,
    stun_server: Option<String>,
    cc_info: CCInfo,
    do_fec: bool,
    do_retransmission: bool,
    enable_data_channel_navigation: bool,
    meta: Option<gst::Structure>,
    ice_transport_policy: WebRTCICETransportPolicy,
}

/// Represents a codec we can offer
#[derive(Debug, Clone)]
struct Codec {
    encoder: gst::ElementFactory,
    payloader: gst::ElementFactory,
    caps: gst::Caps,
    payload: i32,
}

impl Codec {
    fn is_video(&self) -> bool {
        self.encoder
            .has_type(gst::ElementFactoryType::VIDEO_ENCODER)
    }
}

/// Wrapper around our sink pads
#[derive(Debug, Clone)]
struct InputStream {
    sink_pad: gst::GhostPad,
    producer: Option<StreamProducer>,
    /// The (fixed) caps coming in
    in_caps: Option<gst::Caps>,
    /// The caps we will offer, as a set of fixed structures
    out_caps: Option<gst::Caps>,
    /// Pace input data
    clocksync: Option<gst::Element>,
    /// The serial number picked for this stream
    serial: u32,
}

/// Wrapper around webrtcbin pads
#[derive(Clone)]
struct WebRTCPad {
    pad: gst::Pad,
    /// The (fixed) caps of the corresponding input stream
    in_caps: gst::Caps,
    /// The m= line index in the SDP
    media_idx: u32,
    ssrc: u32,
    /// The name of the corresponding InputStream's sink_pad
    stream_name: String,
    /// The payload selected in the answer, None at first
    payload: Option<i32>,
}

/// Wrapper around GStreamer encoder element, keeps track of factory
/// name in order to provide a unified set / get bitrate API, also
/// tracks a raw capsfilter used to resize / decimate the input video
/// stream according to the bitrate, thresholds hardcoded for now
pub struct VideoEncoder {
    factory_name: String,
    codec_name: String,
    element: gst::Element,
    filter: gst::Element,
    halved_framerate: gst::Fraction,
    video_info: gst_video::VideoInfo,
    session_id: String,
    mitigation_mode: WebRTCSinkMitigationMode,
    pub transceiver: gst_webrtc::WebRTCRTPTransceiver,
}

struct Session {
    id: String,

    pipeline: gst::Pipeline,
    webrtcbin: gst::Element,
    rtprtxsend: Option<gst::Element>,
    webrtc_pads: HashMap<u32, WebRTCPad>,
    peer_id: String,
    encoders: Vec<VideoEncoder>,

    // Our Homegrown controller (if cc_info.heuristic == Homegrown)
    congestion_controller: Option<CongestionController>,
    // Our BandwidthEstimator (if cc_info.heuristic == GoogleCongestionControl)
    rtpgccbwe: Option<gst::Element>,

    sdp: Option<gst_sdp::SDPMessage>,
    stats: gst::Structure,

    cc_info: CCInfo,

    links: HashMap<u32, gst_utils::ConsumptionLink>,
    stats_sigid: Option<glib::SignalHandlerId>,
}

#[derive(PartialEq)]
enum SignallerState {
    Started,
    Stopped,
}

#[derive(Debug, serde::Deserialize)]
struct NavigationEvent {
    mid: Option<String>,
    #[serde(flatten)]
    event: gst_video::NavigationEvent,
}

/* Our internal state */
struct State {
    signaller: Box<dyn super::SignallableObject>,
    signaller_state: SignallerState,
    sessions: HashMap<String, Session>,
    codecs: BTreeMap<i32, Codec>,
    /// Used to abort codec discovery
    codecs_abort_handle: Option<futures::future::AbortHandle>,
    /// Used to wait for the discovery task to fully stop
    codecs_done_receiver: Option<futures::channel::oneshot::Receiver<()>>,
    /// Used to determine whether we can start the signaller when going to Playing,
    /// or whether we should wait
    codec_discovery_done: bool,
    audio_serial: u32,
    video_serial: u32,
    streams: HashMap<String, InputStream>,
    navigation_handler: Option<NavigationEventHandler>,
    mids: HashMap<String, String>,
    stats_collection_handle: Option<tokio::task::JoinHandle<()>>,
}

fn create_navigation_event(sink: &super::WebRTCSink, msg: &str) {
    let event: Result<NavigationEvent, _> = serde_json::from_str(msg);

    if let Ok(event) = event {
        gst::log!(CAT, obj: sink, "Processing navigation event: {:?}", event);

        if let Some(mid) = event.mid {
            let this = sink.imp();

            let state = this.state.lock().unwrap();
            if let Some(stream_name) = state.mids.get(&mid) {
                if let Some(stream) = state.streams.get(stream_name) {
                    let event = gst::event::Navigation::new(event.event.structure());

                    if !stream.sink_pad.push_event(event.clone()) {
                        gst::info!(CAT, "Could not send event: {:?}", event);
                    }
                }
            }
        } else {
            let this = sink.imp();

            let state = this.state.lock().unwrap();
            let event = gst::event::Navigation::new(event.event.structure());
            state.streams.iter().for_each(|(_, stream)| {
                if stream.sink_pad.name().starts_with("video_") {
                    gst::log!(CAT, "Navigating to: {:?}", event);
                    if !stream.sink_pad.push_event(event.clone()) {
                        gst::info!(CAT, "Could not send event: {:?}", event);
                    }
                }
            });
        }
    } else {
        gst::error!(CAT, "Invalid navigation event: {:?}", msg);
    }
}

/// Wrapper around `gst::ElementFactory::make` with a better error
/// message
pub fn make_element(element: &str, name: Option<&str>) -> Result<gst::Element, Error> {
    let mut builder = gst::ElementFactory::make(element);
    if let Some(name) = name {
        builder = builder.name(name);
    }

    builder
        .build()
        .with_context(|| format!("Failed to make element {element}"))
}

/// Simple utility for tearing down a pipeline cleanly
struct PipelineWrapper(gst::Pipeline);

// Structure to generate GstNavigation event from a WebRTCDataChannel
// This is simply used to hold references to the inner items.
#[derive(Debug)]
struct NavigationEventHandler((glib::SignalHandlerId, WebRTCDataChannel));

/// Our instance structure
#[derive(Default)]
pub struct WebRTCSink {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            video_caps: ["video/x-vp8", "video/x-h264", "video/x-vp9", "video/x-h265"]
                .into_iter()
                .map(gst::Structure::new_empty)
                .collect::<gst::Caps>(),
            audio_caps: ["audio/x-opus"]
                .into_iter()
                .map(gst::Structure::new_empty)
                .collect::<gst::Caps>(),
            stun_server: DEFAULT_STUN_SERVER.map(String::from),
            turn_servers: gst::Array::new(Vec::new() as Vec<glib::SendValue>),
            cc_info: CCInfo {
                heuristic: WebRTCSinkCongestionControl::GoogleCongestionControl,
                min_bitrate: DEFAULT_MIN_BITRATE,
                max_bitrate: DEFAULT_MAX_BITRATE,
                start_bitrate: DEFAULT_START_BITRATE,
            },
            do_fec: DEFAULT_DO_FEC,
            do_retransmission: DEFAULT_DO_RETRANSMISSION,
            enable_data_channel_navigation: DEFAULT_ENABLE_DATA_CHANNEL_NAVIGATION,
            meta: None,
            ice_transport_policy: DEFAULT_ICE_TRANSPORT_POLICY,
        }
    }
}

impl Default for State {
    fn default() -> Self {
        let signaller = Signaller::default();

        Self {
            signaller: Box::new(signaller),
            signaller_state: SignallerState::Stopped,
            sessions: HashMap::new(),
            codecs: BTreeMap::new(),
            codecs_abort_handle: None,
            codecs_done_receiver: None,
            codec_discovery_done: false,
            audio_serial: 0,
            video_serial: 0,
            streams: HashMap::new(),
            navigation_handler: None,
            mids: HashMap::new(),
            stats_collection_handle: None,
        }
    }
}

fn make_converter_for_video_caps(caps: &gst::Caps) -> Result<gst::Element, Error> {
    assert!(caps.is_fixed());

    let video_info = gst_video::VideoInfo::from_caps(caps)?;

    let ret = gst::Bin::default();

    let (head, mut tail) = {
        if let Some(feature) = caps.features(0) {
            if feature.contains(CUDA_MEMORY_FEATURE) {
                let cudaupload = make_element("cudaupload", None)?;
                let cudaconvert = make_element("cudaconvert", None)?;
                let cudascale = make_element("cudascale", None)?;

                ret.add_many(&[&cudaupload, &cudaconvert, &cudascale])?;
                gst::Element::link_many(&[&cudaupload, &cudaconvert, &cudascale])?;

                (cudaupload, cudascale)
            } else if feature.contains(GL_MEMORY_FEATURE) {
                let glupload = make_element("glupload", None)?;
                let glconvert = make_element("glcolorconvert", None)?;
                let glscale = make_element("glcolorscale", None)?;

                ret.add_many(&[&glupload, &glconvert, &glscale])?;
                gst::Element::link_many(&[&glupload, &glconvert, &glscale])?;

                (glupload, glscale)
            } else if feature.contains(NVMM_MEMORY_FEATURE) {
                let queue = make_element("queue", None)?;
                let nvconvert = if let Ok(nvconvert) = make_element("nvvideoconvert", None) {
                    nvconvert.set_property("compute-hw", 0);
                    nvconvert.set_property("nvbuf-memory-type", 0);
                    nvconvert
                } else {
                    make_element("nvvidconv", None)?
                };

                ret.add_many(&[&queue, &nvconvert])?;
                gst::Element::link_many(&[&queue, &nvconvert])?;

                (queue, nvconvert)
            } else {
                let convert = make_element("videoconvert", None)?;
                let scale = make_element("videoscale", None)?;

                ret.add_many(&[&convert, &scale])?;
                gst::Element::link_many(&[&convert, &scale])?;

                (convert, scale)
            }
        } else {
            let convert = make_element("videoconvert", None)?;
            let scale = make_element("videoscale", None)?;

            ret.add_many(&[&convert, &scale])?;
            gst::Element::link_many(&[&convert, &scale])?;

            (convert, scale)
        }
    };

    ret.add_pad(
        &gst::GhostPad::with_target(Some("sink"), &head.static_pad("sink").unwrap()).unwrap(),
    )
    .unwrap();

    if video_info.fps().numer() != 0 {
        let vrate = make_element("videorate", None)?;
        vrate.set_property("drop-only", true);
        vrate.set_property("skip-to-first", true);

        ret.add(&vrate)?;
        tail.link(&vrate)?;
        tail = vrate;
    }

    ret.add_pad(
        &gst::GhostPad::with_target(Some("src"), &tail.static_pad("src").unwrap()).unwrap(),
    )
    .unwrap();

    Ok(ret.upcast())
}

/// Default configuration for known encoders, can be disabled
/// by returning True from an encoder-setup handler.
fn configure_encoder(enc: &gst::Element, start_bitrate: u32) {
    if let Some(factory) = enc.factory() {
        match factory.name().as_str() {
            "vp8enc" | "vp9enc" => {
                enc.set_property("deadline", 1i64);
                enc.set_property("target-bitrate", start_bitrate as i32);
                enc.set_property("cpu-used", -16i32);
                enc.set_property("keyframe-max-dist", 2000i32);
                enc.set_property_from_str("keyframe-mode", "disabled");
                enc.set_property_from_str("end-usage", "cbr");
                enc.set_property("buffer-initial-size", 100i32);
                enc.set_property("buffer-optimal-size", 120i32);
                enc.set_property("buffer-size", 150i32);
                enc.set_property("max-intra-bitrate", 250i32);
                enc.set_property_from_str("error-resilient", "default");
                enc.set_property("lag-in-frames", 0i32);
            }
            "x264enc" => {
                enc.set_property("bitrate", start_bitrate / 1000);
                enc.set_property_from_str("tune", "zerolatency");
                enc.set_property_from_str("speed-preset", "ultrafast");
                enc.set_property("threads", 12u32);
                enc.set_property("key-int-max", 2560u32);
                enc.set_property("b-adapt", false);
                enc.set_property("vbv-buf-capacity", 120u32);
            }
            "nvh264enc" => {
                enc.set_property("bitrate", start_bitrate / 1000);
                enc.set_property("gop-size", 2560i32);
                enc.set_property_from_str("rc-mode", "cbr-ld-hq");
                enc.set_property("zerolatency", true);
            }
            "vaapih264enc" | "vaapivp8enc" => {
                enc.set_property("bitrate", start_bitrate / 1000);
                enc.set_property("keyframe-period", 2560u32);
                enc.set_property_from_str("rate-control", "cbr");
            }
            "nvv4l2h264enc" => {
                enc.set_property("bitrate", start_bitrate);
                enc.set_property_from_str("preset-level", "UltraFastPreset");
                enc.set_property("maxperf-enable", true);
                enc.set_property("insert-vui", true);
                enc.set_property("idrinterval", 256u32);
                enc.set_property("insert-sps-pps", true);
                enc.set_property("insert-aud", true);
                enc.set_property_from_str("control-rate", "constant_bitrate");
            }
            "nvv4l2vp8enc" | "nvv4l2vp9enc" => {
                enc.set_property("bitrate", start_bitrate);
                enc.set_property_from_str("preset-level", "UltraFastPreset");
                enc.set_property("maxperf-enable", true);
                enc.set_property("idrinterval", 256u32);
                enc.set_property_from_str("control-rate", "constant_bitrate");
            }
            _ => (),
        }
    }
}

/// Bit of an awkward function, but the goal here is to keep
/// most of the encoding code for consumers in line with
/// the codec discovery code, and this gets the job done.
fn setup_encoding(
    pipeline: &gst::Pipeline,
    src: &gst::Element,
    input_caps: &gst::Caps,
    codec: &Codec,
    mut encoded_filter: Option<gst::Element>,
    ssrc: Option<u32>,
    twcc: bool,
) -> Result<(gst::Element, gst::Element, gst::Element), Error> {
    let conv = match codec.is_video() {
        true => make_converter_for_video_caps(input_caps)?.upcast(),
        false => gst::parse_bin_from_description("audioresample ! audioconvert", true)?.upcast(),
    };

    let conv_filter = make_element("capsfilter", None)?;

    let enc = codec
        .encoder
        .create()
        .build()
        .with_context(|| format!("Creating encoder {}", codec.encoder.name()))?;
    let pay = codec
        .payloader
        .create()
        .build()
        .with_context(|| format!("Creating payloader {}", codec.payloader.name()))?;
    let parse_filter = make_element("capsfilter", None)?;

    pay.set_property("mtu", 1200_u32);
    pay.set_property("pt", codec.payload as u32);

    if let Some(ssrc) = ssrc {
        pay.set_property("ssrc", ssrc);
    }

    pipeline
        .add_many(&[&conv, &conv_filter, &enc, &parse_filter, &pay])
        .unwrap();
    gst::Element::link_many(&[src, &conv, &conv_filter, &enc])
        .with_context(|| "Linking encoding elements")?;

    let codec_name = codec.caps.structure(0).unwrap().name();

    let enc_last = if let Some(encoded_filter) = encoded_filter.take() {
        pipeline.add(&encoded_filter).unwrap();
        enc.link(&encoded_filter)
            .with_context(|| "Linking encoded filter")?;

        encoded_filter
    } else {
        enc.clone()
    };

    if let Some(parser) = if codec_name == "video/x-h264" {
        Some(make_element("h264parse", None)?)
    } else if codec_name == "video/x-h265" {
        Some(make_element("h265parse", None)?)
    } else {
        None
    } {
        pipeline.add(&parser).unwrap();
        gst::Element::link_many(&[&enc_last, &parser, &parse_filter])
            .with_context(|| "Linking encoding elements")?;
    } else {
        gst::Element::link_many(&[&enc_last, &parse_filter])
            .with_context(|| "Linking encoding elements")?;
    }

    let conv_caps = if codec.is_video() {
        let mut structure_builder = gst::Structure::builder("video/x-raw")
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1));

        if codec.encoder.name() == "nvh264enc" {
            // Quirk: nvh264enc can perform conversion from RGB formats, but
            // doesn't advertise / negotiate colorimetry correctly, leading
            // to incorrect color display in Chrome (but interestingly not in
            // Firefox). In any case, restrict to exclude RGB formats altogether,
            // and let videoconvert do the conversion properly if needed.
            structure_builder =
                structure_builder.field("format", gst::List::new(["NV12", "YV12", "I420"]));
        }

        gst::Caps::builder_full_with_any_features()
            .structure(structure_builder.build())
            .build()
    } else {
        gst::Caps::builder("audio/x-raw").build()
    };

    match codec.encoder.name().as_str() {
        "vp8enc" | "vp9enc" => {
            pay.set_property_from_str("picture-id-mode", "15-bit");
        }
        _ => (),
    }

    /* We only enforce TWCC in the offer caps, once a remote description
     * has been set it will get automatically negotiated. This is necessary
     * because the implementor in Firefox had apparently not understood the
     * concept of *transport-wide* congestion control, and firefox doesn't
     * provide feedback for audio packets.
     */
    if twcc {
        let twcc_extension = gst_rtp::RTPHeaderExtension::create_from_uri(RTP_TWCC_URI).unwrap();
        twcc_extension.set_id(1);
        pay.emit_by_name::<()>("add-extension", &[&twcc_extension]);
    }

    conv_filter.set_property("caps", conv_caps);

    let parse_caps = if codec_name == "video/x-h264" {
        gst::Caps::builder(codec_name)
            .field("stream-format", "avc")
            .field("profile", "constrained-baseline")
            .build()
    } else if codec_name == "video/x-h265" {
        gst::Caps::builder(codec_name)
            .field("stream-format", "hvc1")
            .build()
    } else {
        gst::Caps::new_any()
    };

    parse_filter.set_property("caps", parse_caps);

    gst::Element::link_many(&[&parse_filter, &pay]).with_context(|| "Linking encoding elements")?;

    Ok((enc, conv_filter, pay))
}

impl VideoEncoder {
    fn new(
        element: gst::Element,
        filter: gst::Element,
        video_info: gst_video::VideoInfo,
        peer_id: &str,
        codec_name: &str,
        transceiver: gst_webrtc::WebRTCRTPTransceiver,
    ) -> Self {
        let halved_framerate = video_info.fps().mul(gst::Fraction::new(1, 2));

        Self {
            factory_name: element.factory().unwrap().name().into(),
            codec_name: codec_name.to_string(),
            element,
            filter,
            halved_framerate,
            video_info,
            session_id: peer_id.to_string(),
            mitigation_mode: WebRTCSinkMitigationMode::NONE,
            transceiver,
        }
    }

    pub fn bitrate(&self) -> i32 {
        match self.factory_name.as_str() {
            "vp8enc" | "vp9enc" => self.element.property::<i32>("target-bitrate"),
            "x264enc" | "nvh264enc" | "vaapih264enc" | "vaapivp8enc" => {
                (self.element.property::<u32>("bitrate") * 1000) as i32
            }
            "nvv4l2h264enc" | "nvv4l2vp8enc" | "nvv4l2vp9enc" => {
                (self.element.property::<u32>("bitrate")) as i32
            }
            factory => unimplemented!("Factory {} is currently not supported", factory),
        }
    }

    pub fn scale_height_round_2(&self, height: i32) -> i32 {
        let ratio = gst_video::calculate_display_ratio(
            self.video_info.width(),
            self.video_info.height(),
            self.video_info.par(),
            gst::Fraction::new(1, 1),
        )
        .unwrap();

        let width = height.mul_div_ceil(ratio.numer(), ratio.denom()).unwrap();

        (width + 1) & !1
    }

    pub fn set_bitrate(&mut self, element: &super::WebRTCSink, bitrate: i32) {
        match self.factory_name.as_str() {
            "vp8enc" | "vp9enc" => self.element.set_property("target-bitrate", bitrate),
            "x264enc" | "nvh264enc" | "vaapih264enc" | "vaapivp8enc" => self
                .element
                .set_property("bitrate", (bitrate / 1000) as u32),
            "nvv4l2h264enc" | "nvv4l2vp8enc" | "nvv4l2vp9enc" => {
                self.element.set_property("bitrate", bitrate as u32)
            }
            factory => unimplemented!("Factory {} is currently not supported", factory),
        }

        let current_caps = self.filter.property::<gst::Caps>("caps");
        let mut s = current_caps.structure(0).unwrap().to_owned();

        // Hardcoded thresholds, may be tuned further in the future, and
        // adapted according to the codec in use
        if bitrate < 500000 {
            let height = 360i32.min(self.video_info.height() as i32);
            let width = self.scale_height_round_2(height);

            s.set("height", height);
            s.set("width", width);

            if self.halved_framerate.numer() != 0 {
                s.set("framerate", self.halved_framerate);
            }

            self.mitigation_mode =
                WebRTCSinkMitigationMode::DOWNSAMPLED | WebRTCSinkMitigationMode::DOWNSCALED;
        } else if bitrate < 1000000 {
            let height = 360i32.min(self.video_info.height() as i32);
            let width = self.scale_height_round_2(height);

            s.set("height", height);
            s.set("width", width);
            s.remove_field("framerate");

            self.mitigation_mode = WebRTCSinkMitigationMode::DOWNSCALED;
        } else if bitrate < 2000000 {
            let height = 720i32.min(self.video_info.height() as i32);
            let width = self.scale_height_round_2(height);

            s.set("height", height);
            s.set("width", width);
            s.remove_field("framerate");

            self.mitigation_mode = WebRTCSinkMitigationMode::DOWNSCALED;
        } else {
            s.remove_field("height");
            s.remove_field("width");
            s.remove_field("framerate");

            self.mitigation_mode = WebRTCSinkMitigationMode::NONE;
        }

        let caps = gst::Caps::builder_full_with_any_features()
            .structure(s)
            .build();

        if !caps.is_strictly_equal(&current_caps) {
            gst::log!(
                CAT,
                obj: element,
                "session {}: setting bitrate {} and caps {} on encoder {:?}",
                self.session_id,
                bitrate,
                caps,
                self.element
            );

            self.filter.set_property("caps", caps);
        }
    }

    fn gather_stats(&self) -> gst::Structure {
        gst::Structure::builder("application/x-webrtcsink-video-encoder-stats")
            .field("bitrate", self.bitrate())
            .field("mitigation-mode", self.mitigation_mode)
            .field("codec-name", self.codec_name.as_str())
            .field(
                "fec-percentage",
                self.transceiver.property::<u32>("fec-percentage"),
            )
            .build()
    }
}

impl State {
    fn finalize_session(
        &mut self,
        element: &super::WebRTCSink,
        session: &mut Session,
        signal: bool,
    ) {
        gst::info!(CAT, "Ending session {}", session.id);
        session.pipeline.debug_to_dot_file_with_ts(
            gst::DebugGraphDetails::all(),
            format!("removing-peer-{}-", session.peer_id,),
        );

        for ssrc in session.webrtc_pads.keys() {
            session.links.remove(ssrc);
        }

        session.pipeline.call_async(|pipeline| {
            let _ = pipeline.set_state(gst::State::Null);
        });

        if signal {
            self.signaller.session_ended(element, &session.peer_id);
        }
    }

    fn end_session(
        &mut self,
        element: &super::WebRTCSink,
        session_id: &str,
        signal: bool,
    ) -> Option<Session> {
        if let Some(mut session) = self.sessions.remove(session_id) {
            self.finalize_session(element, &mut session, signal);
            Some(session)
        } else {
            None
        }
    }

    fn maybe_start_signaller(&mut self, element: &super::WebRTCSink) {
        if self.signaller_state == SignallerState::Stopped
            && element.current_state() >= gst::State::Paused
            && self.codec_discovery_done
        {
            if let Err(err) = self.signaller.start(element) {
                gst::error!(CAT, obj: element, "error: {}", err);
                gst::element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Failed to start signaller {}", err]
                );
            } else {
                gst::info!(CAT, "Started signaller");
                self.signaller_state = SignallerState::Started;
            }
        }
    }

    fn maybe_stop_signaller(&mut self, element: &super::WebRTCSink) {
        if self.signaller_state == SignallerState::Started {
            self.signaller.stop(element);
            self.signaller_state = SignallerState::Stopped;
            gst::info!(CAT, "Stopped signaller");
        }
    }
}

impl Session {
    fn new(
        id: String,
        pipeline: gst::Pipeline,
        webrtcbin: gst::Element,
        peer_id: String,
        congestion_controller: Option<CongestionController>,
        rtpgccbwe: Option<gst::Element>,
        cc_info: CCInfo,
    ) -> Self {
        Self {
            id,
            pipeline,
            webrtcbin,
            peer_id,
            cc_info,
            rtprtxsend: None,
            congestion_controller,
            rtpgccbwe,
            stats: gst::Structure::new_empty("application/x-webrtc-stats"),
            sdp: None,
            webrtc_pads: HashMap::new(),
            encoders: Vec::new(),
            links: HashMap::new(),
            stats_sigid: None,
        }
    }

    fn gather_stats(&self) -> gst::Structure {
        let mut ret = self.stats.to_owned();

        let encoder_stats = self
            .encoders
            .iter()
            .map(VideoEncoder::gather_stats)
            .map(|s| s.to_send_value())
            .collect::<gst::Array>();

        let our_stats = gst::Structure::builder("application/x-webrtcsink-consumer-stats")
            .field("video-encoders", encoder_stats)
            .build();

        ret.set("consumer-stats", our_stats);

        ret
    }

    fn generate_ssrc(&self) -> u32 {
        loop {
            let ret = fastrand::u32(..);

            if !self.webrtc_pads.contains_key(&ret) {
                return ret;
            }
        }
    }

    /// Request a sink pad on our webrtcbin, and set its transceiver's codec_preferences
    fn request_webrtcbin_pad(
        &mut self,
        element: &super::WebRTCSink,
        settings: &Settings,
        stream: &InputStream,
    ) {
        let ssrc = self.generate_ssrc();
        let media_idx = self.webrtc_pads.len() as i32;

        let mut payloader_caps = stream.out_caps.as_ref().unwrap().to_owned();

        {
            let payloader_caps_mut = payloader_caps.make_mut();
            payloader_caps_mut.set("ssrc", ssrc);
        }

        gst::info!(
            CAT,
            obj: element,
            "Requesting WebRTC pad for consumer {} with caps {}",
            self.peer_id,
            payloader_caps
        );

        let pad = self
            .webrtcbin
            .request_pad_simple(&format!("sink_{media_idx}"))
            .unwrap();

        let transceiver = pad.property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");

        transceiver.set_property(
            "direction",
            gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
        );

        transceiver.set_property("codec-preferences", &payloader_caps);

        if stream.sink_pad.name().starts_with("video_") {
            if settings.do_fec {
                transceiver.set_property("fec-type", gst_webrtc::WebRTCFECType::UlpRed);
            }

            transceiver.set_property("do-nack", settings.do_retransmission);
        }

        self.webrtc_pads.insert(
            ssrc,
            WebRTCPad {
                pad,
                in_caps: stream.in_caps.as_ref().unwrap().clone(),
                media_idx: media_idx as u32,
                ssrc,
                stream_name: stream.sink_pad.name().to_string(),
                payload: None,
            },
        );
    }

    /// Called when we have received an answer, connects an InputStream
    /// to a given WebRTCPad
    fn connect_input_stream(
        &mut self,
        element: &super::WebRTCSink,
        producer: &StreamProducer,
        webrtc_pad: &WebRTCPad,
        codecs: &BTreeMap<i32, Codec>,
    ) -> Result<(), Error> {
        gst::info!(
            CAT,
            obj: element,
            "Connecting input stream {} for consumer {}",
            webrtc_pad.stream_name,
            self.peer_id
        );

        let payload = webrtc_pad.payload.unwrap();

        let codec = codecs
            .get(&payload)
            .ok_or_else(|| anyhow!("No codec for payload {}", payload))?;

        let appsrc = make_element("appsrc", Some(&webrtc_pad.stream_name))?;
        self.pipeline.add(&appsrc).unwrap();

        let pay_filter = make_element("capsfilter", None)?;
        self.pipeline.add(&pay_filter).unwrap();

        let encoded_filter = element.emit_by_name::<Option<gst::Element>>(
            "request-encoded-filter",
            &[&Some(&self.peer_id), &webrtc_pad.stream_name, &codec.caps],
        );

        let (enc, raw_filter, pay) = setup_encoding(
            &self.pipeline,
            &appsrc,
            &webrtc_pad.in_caps,
            codec,
            encoded_filter,
            Some(webrtc_pad.ssrc),
            false,
        )?;

        element.emit_by_name::<bool>(
            "encoder-setup",
            &[&self.peer_id, &webrtc_pad.stream_name, &enc],
        );

        // At this point, the peer has provided its answer, and we want to
        // let the payloader / encoder perform negotiation according to that.
        //
        // This means we need to unset our codec preferences, as they would now
        // conflict with what the peer actually requested (see webrtcbin's
        // caps query implementation), and instead install a capsfilter downstream
        // of the payloader with caps constructed from the relevant SDP media.
        let transceiver = webrtc_pad
            .pad
            .property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");
        transceiver.set_property("codec-preferences", None::<gst::Caps>);

        let mut global_caps = gst::Caps::new_empty_simple("application/x-unknown");

        let sdp = self.sdp.as_ref().unwrap();
        let sdp_media = sdp.media(webrtc_pad.media_idx).unwrap();

        sdp.attributes_to_caps(global_caps.get_mut().unwrap())
            .unwrap();
        sdp_media
            .attributes_to_caps(global_caps.get_mut().unwrap())
            .unwrap();

        let caps = sdp_media
            .caps_from_media(payload)
            .unwrap()
            .intersect(&global_caps);
        let s = caps.structure(0).unwrap();
        let mut filtered_s = gst::Structure::new_empty("application/x-rtp");

        filtered_s.extend(s.iter().filter_map(|(key, value)| {
            if key.starts_with("a-") {
                None
            } else {
                Some((key, value.to_owned()))
            }
        }));
        filtered_s.set("ssrc", webrtc_pad.ssrc);

        let caps = gst::Caps::builder_full().structure(filtered_s).build();

        pay_filter.set_property("caps", caps);

        if codec.is_video() {
            let video_info = gst_video::VideoInfo::from_caps(&webrtc_pad.in_caps)?;
            let mut enc = VideoEncoder::new(
                enc,
                raw_filter,
                video_info,
                &self.peer_id,
                codec.caps.structure(0).unwrap().name(),
                transceiver,
            );

            match self.cc_info.heuristic {
                WebRTCSinkCongestionControl::Disabled => {
                    // If congestion control is disabled, we simply use the highest
                    // known "safe" value for the bitrate.
                    enc.set_bitrate(element, self.cc_info.max_bitrate as i32);
                    enc.transceiver.set_property("fec-percentage", 50u32);
                }
                WebRTCSinkCongestionControl::Homegrown => {
                    if let Some(congestion_controller) = self.congestion_controller.as_mut() {
                        congestion_controller.target_bitrate_on_delay += enc.bitrate();
                        congestion_controller.target_bitrate_on_loss =
                            congestion_controller.target_bitrate_on_delay;
                        enc.transceiver.set_property("fec-percentage", 0u32);
                    } else {
                        /* If congestion control is disabled, we simply use the highest
                         * known "safe" value for the bitrate. */
                        enc.set_bitrate(element, self.cc_info.max_bitrate as i32);
                        enc.transceiver.set_property("fec-percentage", 50u32);
                    }
                }
                _ => enc.transceiver.set_property("fec-percentage", 0u32),
            }

            self.encoders.push(enc);

            if let Some(rtpgccbwe) = self.rtpgccbwe.as_ref() {
                let max_bitrate = self.cc_info.max_bitrate * (self.encoders.len() as u32);
                rtpgccbwe.set_property("max-bitrate", max_bitrate);
            }
        }

        let appsrc = appsrc.downcast::<gst_app::AppSrc>().unwrap();
        gst_utils::StreamProducer::configure_consumer(&appsrc);
        self.pipeline
            .sync_children_states()
            .with_context(|| format!("Connecting input stream for {}", self.peer_id))?;

        pay.link(&pay_filter)?;

        let srcpad = pay_filter.static_pad("src").unwrap();

        srcpad
            .link(&webrtc_pad.pad)
            .with_context(|| format!("Connecting input stream for {}", self.peer_id))?;

        match producer.add_consumer(&appsrc) {
            Ok(link) => {
                self.links.insert(webrtc_pad.ssrc, link);
                Ok(())
            }
            Err(err) => Err(anyhow!("Could not link producer: {:?}", err)),
        }
    }
}

impl Drop for PipelineWrapper {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

impl InputStream {
    /// Called when transitioning state up to Paused
    fn prepare(&mut self, element: &super::WebRTCSink) -> Result<(), Error> {
        let clocksync = make_element("clocksync", None)?;
        let appsink = make_element("appsink", None)?
            .downcast::<gst_app::AppSink>()
            .unwrap();

        element.add(&clocksync).unwrap();
        element.add(&appsink).unwrap();

        clocksync
            .link(&appsink)
            .with_context(|| format!("Linking input stream {}", self.sink_pad.name()))?;

        element
            .sync_children_states()
            .with_context(|| format!("Linking input stream {}", self.sink_pad.name()))?;

        self.sink_pad
            .set_target(Some(&clocksync.static_pad("sink").unwrap()))
            .unwrap();

        self.producer = Some(StreamProducer::from(&appsink));

        Ok(())
    }

    /// Called when transitioning state back down to Ready
    fn unprepare(&mut self, element: &super::WebRTCSink) {
        self.sink_pad.set_target(None::<&gst::Pad>).unwrap();

        if let Some(clocksync) = self.clocksync.take() {
            element.remove(&clocksync).unwrap();
            clocksync.set_state(gst::State::Null).unwrap();
        }

        if let Some(producer) = self.producer.take() {
            let appsink = producer.appsink().upcast_ref::<gst::Element>();
            element.remove(appsink).unwrap();
            appsink.set_state(gst::State::Null).unwrap();
        }
    }
}

impl NavigationEventHandler {
    pub fn new(element: &super::WebRTCSink, webrtcbin: &gst::Element) -> Self {
        gst::info!(CAT, "Creating navigation data channel");
        let channel = webrtcbin.emit_by_name::<WebRTCDataChannel>(
            "create-data-channel",
            &[
                &"input",
                &gst::Structure::builder("config")
                    .field("priority", gst_webrtc::WebRTCPriorityType::High)
                    .build(),
            ],
        );

        let weak_element = element.downgrade();
        Self((
            channel.connect("on-message-string", false, move |values| {
                if let Some(element) = weak_element.upgrade() {
                    let _channel = values[0].get::<WebRTCDataChannel>().unwrap();
                    let msg = values[1].get::<&str>().unwrap();
                    create_navigation_event(&element, msg);
                }

                None
            }),
            channel,
        ))
    }
}

impl WebRTCSink {
    /// Build an ordered map of Codecs, given user-provided audio / video caps */
    fn lookup_codecs(&self) -> BTreeMap<i32, Codec> {
        /* First gather all encoder and payloader factories */
        let encoders = gst::ElementFactory::factories_with_type(
            gst::ElementFactoryType::ENCODER,
            gst::Rank::Marginal,
        );

        let payloaders = gst::ElementFactory::factories_with_type(
            gst::ElementFactoryType::PAYLOADER,
            gst::Rank::Marginal,
        );

        /* Now iterate user-provided codec preferences and determine
         * whether we can fulfill these preferences */
        let settings = self.settings.lock().unwrap();
        let mut payload = 96..128;

        settings
            .video_caps
            .iter()
            .chain(settings.audio_caps.iter())
            .filter_map(move |s| {
                let caps = gst::Caps::builder_full().structure(s.to_owned()).build();

                Option::zip(
                    encoders
                        .iter()
                        .find(|factory| factory.can_src_any_caps(&caps)),
                    payloaders
                        .iter()
                        .find(|factory| factory.can_sink_any_caps(&caps)),
                )
                .and_then(|(encoder, payloader)| {
                    /* Assign a payload type to the codec */
                    if let Some(pt) = payload.next() {
                        Some(Codec {
                            encoder: encoder.clone(),
                            payloader: payloader.clone(),
                            caps,
                            payload: pt,
                        })
                    } else {
                        gst::warning!(
                            CAT,
                            imp: self,
                            "Too many formats for available payload type range, ignoring {}",
                            s
                        );
                        None
                    }
                })
            })
            .map(|codec| (codec.payload, codec))
            .collect()
    }

    /// Prepare for accepting consumers, by setting
    /// up StreamProducers for each of our sink pads
    fn prepare(&self, element: &super::WebRTCSink) -> Result<(), Error> {
        gst::debug!(CAT, obj: element, "preparing");

        self.state
            .lock()
            .unwrap()
            .streams
            .iter_mut()
            .try_for_each(|(_, stream)| stream.prepare(element))?;

        Ok(())
    }

    /// Unprepare by stopping consumers, then the signaller object.
    /// Might abort codec discovery
    fn unprepare(&self, element: &super::WebRTCSink) -> Result<(), Error> {
        gst::info!(CAT, obj: element, "unpreparing");

        let mut state = self.state.lock().unwrap();

        let session_ids: Vec<_> = state.sessions.keys().map(|k| k.to_owned()).collect();

        for id in session_ids {
            state.end_session(element, &id, true);
        }

        state
            .streams
            .iter_mut()
            .for_each(|(_, stream)| stream.unprepare(element));

        if let Some(handle) = state.codecs_abort_handle.take() {
            handle.abort();
        }

        if let Some(receiver) = state.codecs_done_receiver.take() {
            RUNTIME.spawn(async {
                let _ = receiver.await;
            });
        }

        if let Some(stats_collection_handle) = state.stats_collection_handle.take() {
            stats_collection_handle.abort();
            // Make sure any currently running stats collection task completes
            drop(state);
            let _ = RUNTIME.block_on(stats_collection_handle);
            state = self.state.lock().unwrap();
        }

        state.maybe_stop_signaller(element);

        state.codec_discovery_done = false;
        state.codecs = BTreeMap::new();

        Ok(())
    }

    /// When using a custom signaller
    pub fn set_signaller(&self, signaller: Box<dyn super::SignallableObject>) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();

        state.signaller = signaller;

        Ok(())
    }

    /// Called by the signaller when it has encountered an error
    pub fn handle_signalling_error(&self, element: &super::WebRTCSink, error: anyhow::Error) {
        gst::error!(CAT, obj: element, "Signalling error: {:?}", error);

        gst::element_error!(
            element,
            gst::StreamError::Failed,
            ["Signalling error: {:?}", error]
        );
    }

    fn on_offer_created(
        &self,
        element: &super::WebRTCSink,
        offer: gst_webrtc::WebRTCSessionDescription,
        session_id: &str,
    ) {
        let mut state = self.state.lock().unwrap();

        if let Some(session) = state.sessions.get(session_id) {
            session
                .webrtcbin
                .emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);

            if let Err(err) = state.signaller.handle_sdp(element, session_id, &offer) {
                gst::warning!(
                    CAT,
                    "Failed to handle SDP for session {}: {}",
                    session_id,
                    err
                );

                state.end_session(element, session_id, true);
            }
        }
    }

    fn negotiate(&self, element: &super::WebRTCSink, session_id: &str) {
        let state = self.state.lock().unwrap();

        gst::debug!(CAT, obj: element, "Negotiating for session {}", session_id);

        if let Some(session) = state.sessions.get(session_id) {
            let element = element.downgrade();
            gst::debug!(CAT, "Creating offer for session {}", session_id);
            let session_id = session_id.to_string();
            let promise = gst::Promise::with_change_func(move |reply| {
                gst::debug!(CAT, "Created offer for session {}", session_id);

                if let Some(element) = element.upgrade() {
                    let this = element.imp();
                    let reply = match reply {
                        Ok(Some(reply)) => reply,
                        Ok(None) => {
                            gst::warning!(
                                CAT,
                                obj: element,
                                "Promise returned without a reply for {}",
                                session_id
                            );
                            let _ = this.remove_session(&element, &session_id, true);
                            return;
                        }
                        Err(err) => {
                            gst::warning!(
                                CAT,
                                obj: element,
                                "Promise returned with an error for {}: {:?}",
                                session_id,
                                err
                            );
                            let _ = this.remove_session(&element, &session_id, true);
                            return;
                        }
                    };

                    if let Ok(offer) = reply
                        .value("offer")
                        .map(|offer| offer.get::<gst_webrtc::WebRTCSessionDescription>().unwrap())
                    {
                        this.on_offer_created(&element, offer, &session_id);
                    } else {
                        gst::warning!(
                            CAT,
                            "Reply without an offer for session {}: {:?}",
                            session_id,
                            reply
                        );
                        let _ = this.remove_session(&element, &session_id, true);
                    }
                }
            });

            session
                .webrtcbin
                .emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
        } else {
            gst::debug!(
                CAT,
                obj: element,
                "consumer for session {} no longer exists (sessions: {:?}",
                session_id,
                state.sessions.keys()
            );
        }
    }

    fn on_ice_candidate(
        &self,
        element: &super::WebRTCSink,
        session_id: String,
        sdp_m_line_index: u32,
        candidate: String,
    ) {
        let mut state = self.state.lock().unwrap();
        if let Err(err) = state.signaller.handle_ice(
            element,
            &session_id,
            &candidate,
            Some(sdp_m_line_index),
            None,
        ) {
            gst::warning!(
                CAT,
                "Failed to handle ICE in session {}: {}",
                session_id,
                err
            );

            state.end_session(element, &session_id, true);
        }
    }

    /// Called by the signaller to add a new session
    pub fn start_session(
        &self,
        element: &super::WebRTCSink,
        session_id: &str,
        peer_id: &str,
    ) -> Result<(), WebRTCSinkError> {
        let settings = self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        let peer_id = peer_id.to_string();
        let session_id = session_id.to_string();

        if state.sessions.contains_key(&session_id) {
            return Err(WebRTCSinkError::DuplicateSessionId(session_id));
        }

        gst::info!(
            CAT,
            obj: element,
            "Adding session: {} for peer: {}",
            peer_id,
            session_id
        );

        let pipeline = gst::Pipeline::builder()
            .name(format!("session-pipeline-{session_id}"))
            .build();

        let webrtcbin = make_element("webrtcbin", Some(&format!("webrtcbin-{session_id}")))
            .map_err(|err| WebRTCSinkError::SessionPipelineError {
                session_id: session_id.clone(),
                peer_id: peer_id.clone(),
                details: err.to_string(),
            })?;

        webrtcbin.set_property_from_str("bundle-policy", "max-bundle");
        webrtcbin.set_property("ice-transport-policy", settings.ice_transport_policy);

        if let Some(stun_server) = settings.stun_server.as_ref() {
            webrtcbin.set_property("stun-server", stun_server);
        }

        for turn_server in settings.turn_servers.iter() {
            webrtcbin.emit_by_name::<bool>("add-turn-server", &[&turn_server]);
        }

        let rtpgccbwe = match settings.cc_info.heuristic {
            WebRTCSinkCongestionControl::GoogleCongestionControl => {
                let rtpgccbwe = match gst::ElementFactory::make("rtpgccbwe").build() {
                    Err(err) => {
                        glib::g_warning!(
                            "webrtcsink",
                            "The `rtpgccbwe` element is not available \
                            not doing any congestion control: {err:?}"
                        );
                        None
                    }
                    Ok(cc) => {
                        webrtcbin.connect_closure(
                            "request-aux-sender",
                            false,
                            glib::closure!(@watch element, @strong session_id, @weak-allow-none cc
                                    => move |_webrtcbin: gst::Element, _transport: gst::Object| {
                                if let Some(ref cc) = cc {
                                    let settings = element.imp().settings.lock().unwrap();

                                    // TODO: Bind properties with @element's
                                    cc.set_properties(&[
                                        ("min-bitrate", &settings.cc_info.min_bitrate),
                                        ("estimated-bitrate", &settings.cc_info.start_bitrate),
                                        ("max-bitrate", &settings.cc_info.max_bitrate),
                                    ]);

                                    cc.connect_notify(Some("estimated-bitrate"),
                                        glib::clone!(@weak element, @strong session_id
                                            => move |bwe, pspec| {
                                            element.imp().set_bitrate(&element, &session_id,
                                                bwe.property::<u32>(pspec.name()));
                                        }
                                    ));
                                }

                                cc
                            }),
                        );

                        Some(cc)
                    }
                };

                webrtcbin.connect_closure(
                    "deep-element-added",
                    false,
                    glib::closure!(@watch element, @strong session_id
                            => move |_webrtcbin: gst::Element, _bin: gst::Bin, e: gst::Element| {

                        if e.factory().map_or(false, |f| f.name() == "rtprtxsend") {
                            if e.has_property("stuffing-kbps", Some(i32::static_type())) {
                                element.imp().set_rtptrxsend(element, &session_id, e);
                            } else {
                                gst::warning!(CAT, "rtprtxsend doesn't have a `stuffing-kbps` \
                                    property, stuffing disabled");
                            }
                        }
                    }),
                );

                rtpgccbwe
            }
            _ => None,
        };

        pipeline.add(&webrtcbin).unwrap();

        let element_clone = element.downgrade();
        let session_id_clone = session_id.clone();
        webrtcbin.connect("on-ice-candidate", false, move |values| {
            if let Some(element) = element_clone.upgrade() {
                let this = element.imp();
                let sdp_m_line_index = values[1].get::<u32>().expect("Invalid argument");
                let candidate = values[2].get::<String>().expect("Invalid argument");
                this.on_ice_candidate(
                    &element,
                    session_id_clone.to_string(),
                    sdp_m_line_index,
                    candidate,
                );
            }
            None
        });

        let element_clone = element.downgrade();
        let peer_id_clone = peer_id.clone();
        let session_id_clone = session_id.clone();
        webrtcbin.connect_notify(Some("connection-state"), move |webrtcbin, _pspec| {
            if let Some(element) = element_clone.upgrade() {
                let state =
                    webrtcbin.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");

                match state {
                    gst_webrtc::WebRTCPeerConnectionState::Failed => {
                        let this = element.imp();
                        gst::warning!(
                            CAT,
                            obj: element,
                            "Connection state for in session {} (peer {}) failed",
                            session_id_clone,
                            peer_id_clone
                        );
                        let _ = this.remove_session(&element, &session_id_clone, true);
                    }
                    _ => {
                        gst::log!(
                            CAT,
                            obj: element,
                            "Connection state in session {} (peer {}) changed: {:?}",
                            session_id_clone,
                            peer_id_clone,
                            state
                        );
                    }
                }
            }
        });

        let element_clone = element.downgrade();
        let peer_id_clone = peer_id.clone();
        let session_id_clone = session_id.clone();
        webrtcbin.connect_notify(Some("ice-connection-state"), move |webrtcbin, _pspec| {
            if let Some(element) = element_clone.upgrade() {
                let state = webrtcbin
                    .property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
                let this = element.imp();

                match state {
                    gst_webrtc::WebRTCICEConnectionState::Failed => {
                        gst::warning!(
                            CAT,
                            obj: element,
                            "Ice connection state in session {} (peer {}) failed",
                            session_id_clone,
                            peer_id_clone,
                        );
                        let _ = this.remove_session(&element, &session_id_clone, true);
                    }
                    _ => {
                        gst::log!(
                            CAT,
                            obj: element,
                            "Ice connection state in session {} (peer {}) changed: {:?}",
                            session_id_clone,
                            peer_id_clone,
                            state
                        );
                    }
                }

                if state == gst_webrtc::WebRTCICEConnectionState::Completed {
                    let state = this.state.lock().unwrap();

                    if let Some(session) = state.sessions.get(&session_id_clone) {
                        for webrtc_pad in session.webrtc_pads.values() {
                            if let Some(srcpad) = webrtc_pad.pad.peer() {
                                srcpad.send_event(
                                    gst_video::UpstreamForceKeyUnitEvent::builder()
                                        .all_headers(true)
                                        .build(),
                                );
                            }
                        }
                    }
                }
            }
        });

        let element_clone = element.downgrade();
        let peer_id_clone = peer_id.clone();
        let session_id_clone = session_id.clone();
        webrtcbin.connect_notify(Some("ice-gathering-state"), move |webrtcbin, _pspec| {
            let state =
                webrtcbin.property::<gst_webrtc::WebRTCICEGatheringState>("ice-gathering-state");

            if let Some(element) = element_clone.upgrade() {
                gst::log!(
                    CAT,
                    obj: element,
                    "Ice gathering state in session {} (peer {}) changed: {:?}",
                    session_id_clone,
                    peer_id_clone,
                    state
                );
            }
        });

        let mut session = Session::new(
            session_id.clone(),
            pipeline.clone(),
            webrtcbin.clone(),
            peer_id.clone(),
            match settings.cc_info.heuristic {
                WebRTCSinkCongestionControl::Homegrown => Some(CongestionController::new(
                    &peer_id,
                    settings.cc_info.min_bitrate,
                    settings.cc_info.max_bitrate,
                )),
                _ => None,
            },
            rtpgccbwe,
            settings.cc_info,
        );

        let rtpbin = webrtcbin
            .dynamic_cast_ref::<gst::ChildProxy>()
            .unwrap()
            .child_by_name("rtpbin")
            .unwrap();

        if session.congestion_controller.is_some() {
            let session_id_str = session_id.to_string();
            rtpbin.connect_closure("on-new-ssrc", true,
                glib::closure!(@weak-allow-none element,
                                => move |rtpbin: gst::Object, session_id: u32, _src: u32| {
                        let rtp_session = rtpbin.emit_by_name::<gst::Element>("get-session", &[&session_id]);

                        let element = element.expect("on-new-ssrc emited when webrtcsink has been disposed?");
                        let mut state = element.imp().state.lock().unwrap();
                        if let Some(session) = state.sessions.get_mut(&session_id_str) {

                            if session.stats_sigid.is_none() {
                                let session_id_str = session_id_str.clone();
                                let element = element.downgrade();
                                session.stats_sigid = Some(rtp_session.connect_notify(Some("twcc-stats"),
                                    move |sess, pspec| {
                                        if let Some(element) = element.upgrade() {
                                            // Run the Loss-based control algorithm on new peer TWCC feedbacks
                                            element.imp().process_loss_stats(&element, &session_id_str, &sess.property::<gst::Structure>(pspec.name()));
                                        }
                                    }
                                ));
                            }
                        }
                    })
                );
        }

        let mut streams: Vec<InputStream> = state.streams.values().cloned().collect();
        streams.sort_by_key(|s| s.serial);
        streams
            .iter()
            .for_each(|stream| session.request_webrtcbin_pad(element, &settings, stream));

        let clock = element.clock();

        pipeline.use_clock(clock.as_ref());
        pipeline.set_start_time(gst::ClockTime::NONE);
        pipeline.set_base_time(element.base_time().unwrap());

        let mut bus_stream = pipeline.bus().unwrap().stream();
        let element_clone = element.downgrade();
        let pipeline_clone = pipeline.downgrade();
        let session_id_clone = session_id.to_owned();

        RUNTIME.spawn(async move {
            while let Some(msg) = bus_stream.next().await {
                if let Some(element) = element_clone.upgrade() {
                    let this = element.imp();
                    match msg.view() {
                        gst::MessageView::Error(err) => {
                            gst::error!(
                                CAT,
                                "session {} error: {}, details: {:?}",
                                session_id_clone,
                                err.error(),
                                err.debug()
                            );
                            let _ = this.remove_session(&element, &session_id_clone, true);
                        }
                        gst::MessageView::StateChanged(state_changed) => {
                            if let Some(pipeline) = pipeline_clone.upgrade() {
                                if state_changed.src() == Some(pipeline.upcast_ref()) {
                                    pipeline.debug_to_dot_file_with_ts(
                                        gst::DebugGraphDetails::all(),
                                        format!(
                                            "webrtcsink-session-{}-{:?}-to-{:?}",
                                            session_id_clone,
                                            state_changed.old(),
                                            state_changed.current()
                                        ),
                                    );
                                }
                            }
                        }
                        gst::MessageView::Latency(..) => {
                            if let Some(pipeline) = pipeline_clone.upgrade() {
                                gst::info!(CAT, obj: pipeline, "Recalculating latency");
                                let _ = pipeline.recalculate_latency();
                            }
                        }
                        gst::MessageView::Eos(..) => {
                            gst::error!(
                                CAT,
                                "Unexpected end of stream in session {}",
                                session_id_clone,
                            );
                            let _ = this.remove_session(&element, &session_id_clone, true);
                        }
                        _ => (),
                    }
                }
            }
        });

        pipeline.set_state(gst::State::Ready).map_err(|err| {
            WebRTCSinkError::SessionPipelineError {
                session_id: session_id.to_string(),
                peer_id: peer_id.to_string(),
                details: err.to_string(),
            }
        })?;

        if settings.enable_data_channel_navigation {
            state.navigation_handler = Some(NavigationEventHandler::new(element, &webrtcbin));
        }

        state.sessions.insert(session_id.to_string(), session);

        drop(state);
        drop(settings);

        // This is intentionally emitted with the pipeline in the Ready state,
        // so that application code can create data channels at the correct
        // moment.
        element.emit_by_name::<()>("consumer-added", &[&peer_id, &webrtcbin]);

        // We don't connect to on-negotiation-needed, this in order to call the above
        // signal without holding the state lock:
        //
        // Going to Ready triggers synchronous emission of the on-negotiation-needed
        // signal, during which time the application may add a data channel, causing
        // renegotiation, which we do not support at this time.
        //
        // This is completely safe, as we know that by now all conditions are gathered:
        // webrtcbin is in the Ready state, and all its transceivers have codec_preferences.
        self.negotiate(element, &session_id);

        pipeline.set_state(gst::State::Playing).map_err(|err| {
            WebRTCSinkError::SessionPipelineError {
                session_id: session_id.to_string(),
                peer_id: peer_id.to_string(),
                details: err.to_string(),
            }
        })?;

        Ok(())
    }

    /// Called by the signaller to remove a consumer
    pub fn remove_session(
        &self,
        element: &super::WebRTCSink,
        session_id: &str,
        signal: bool,
    ) -> Result<(), WebRTCSinkError> {
        let mut state = self.state.lock().unwrap();

        if !state.sessions.contains_key(session_id) {
            return Err(WebRTCSinkError::NoSessionWithId(session_id.to_string()));
        }

        if let Some(session) = state.end_session(element, session_id, signal) {
            drop(state);
            element.emit_by_name::<()>("consumer-removed", &[&session.peer_id, &session.webrtcbin]);
        }

        Ok(())
    }

    fn process_loss_stats(
        &self,
        element: &super::WebRTCSink,
        session_id: &str,
        stats: &gst::Structure,
    ) {
        let mut state = element.imp().state.lock().unwrap();
        if let Some(session) = state.sessions.get_mut(session_id) {
            if let Some(congestion_controller) = session.congestion_controller.as_mut() {
                congestion_controller.loss_control(element, stats, &mut session.encoders);
            }
            session.stats = stats.to_owned();
        }
    }

    fn process_stats(
        &self,
        element: &super::WebRTCSink,
        webrtcbin: gst::Element,
        session_id: &str,
    ) {
        let session_id = session_id.to_string();
        let promise = gst::Promise::with_change_func(
            glib::clone!(@strong session_id, @weak element => move |reply| {
                if let Ok(Some(stats)) = reply {

                    let mut state = element.imp().state.lock().unwrap();
                    if let Some(session) = state.sessions.get_mut(&session_id) {
                        if let Some(congestion_controller) = session.congestion_controller.as_mut() {
                            congestion_controller.delay_control(&element, stats, &mut session.encoders,);
                        }
                        session.stats = stats.to_owned();
                    }
                }
            }),
        );

        webrtcbin.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
    }

    fn set_rtptrxsend(&self, element: &super::WebRTCSink, peer_id: &str, rtprtxsend: gst::Element) {
        let mut state = element.imp().state.lock().unwrap();

        if let Some(session) = state.sessions.get_mut(peer_id) {
            session.rtprtxsend = Some(rtprtxsend);
        }
    }

    fn set_bitrate(&self, element: &super::WebRTCSink, peer_id: &str, bitrate: u32) {
        let settings = element.imp().settings.lock().unwrap();
        let mut state = element.imp().state.lock().unwrap();

        if let Some(session) = state.sessions.get_mut(peer_id) {
            let n_encoders = session.encoders.len();

            let fec_ratio = {
                if settings.do_fec && bitrate > DO_FEC_THRESHOLD {
                    (bitrate as f64 - DO_FEC_THRESHOLD as f64)
                        / ((session.cc_info.max_bitrate as usize * n_encoders) as f64
                            - DO_FEC_THRESHOLD as f64)
                } else {
                    0f64
                }
            };

            let fec_percentage = fec_ratio * 50f64;
            let encoders_bitrate =
                ((bitrate as f64) / (1. + (fec_percentage / 100.)) / (n_encoders as f64)) as i32;

            if let Some(rtpxsend) = session.rtprtxsend.as_ref() {
                rtpxsend.set_property("stuffing-kbps", (bitrate as f64 / 1000.) as i32);
            }

            for encoder in session.encoders.iter_mut() {
                encoder.set_bitrate(element, encoders_bitrate);
                encoder
                    .transceiver
                    .set_property("fec-percentage", (fec_percentage as u32).min(100));
            }
        }
    }

    fn on_remote_description_set(&self, element: &super::WebRTCSink, session_id: String) {
        let mut state = self.state.lock().unwrap();
        let mut remove = false;
        let codecs = state.codecs.clone();

        if let Some(mut session) = state.sessions.remove(&session_id) {
            for webrtc_pad in session.webrtc_pads.clone().values() {
                let transceiver = webrtc_pad
                    .pad
                    .property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");

                if let Some(mid) = transceiver.mid() {
                    state
                        .mids
                        .insert(mid.to_string(), webrtc_pad.stream_name.clone());
                }

                if let Some(producer) = state
                    .streams
                    .get(&webrtc_pad.stream_name)
                    .and_then(|stream| stream.producer.clone())
                {
                    drop(state);
                    if let Err(err) =
                        session.connect_input_stream(element, &producer, webrtc_pad, &codecs)
                    {
                        gst::error!(
                            CAT,
                            obj: element,
                            "Failed to connect input stream {} for session {}: {}",
                            webrtc_pad.stream_name,
                            session_id,
                            err
                        );
                        remove = true;
                        state = self.state.lock().unwrap();
                        break;
                    }
                    state = self.state.lock().unwrap();
                } else {
                    gst::error!(
                        CAT,
                        obj: element,
                        "No producer to connect session {} to",
                        session_id,
                    );
                    remove = true;
                    break;
                }
            }

            session.pipeline.debug_to_dot_file_with_ts(
                gst::DebugGraphDetails::all(),
                format!("webrtcsink-peer-{session_id}-remote-description-set",),
            );

            let element_clone = element.downgrade();
            let webrtcbin = session.webrtcbin.downgrade();
            state.stats_collection_handle = Some(RUNTIME.spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));

                loop {
                    interval.tick().await;
                    let element_clone = element_clone.clone();
                    if let (Some(webrtcbin), Some(element)) =
                        (webrtcbin.upgrade(), element_clone.upgrade())
                    {
                        element
                            .imp()
                            .process_stats(&element, webrtcbin, &session_id);
                    } else {
                        break;
                    }
                }
            }));

            if remove {
                state.finalize_session(element, &mut session, true);
            } else {
                state.sessions.insert(session.id.clone(), session);
            }
        }
    }

    /// Called by the signaller with an ice candidate
    pub fn handle_ice(
        &self,
        _element: &super::WebRTCSink,
        session_id: &str,
        sdp_m_line_index: Option<u32>,
        _sdp_mid: Option<String>,
        candidate: &str,
    ) -> Result<(), WebRTCSinkError> {
        let state = self.state.lock().unwrap();

        let sdp_m_line_index = sdp_m_line_index.ok_or(WebRTCSinkError::MandatorySdpMlineIndex)?;

        if let Some(session) = state.sessions.get(session_id) {
            gst::trace!(CAT, "adding ice candidate for session {}", session_id);
            session
                .webrtcbin
                .emit_by_name::<()>("add-ice-candidate", &[&sdp_m_line_index, &candidate]);
            Ok(())
        } else {
            Err(WebRTCSinkError::NoSessionWithId(session_id.to_string()))
        }
    }

    /// Called by the signaller with an answer to our offer
    pub fn handle_sdp(
        &self,
        element: &super::WebRTCSink,
        session_id: &str,
        desc: &gst_webrtc::WebRTCSessionDescription,
    ) -> Result<(), WebRTCSinkError> {
        let mut state = self.state.lock().unwrap();

        if let Some(session) = state.sessions.get_mut(session_id) {
            let sdp = desc.sdp();

            session.sdp = Some(sdp.to_owned());

            for webrtc_pad in session.webrtc_pads.values_mut() {
                let media_idx = webrtc_pad.media_idx;
                /* TODO: support partial answer, webrtcbin doesn't seem
                 * very well equipped to deal with this at the moment */
                if let Some(media) = sdp.media(media_idx) {
                    if media.attribute_val("inactive").is_some() {
                        let media_str = sdp
                            .media(webrtc_pad.media_idx)
                            .and_then(|media| media.as_text().ok());

                        gst::warning!(
                            CAT,
                            "consumer from session {} refused media {}: {:?}",
                            session_id,
                            media_idx,
                            media_str
                        );
                        state.end_session(element, session_id, true);

                        return Err(WebRTCSinkError::ConsumerRefusedMedia {
                            session_id: session_id.to_string(),
                            media_idx,
                        });
                    }
                }

                if let Some(payload) = sdp
                    .media(webrtc_pad.media_idx)
                    .and_then(|media| media.format(0))
                    .and_then(|format| format.parse::<i32>().ok())
                {
                    webrtc_pad.payload = Some(payload);
                } else {
                    gst::warning!(
                        CAT,
                        "consumer from session {} did not provide valid payload for media index {} for session {}",
                        session_id,
                        media_idx,
                        session_id,
                    );

                    state.end_session(element, session_id, true);

                    return Err(WebRTCSinkError::ConsumerNoValidPayload {
                        session_id: session_id.to_string(),
                        media_idx,
                    });
                }
            }

            let element = element.downgrade();
            let session_id = session_id.to_string();

            let promise = gst::Promise::with_change_func(move |reply| {
                gst::debug!(CAT, "received reply {:?}", reply);
                if let Some(element) = element.upgrade() {
                    let this = element.imp();

                    this.on_remote_description_set(&element, session_id);
                }
            });

            session
                .webrtcbin
                .emit_by_name::<()>("set-remote-description", &[desc, &promise]);

            Ok(())
        } else {
            Err(WebRTCSinkError::NoSessionWithId(session_id.to_string()))
        }
    }

    async fn run_discovery_pipeline(
        element: &super::WebRTCSink,
        name: &str,
        codec: &Codec,
        caps: &gst::Caps,
    ) -> Result<gst::Structure, Error> {
        let pipe = PipelineWrapper(gst::Pipeline::default());

        let src = if codec.is_video() {
            make_element("videotestsrc", None)?
        } else {
            make_element("audiotestsrc", None)?
        };
        let mut elements = vec![src.clone()];

        if codec.is_video() {
            elements.push(make_converter_for_video_caps(caps)?);
        }

        gst::debug!(
            CAT,
            obj: element,
            "Running discovery pipeline for caps {caps} with codec {codec:?}"
        );

        let capsfilter = make_element("capsfilter", None)?;
        elements.push(capsfilter.clone());
        let elements_slice = &elements.iter().collect::<Vec<_>>();
        pipe.0.add_many(elements_slice).unwrap();
        gst::Element::link_many(elements_slice)
            .with_context(|| format!("Running discovery pipeline for caps {caps}"))?;

        let encoded_filter = element.emit_by_name::<Option<gst::Element>>(
            "request-encoded-filter",
            &[&Option::<String>::None, &name, &codec.caps],
        );

        let (_, _, pay) = setup_encoding(
            &pipe.0,
            &capsfilter,
            caps,
            codec,
            encoded_filter,
            None,
            true,
        )?;

        let sink = make_element("fakesink", None)?;

        pipe.0.add(&sink).unwrap();

        pay.link(&sink)
            .with_context(|| format!("Running discovery pipeline for caps {caps}"))?;

        capsfilter.set_property("caps", caps);

        src.set_property("num-buffers", 1);

        let mut stream = pipe.0.bus().unwrap().stream();

        pipe.0
            .set_state(gst::State::Playing)
            .with_context(|| format!("Running discovery pipeline for caps {caps}"))?;

        let in_caps = caps;

        while let Some(msg) = stream.next().await {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    pipe.0.debug_to_dot_file_with_ts(
                        gst::DebugGraphDetails::all(),
                        "webrtcsink-discovery-error",
                    );
                    return Err(err.error().into());
                }
                gst::MessageView::Eos(_) => {
                    let caps = pay.static_pad("src").unwrap().current_caps().unwrap();

                    pipe.0.debug_to_dot_file_with_ts(
                        gst::DebugGraphDetails::all(),
                        "webrtcsink-discovery-done",
                    );

                    if let Some(s) = caps.structure(0) {
                        let mut s = s.to_owned();
                        s.remove_fields([
                            "timestamp-offset",
                            "seqnum-offset",
                            "ssrc",
                            "sprop-parameter-sets",
                            "a-framerate",
                        ]);
                        s.set("payload", codec.payload);
                        gst::debug!(
                            CAT,
                            obj: element,
                            "Codec discovery pipeline for caps {in_caps} with codec {codec:?} succeeded: {s}"
                        );
                        return Ok(s);
                    } else {
                        return Err(anyhow!("Discovered empty caps"));
                    }
                }
                _ => {
                    continue;
                }
            }
        }

        unreachable!()
    }

    async fn lookup_caps(
        element: &super::WebRTCSink,
        name: String,
        in_caps: gst::Caps,
        codecs: &BTreeMap<i32, Codec>,
    ) -> (String, gst::Caps) {
        let sink_caps = in_caps.as_ref().to_owned();

        let is_video = match sink_caps.structure(0).unwrap().name().as_str() {
            "video/x-raw" => true,
            "audio/x-raw" => false,
            _ => unreachable!(),
        };

        let mut payloader_caps = gst::Caps::new_empty();
        let payloader_caps_mut = payloader_caps.make_mut();

        let futs = codecs
            .iter()
            .filter(|(_, codec)| codec.is_video() == is_video)
            .map(|(_, codec)| {
                WebRTCSink::run_discovery_pipeline(element, &name, codec, &sink_caps)
            });

        for ret in futures::future::join_all(futs).await {
            match ret {
                Ok(s) => {
                    payloader_caps_mut.append_structure(s);
                }
                Err(err) => {
                    /* We don't consider this fatal, as long as we end up with one
                     * potential codec for each input stream
                     */
                    gst::warning!(
                        CAT,
                        obj: element,
                        "Codec discovery pipeline failed: {}",
                        err
                    );
                }
            }
        }

        (name, payloader_caps)
    }

    async fn lookup_streams_caps(&self, element: &super::WebRTCSink) -> Result<(), Error> {
        let codecs = self.lookup_codecs();

        gst::debug!(CAT, obj: element, "Looked up codecs {codecs:?}");

        let futs: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .streams
            .iter()
            .map(|(name, stream)| {
                WebRTCSink::lookup_caps(
                    element,
                    name.to_owned(),
                    stream.in_caps.as_ref().unwrap().to_owned(),
                    &codecs,
                )
            })
            .collect();

        let caps: Vec<(String, gst::Caps)> = futures::future::join_all(futs).await;

        let mut state = self.state.lock().unwrap();

        for (name, caps) in caps {
            if caps.is_empty() {
                return Err(anyhow!("No caps found for stream {}", name));
            }

            if let Some(mut stream) = state.streams.get_mut(&name) {
                stream.out_caps = Some(caps);
            }
        }

        state.codecs = codecs;

        Ok(())
    }

    fn gather_stats(&self) -> gst::Structure {
        gst::Structure::from_iter(
            "application/x-webrtcsink-stats",
            self.state
                .lock()
                .unwrap()
                .sessions
                .iter()
                .map(|(name, consumer)| (name.as_str(), consumer.gather_stats().to_send_value())),
        )
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::WebRTCSink, event: gst::Event) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::Caps(e) => {
                if let Some(caps) = pad.current_caps() {
                    if caps.is_strictly_equal(e.caps()) {
                        // Nothing changed
                        true
                    } else {
                        gst::error!(CAT, obj: pad, "Renegotiation is not supported");
                        false
                    }
                } else {
                    gst::info!(CAT, obj: pad, "Received caps event {:?}", e);

                    let mut all_pads_have_caps = true;

                    self.state
                        .lock()
                        .unwrap()
                        .streams
                        .iter_mut()
                        .for_each(|(_, mut stream)| {
                            if stream.sink_pad.upcast_ref::<gst::Pad>() == pad {
                                stream.in_caps = Some(e.caps().to_owned());
                            } else if stream.in_caps.is_none() {
                                all_pads_have_caps = false;
                            }
                        });

                    if all_pads_have_caps {
                        let element_clone = element.downgrade();
                        RUNTIME.spawn(async move {
                            if let Some(element) = element_clone.upgrade() {
                                let this = element.imp();
                                let (fut, handle) =
                                    futures::future::abortable(this.lookup_streams_caps(&element));

                                let (codecs_done_sender, codecs_done_receiver) =
                                    futures::channel::oneshot::channel();

                                // Compiler isn't budged by dropping state before await,
                                // so let's make a new scope instead.
                                {
                                    let mut state = this.state.lock().unwrap();
                                    state.codecs_abort_handle = Some(handle);
                                    state.codecs_done_receiver = Some(codecs_done_receiver);
                                }

                                match fut.await {
                                    Ok(Err(err)) => {
                                        gst::error!(CAT, obj: element, "error: {}", err);
                                        gst::element_error!(
                                            element,
                                            gst::StreamError::CodecNotFound,
                                            ["Failed to look up output caps: {}", err]
                                        );
                                    }
                                    Ok(Ok(_)) => {
                                        let mut state = this.state.lock().unwrap();
                                        state.codec_discovery_done = true;
                                        state.maybe_start_signaller(&element);
                                    }
                                    _ => (),
                                }

                                let _ = codecs_done_sender.send(());
                            }
                        });
                    }

                    gst::Pad::event_default(pad, Some(element), event)
                }
            }
            _ => gst::Pad::event_default(pad, Some(element), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WebRTCSink {
    const NAME: &'static str = "GstWebRTCSink";
    type Type = super::WebRTCSink;
    type ParentType = gst::Bin;
    type Interfaces = (gst::ChildProxy, gst_video::Navigation);
}

impl ObjectImpl for WebRTCSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecBoxed::builder::<gst::Caps>("video-caps")
                    .nick("Video encoder caps")
                    .blurb("Governs what video codecs will be proposed")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("audio-caps")
                    .nick("Audio encoder caps")
                    .blurb("Governs what audio codecs will be proposed")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("stun-server")
                    .nick("STUN Server")
                    .blurb("The STUN server of the form stun://hostname:port")
                    .default_value(DEFAULT_STUN_SERVER)
                    .build(),
                gst::ParamSpecArray::builder("turn-servers")
                    .nick("List of TURN Servers to user")
                    .blurb("The TURN servers of the form <\"turn(s)://username:password@host:port\", \"turn(s)://username1:password1@host1:port1\">")
                    .element_spec(&glib::ParamSpecString::builder("turn-server")
                        .nick("TURN Server")
                        .blurb("The TURN server of the form turn(s)://username:password@host:port.")
                        .build()
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("congestion-control", DEFAULT_CONGESTION_CONTROL)
                    .nick("Congestion control")
                    .blurb("Defines how congestion is controlled, if at all")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("min-bitrate")
                    .nick("Minimal Bitrate")
                    .blurb("Minimal bitrate to use (in bit/sec) when computing it through the congestion control algorithm")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_MIN_BITRATE)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("max-bitrate")
                    .nick("Maximum Bitrate")
                    .blurb("Maximum bitrate to use (in bit/sec) when computing it through the congestion control algorithm")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_MAX_BITRATE)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("start-bitrate")
                    .nick("Start Bitrate")
                    .blurb("Start bitrate to use (in bit/sec)")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_START_BITRATE)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
                    .nick("Consumer statistics")
                    .blurb("Statistics for the current consumers")
                    .read_only()
                    .build(),
                glib::ParamSpecBoolean::builder("do-fec")
                    .nick("Do Forward Error Correction")
                    .blurb("Whether the element should negotiate and send FEC data")
                    .default_value(DEFAULT_DO_FEC)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("do-retransmission")
                    .nick("Do retransmission")
                    .blurb("Whether the element should offer to honor retransmission requests")
                    .default_value(DEFAULT_DO_RETRANSMISSION)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("enable-data-channel-navigation")
                    .nick("Enable data channel navigation")
                    .blurb("Enable navigation events through a dedicated WebRTCDataChannel")
                    .default_value(DEFAULT_ENABLE_DATA_CHANNEL_NAVIGATION)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("meta")
                    .nick("Meta")
                    .blurb("Free form metadata about the producer")
                    .build(),
                glib::ParamSpecEnum::builder_with_default("ice-transport-policy", DEFAULT_ICE_TRANSPORT_POLICY)
                    .nick("ICE Transport Policy")
                    .blurb("The policy to apply for ICE transport")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "video-caps" => {
                let mut settings = self.settings.lock().unwrap();
                settings.video_caps = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(gst::Caps::new_empty);
            }
            "audio-caps" => {
                let mut settings = self.settings.lock().unwrap();
                settings.audio_caps = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(gst::Caps::new_empty);
            }
            "stun-server" => {
                let mut settings = self.settings.lock().unwrap();
                settings.stun_server = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
            }
            "turn-servers" => {
                let mut settings = self.settings.lock().unwrap();
                settings.turn_servers = value.get::<gst::Array>().expect("type checked upstream")
            }
            "congestion-control" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cc_info.heuristic = value
                    .get::<WebRTCSinkCongestionControl>()
                    .expect("type checked upstream");
            }
            "min-bitrate" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cc_info.min_bitrate = value.get::<u32>().expect("type checked upstream");
            }
            "max-bitrate" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cc_info.max_bitrate = value.get::<u32>().expect("type checked upstream");
            }
            "start-bitrate" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cc_info.start_bitrate = value.get::<u32>().expect("type checked upstream");
            }
            "do-fec" => {
                let mut settings = self.settings.lock().unwrap();
                settings.do_fec = value.get::<bool>().expect("type checked upstream");
            }
            "do-retransmission" => {
                let mut settings = self.settings.lock().unwrap();
                settings.do_retransmission = value.get::<bool>().expect("type checked upstream");
            }
            "enable-data-channel-navigation" => {
                let mut settings = self.settings.lock().unwrap();
                settings.enable_data_channel_navigation =
                    value.get::<bool>().expect("type checked upstream");
            }
            "meta" => {
                let mut settings = self.settings.lock().unwrap();
                settings.meta = value
                    .get::<Option<gst::Structure>>()
                    .expect("type checked upstream")
            }
            "ice-transport-policy" => {
                let mut settings = self.settings.lock().unwrap();
                settings.ice_transport_policy = value
                    .get::<WebRTCICETransportPolicy>()
                    .expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "video-caps" => {
                let settings = self.settings.lock().unwrap();
                settings.video_caps.to_value()
            }
            "audio-caps" => {
                let settings = self.settings.lock().unwrap();
                settings.audio_caps.to_value()
            }
            "congestion-control" => {
                let settings = self.settings.lock().unwrap();
                settings.cc_info.heuristic.to_value()
            }
            "stun-server" => {
                let settings = self.settings.lock().unwrap();
                settings.stun_server.to_value()
            }
            "turn-servers" => {
                let settings = self.settings.lock().unwrap();
                settings.turn_servers.to_value()
            }
            "min-bitrate" => {
                let settings = self.settings.lock().unwrap();
                settings.cc_info.min_bitrate.to_value()
            }
            "max-bitrate" => {
                let settings = self.settings.lock().unwrap();
                settings.cc_info.max_bitrate.to_value()
            }
            "start-bitrate" => {
                let settings = self.settings.lock().unwrap();
                settings.cc_info.start_bitrate.to_value()
            }
            "do-fec" => {
                let settings = self.settings.lock().unwrap();
                settings.do_fec.to_value()
            }
            "do-retransmission" => {
                let settings = self.settings.lock().unwrap();
                settings.do_retransmission.to_value()
            }
            "enable-data-channel-navigation" => {
                let settings = self.settings.lock().unwrap();
                settings.enable_data_channel_navigation.to_value()
            }
            "stats" => self.gather_stats().to_value(),
            "meta" => {
                let settings = self.settings.lock().unwrap();
                settings.meta.to_value()
            }
            "ice-transport-policy" => {
                let settings = self.settings.lock().unwrap();
                settings.ice_transport_policy.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                /**
                 * RsWebRTCSink::consumer-added:
                 * @consumer_id: Identifier of the consumer added
                 * @webrtcbin: The new webrtcbin
                 *
                 * This signal can be used to tweak @webrtcbin, creating a data
                 * channel for example.
                 */
                glib::subclass::Signal::builder("consumer-added")
                    .param_types([String::static_type(), gst::Element::static_type()])
                    .build(),
                /**
                 * RsWebRTCSink::consumer_removed:
                 * @consumer_id: Identifier of the consumer that was removed
                 * @webrtcbin: The webrtcbin connected to the newly removed consumer
                 *
                 * This signal is emitted right after the connection with a consumer
                 * has been dropped.
                 */
                glib::subclass::Signal::builder("consumer-removed")
                    .param_types([String::static_type(), gst::Element::static_type()])
                    .build(),
                /**
                 * RsWebRTCSink::get_sessions:
                 *
                 * List all sessions (by ID).
                 */
                glib::subclass::Signal::builder("get-sessions")
                    .action()
                    .class_handler(|_, args| {
                        let element = args[0].get::<super::WebRTCSink>().expect("signal arg");
                        let this = element.imp();

                        let res = Some(
                            this.state
                                .lock()
                                .unwrap()
                                .sessions
                                .keys()
                                .cloned()
                                .collect::<Vec<String>>()
                                .to_value(),
                        );
                        res
                    })
                    .return_type::<Vec<String>>()
                    .build(),
                /**
                 * RsWebRTCSink::encoder-setup:
                 * @consumer_id: Identifier of the consumer
                 * @pad_name: The name of the corresponding input pad
                 * @encoder: The constructed encoder
                 *
                 * This signal can be used to tweak @encoder properties.
                 *
                 * Returns: True if the encoder is entirely configured,
                 * False to let other handlers run
                 */
                glib::subclass::Signal::builder("encoder-setup")
                    .param_types([
                        String::static_type(),
                        String::static_type(),
                        gst::Element::static_type(),
                    ])
                    .return_type::<bool>()
                    .accumulator(|_hint, _ret, value| !value.get::<bool>().unwrap())
                    .class_handler(|_, args| {
                        let element = args[0].get::<super::WebRTCSink>().expect("signal arg");
                        let enc = args[3].get::<gst::Element>().unwrap();

                        gst::debug!(
                            CAT,
                            obj: element,
                            "applying default configuration on encoder {:?}",
                            enc
                        );

                        let this = element.imp();
                        let settings = this.settings.lock().unwrap();
                        configure_encoder(&enc, settings.cc_info.start_bitrate);

                        // Return false here so that latter handlers get called
                        Some(false.to_value())
                    })
                    .build(),
                /**
                 * RsWebRTCSink::request-encoded-filter:
                 * @consumer_id: Identifier of the consumer
                 * @pad_name: The name of the corresponding input pad
                 * @encoded_caps: The Caps of the encoded stream
                 *
                 * This signal can be used to insert a filter
                 * element between the encoder and the payloader.
                 *
                 * When called during Caps discovery, the `consumer_id` is `None`.
                 *
                 * Returns: the element to insert.
                 */
                glib::subclass::Signal::builder("request-encoded-filter")
                    .param_types([
                        Option::<String>::static_type(),
                        String::static_type(),
                        gst::Caps::static_type(),
                    ])
                    .return_type::<gst::Element>()
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);
        obj.set_element_flags(gst::ElementFlags::SINK);
    }
}

impl GstObjectImpl for WebRTCSink {}

impl ElementImpl for WebRTCSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "WebRTCSink",
                "Sink/Network/WebRTC",
                "WebRTC sink",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder_full()
                .structure(gst::Structure::builder("video/x-raw").build())
                .structure_with_features(
                    gst::Structure::builder("video/x-raw").build(),
                    gst::CapsFeatures::new([CUDA_MEMORY_FEATURE]),
                )
                .structure_with_features(
                    gst::Structure::builder("video/x-raw").build(),
                    gst::CapsFeatures::new([GL_MEMORY_FEATURE]),
                )
                .structure_with_features(
                    gst::Structure::builder("video/x-raw").build(),
                    gst::CapsFeatures::new([NVMM_MEMORY_FEATURE]),
                )
                .build();
            let video_pad_template = gst::PadTemplate::new(
                "video_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
            )
            .unwrap();

            let caps = gst::Caps::builder("audio/x-raw").build();
            let audio_pad_template = gst::PadTemplate::new(
                "audio_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
            )
            .unwrap();

            vec![video_pad_template, audio_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        _name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let element = self.obj();
        if element.current_state() > gst::State::Ready {
            gst::error!(CAT, "element pads can only be requested before starting");
            return None;
        }

        let mut state = self.state.lock().unwrap();

        let serial;

        let name = if templ.name().starts_with("video_") {
            let name = format!("video_{}", state.video_serial);
            serial = state.video_serial;
            state.video_serial += 1;
            name
        } else {
            let name = format!("audio_{}", state.audio_serial);
            serial = state.audio_serial;
            state.audio_serial += 1;
            name
        };

        let sink_pad = gst::GhostPad::builder_with_template(templ, Some(name.as_str()))
            .event_function(|pad, parent, event| {
                WebRTCSink::catch_panic_pad_function(
                    parent,
                    || false,
                    |this| this.sink_event(pad.upcast_ref(), &this.obj(), event),
                )
            })
            .build();

        sink_pad.set_active(true).unwrap();
        sink_pad.use_fixed_caps();
        element.add_pad(&sink_pad).unwrap();

        state.streams.insert(
            name,
            InputStream {
                sink_pad: sink_pad.clone(),
                producer: None,
                in_caps: None,
                out_caps: None,
                clocksync: None,
                serial,
            },
        );

        Some(sink_pad.upcast())
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let element = self.obj();
        if let gst::StateChange::ReadyToPaused = transition {
            if let Err(err) = self.prepare(&element) {
                gst::element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Failed to prepare: {}", err]
                );
                return Err(gst::StateChangeError);
            }
        }

        let mut ret = self.parent_change_state(transition);

        match transition {
            gst::StateChange::PausedToReady => {
                if let Err(err) = self.unprepare(&element) {
                    gst::element_error!(
                        element,
                        gst::StreamError::Failed,
                        ["Failed to unprepare: {}", err]
                    );
                    return Err(gst::StateChangeError);
                }
            }
            gst::StateChange::ReadyToPaused => {
                ret = Ok(gst::StateChangeSuccess::NoPreroll);
            }
            gst::StateChange::PausedToPlaying => {
                let mut state = self.state.lock().unwrap();
                state.maybe_start_signaller(&element);
            }
            _ => (),
        }

        ret
    }
}

impl BinImpl for WebRTCSink {}

impl ChildProxyImpl for WebRTCSink {
    fn child_by_index(&self, _index: u32) -> Option<glib::Object> {
        None
    }

    fn children_count(&self) -> u32 {
        0
    }

    fn child_by_name(&self, name: &str) -> Option<glib::Object> {
        match name {
            "signaller" => Some(
                self.state
                    .lock()
                    .unwrap()
                    .signaller
                    .as_ref()
                    .as_ref()
                    .clone(),
            ),
            _ => None,
        }
    }
}

impl NavigationImpl for WebRTCSink {
    fn send_event(&self, event_def: gst::Structure) {
        let mut state = self.state.lock().unwrap();
        let event = gst::event::Navigation::new(event_def);

        state.streams.iter_mut().for_each(|(_, stream)| {
            if stream.sink_pad.name().starts_with("video_") {
                gst::log!(CAT, "Navigating to: {:?}", event);
                // FIXME: Handle multi tracks.
                if !stream.sink_pad.push_event(event.clone()) {
                    gst::info!(CAT, "Could not send event: {:?}", event);
                }
            }
        });
    }
}
