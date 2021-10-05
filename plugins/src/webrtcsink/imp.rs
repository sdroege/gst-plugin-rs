use anyhow::Context;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info, gst_log, gst_trace, gst_warning};

use async_std::task;
use futures::prelude::*;

use anyhow::{anyhow, Error};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;

use super::utils::{make_element, StreamProducer};
use crate::signaller::Signaller;
use std::collections::BTreeMap;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtcsink",
        gst::DebugColorFlags::empty(),
        Some("WebRTC sink"),
    )
});

/// User configuration
struct Settings {
    video_caps: gst::Caps,
    audio_caps: gst::Caps,
}

/// Represents a codec we can offer
#[derive(Debug)]
struct Codec {
    is_video: bool,
    encoder: gst::ElementFactory,
    payloader: gst::ElementFactory,
    caps: gst::Caps,
    payload: i32,
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
}

/// Wrapper around webrtcbin pads
#[derive(Clone)]
struct WebRTCPad {
    pad: gst::Pad,
    /// Our offer caps
    caps: gst::Caps,
    /// The m= line index in the SDP
    media_idx: u32,
    ssrc: u32,
    /// The name of the corresponding InputStream's sink_pad
    stream_name: String,
    /// The payload selected in the answer, None at first
    payload: Option<i32>,
}

struct Consumer {
    pipeline: gst::Pipeline,
    webrtcbin: gst::Element,
    webrtc_pads: HashMap<u32, WebRTCPad>,
    peer_id: String,
}

#[derive(PartialEq)]
enum SignallerState {
    Started,
    Stopped,
}

/* Our internal state */
struct State {
    signaller: Box<dyn super::SignallableObject>,
    signaller_state: SignallerState,
    consumers: HashMap<String, Consumer>,
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
}

/// Simple utility for tearing down a pipeline cleanly
struct PipelineWrapper(gst::Pipeline);

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
                .iter()
                .map(|s| gst::Structure::new_empty(s))
                .collect::<gst::Caps>(),
            audio_caps: ["audio/x-opus"]
                .iter()
                .map(|s| gst::Structure::new_empty(s))
                .collect::<gst::Caps>(),
        }
    }
}

impl Default for State {
    fn default() -> Self {
        let signaller = Signaller::new();

        Self {
            signaller: Box::new(signaller),
            signaller_state: SignallerState::Stopped,
            consumers: HashMap::new(),
            codecs: BTreeMap::new(),
            codecs_abort_handle: None,
            codecs_done_receiver: None,
            codec_discovery_done: false,
            audio_serial: 0,
            video_serial: 0,
            streams: HashMap::new(),
        }
    }
}

/// Bit of an awkward function, but the goal here is to keep
/// most of the encoding code for consumers in line with
/// the codec discovery code, and this gets the job done.
fn setup_encoding(
    pipeline: &gst::Pipeline,
    src: &gst::Element,
    codec: &Codec,
    ssrc: Option<u32>,
) -> Result<gst::Element, Error> {
    let conv = match codec.is_video {
        true => make_element("videoconvert", None)?,
        false => gst::parse_bin_from_description("audioresample ! audioconvert", true)?.upcast(),
    };

    let conv_filter = make_element("capsfilter", None)?;

    let enc = codec
        .encoder
        .create(None)
        .with_context(|| format!("Creating encoder {}", codec.encoder.name()))?;
    let pay = codec
        .payloader
        .create(None)
        .with_context(|| format!("Creating payloader {}", codec.payloader.name()))?;
    let parse_filter = make_element("capsfilter", None)?;

    pay.set_property("pt", codec.payload as u32).unwrap();

    if let Some(ssrc) = ssrc {
        pay.set_property("ssrc", ssrc).unwrap();
    }

    pipeline
        .add_many(&[&conv, &conv_filter, &enc, &parse_filter, &pay])
        .unwrap();
    gst::Element::link_many(&[src, &conv, &conv_filter, &enc])
        .with_context(|| "Linking encoding elements")?;

    let codec_name = codec.caps.structure(0).unwrap().name();

    if let Some(parser) = if codec_name == "video/x-h264" {
        Some(make_element("h264parse", None)?)
    } else if codec_name == "video/x-h265" {
        Some(make_element("h265parse", None)?)
    } else {
        None
    } {
        pipeline.add(&parser).unwrap();
        gst::Element::link_many(&[&enc, &parser, &parse_filter])
            .with_context(|| "Linking encoding elements")?;
    } else {
        gst::Element::link_many(&[&enc, &parse_filter])
            .with_context(|| "Linking encoding elements")?;
    }

    // Quirk: nvh264enc can perform conversion from RGB formats, but
    // doesn't advertise / negotiate colorimetry correctly, leading
    // to incorrect color display in Chrome (but interestingly not in
    // Firefox). In any case, restrict to exclude RGB formats altogether,
    // and let videoconvert do the conversion properly if needed.
    let conv_caps = if codec.encoder.name() == "nvh264enc" {
        gst::Caps::builder("video/x-raw")
            .field("format", &gst::List::new(&[&"NV12", &"YV12", &"I420"]))
            .build()
    } else {
        gst::Caps::new_any()
    };

    conv_filter.set_property("caps", conv_caps).unwrap();

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

    parse_filter.set_property("caps", parse_caps).unwrap();

    gst::Element::link_many(&[&parse_filter, &pay]).with_context(|| "Linking encoding elements")?;

    Ok(pay)
}

impl State {
    fn remove_consumer(&mut self, element: &super::WebRTCSink, peer_id: &str, signal: bool) {
        if let Some(consumer) = self.consumers.remove(peer_id) {
            for webrtc_pad in consumer.webrtc_pads.values() {
                if let Some(producer) = self
                    .streams
                    .get(&webrtc_pad.stream_name)
                    .and_then(|stream| stream.producer.as_ref())
                {
                    consumer.disconnect_input_stream(producer);
                }
            }

            consumer.pipeline.call_async(|pipeline| {
                let _ = pipeline.set_state(gst::State::Null);
            });

            if signal {
                self.signaller.consumer_removed(element, peer_id);
            }
        }
    }

    fn maybe_start_signaller(&mut self, element: &super::WebRTCSink) {
        if self.signaller_state == SignallerState::Stopped
            && element.current_state() == gst::State::Playing
            && self.codec_discovery_done
        {
            if let Err(err) = self.signaller.start(&element) {
                gst_error!(CAT, obj: element, "error: {}", err);
                gst::element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Failed to start signaller {}", err]
                );
            } else {
                gst_info!(CAT, "Started signaller");
                self.signaller_state = SignallerState::Started;
            }
        }
    }

    fn maybe_stop_signaller(&mut self, element: &super::WebRTCSink) {
        if self.signaller_state == SignallerState::Started {
            self.signaller.stop(element);
            self.signaller_state = SignallerState::Stopped;
            gst_info!(CAT, "Stopped signaller");
        }
    }
}

impl Consumer {
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
        caps: &gst::Caps,
        stream_name: &str,
    ) {
        let ssrc = self.generate_ssrc();
        let media_idx = self.webrtc_pads.len() as i32;

        let mut payloader_caps = caps.to_owned();

        {
            let payloader_caps_mut = payloader_caps.make_mut();
            payloader_caps_mut.set_simple(&[("ssrc", &ssrc)]);
        }

        gst_info!(
            CAT,
            obj: element,
            "Requesting WebRTC pad for consumer {} with caps {}",
            self.peer_id,
            payloader_caps
        );

        let pad = self
            .webrtcbin
            .request_pad_simple(&format!("sink_{}", media_idx))
            .unwrap();

        let transceiver = pad
            .property("transceiver")
            .unwrap()
            .get::<gst_webrtc::WebRTCRTPTransceiver>()
            .unwrap();

        transceiver
            .set_property(
                "direction",
                gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
            )
            .unwrap();

        transceiver
            .set_property("codec-preferences", &payloader_caps)
            .unwrap();

        self.webrtc_pads.insert(
            ssrc,
            WebRTCPad {
                pad,
                caps: payloader_caps,
                media_idx: media_idx as u32,
                ssrc,
                stream_name: stream_name.to_string(),
                payload: None,
            },
        );
    }

    /// Called when we have received an answer, connects an InputStream
    /// to a given WebRTCPad
    fn connect_input_stream(
        &self,
        element: &super::WebRTCSink,
        producer: &StreamProducer,
        webrtc_pad: &WebRTCPad,
        codecs: &BTreeMap<i32, Codec>,
    ) -> Result<(), Error> {
        gst_info!(
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

        let appsrc = make_element("appsrc", None)?;
        self.pipeline.add(&appsrc).unwrap();

        let pay = setup_encoding(&self.pipeline, &appsrc, codec, Some(webrtc_pad.ssrc))?;

        let appsrc = appsrc.downcast::<gst_app::AppSrc>().unwrap();

        appsrc.set_format(gst::Format::Time);
        appsrc.set_is_live(true);
        appsrc.set_handle_segment_change(true);

        self.pipeline
            .sync_children_states()
            .with_context(|| format!("Connecting input stream for {}", self.peer_id))?;

        let srcpad = pay.static_pad("src").unwrap();

        srcpad
            .link(&webrtc_pad.pad)
            .with_context(|| format!("Connecting input stream for {}", self.peer_id))?;

        producer.add_consumer(&appsrc, &self.peer_id);

        Ok(())
    }

    /// Called when tearing down the consumer
    fn disconnect_input_stream(&self, producer: &StreamProducer) {
        producer.remove_consumer(&self.peer_id);
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

        let producer = StreamProducer::from(&appsink);
        producer.forward();

        self.producer = Some(producer);

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

impl WebRTCSink {
    /// Build an ordered map of Codecs, given user-provided audio / video caps */
    fn lookup_codecs(&self) -> BTreeMap<i32, Codec> {
        /* First gather all encoder and payloader factories */
        let encoders = gst::ElementFactory::list_get_elements(
            gst::ElementFactoryListType::ENCODER,
            gst::Rank::Marginal,
        );

        let payloaders = gst::ElementFactory::list_get_elements(
            gst::ElementFactoryListType::PAYLOADER,
            gst::Rank::Marginal,
        );

        /* Now iterate user-provided codec preferences and determine
         * whether we can fulfill these preferences */
        let settings = self.settings.lock().unwrap();
        let mut payload = (96..128).into_iter();

        settings
            .video_caps
            .iter()
            .map(|s| (true, s))
            .chain(settings.audio_caps.iter().map(|s| (false, s)))
            .filter_map(|(is_video, s)| {
                let caps = gst::Caps::builder_full().structure(s.to_owned()).build();

                gst::ElementFactory::list_filter(&encoders, &caps, gst::PadDirection::Src, false)
                    .get(0)
                    .zip(
                        gst::ElementFactory::list_filter(
                            &payloaders,
                            &caps,
                            gst::PadDirection::Sink,
                            false,
                        )
                        .get(0),
                    )
                    .map(|(encoder, payloader)| {
                        /* Assign a payload type to the codec */
                        if let Some(pt) = payload.next() {
                            Some(Codec {
                                is_video,
                                encoder: encoder.clone(),
                                payloader: payloader.clone(),
                                caps,
                                payload: pt,
                            })
                        } else {
                            gst_warning!(CAT, obj: &self.instance(),
                                "Too many formats for available payload type range, ignoring {}",
                                s);
                            None
                        }
                    })
                    .flatten()
            })
            .map(|codec| (codec.payload, codec))
            .collect()
    }

    /// Prepare for accepting consumers, by setting
    /// up StreamProducers for each of our sink pads
    fn prepare(&self, element: &super::WebRTCSink) -> Result<(), Error> {
        gst_debug!(CAT, obj: element, "preparing");

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
        gst_info!(CAT, obj: element, "unpreparing");

        let mut state = self.state.lock().unwrap();

        let consumer_ids: Vec<_> = state.consumers.keys().map(|k| k.to_owned()).collect();

        for id in consumer_ids {
            state.remove_consumer(element, &id, true);
        }

        state
            .streams
            .iter_mut()
            .for_each(|(_, stream)| stream.unprepare(element));

        if let Some(handle) = state.codecs_abort_handle.take() {
            handle.abort();
        }

        if let Some(receiver) = state.codecs_done_receiver.take() {
            task::block_on(async {
                let _ = receiver.await;
            });
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
        gst_error!(CAT, obj: element, "Signalling error: {:?}", error);

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
        peer_id: String,
    ) {
        let mut state = self.state.lock().unwrap();

        if let Some(consumer) = state.consumers.get(&peer_id) {
            consumer
                .webrtcbin
                .emit_by_name("set-local-description", &[&offer, &None::<gst::Promise>])
                .unwrap();

            if let Err(err) = state.signaller.handle_sdp(element, &peer_id, &offer) {
                gst_warning!(
                    CAT,
                    "Failed to handle SDP for consumer {}: {}",
                    peer_id,
                    err
                );

                state.remove_consumer(element, &peer_id, true);
            }
        }
    }

    fn on_negotiation_needed(&self, element: &super::WebRTCSink, peer_id: String) {
        let state = self.state.lock().unwrap();

        gst_debug!(
            CAT,
            obj: element,
            "On negotiation needed for peer {}",
            peer_id
        );

        if let Some(consumer) = state.consumers.get(&peer_id) {
            let element = element.downgrade();
            gst_debug!(CAT, "Creating offer for peer {}", peer_id);
            let promise = gst::Promise::with_change_func(move |reply| {
                gst_debug!(CAT, "Created offer for peer {}", peer_id);

                if let Some(element) = element.upgrade() {
                    let this = Self::from_instance(&element);
                    let reply = match reply {
                        Ok(Some(reply)) => reply,
                        Ok(None) => {
                            gst_warning!(
                                CAT,
                                obj: &element,
                                "Promise returned without a reply for {}",
                                peer_id
                            );
                            let _ = this.remove_consumer(&element, &peer_id, true);
                            return;
                        }
                        Err(err) => {
                            gst_warning!(
                                CAT,
                                obj: &element,
                                "Promise returned with an error for {}: {:?}",
                                peer_id,
                                err
                            );
                            let _ = this.remove_consumer(&element, &peer_id, true);
                            return;
                        }
                    };

                    if let Ok(offer) = reply
                        .value("offer")
                        .map(|offer| offer.get::<gst_webrtc::WebRTCSessionDescription>().unwrap())
                    {
                        this.on_offer_created(&element, offer, peer_id);
                    } else {
                        gst_warning!(
                            CAT,
                            "Reply without an offer for consumer {}: {:?}",
                            peer_id,
                            reply
                        );
                        let _ = this.remove_consumer(&element, &peer_id, true);
                    }
                }
            });

            consumer
                .webrtcbin
                .emit_by_name("create-offer", &[&None::<gst::Structure>, &promise])
                .unwrap();
        } else {
            gst_debug!(
                CAT,
                obj: element,
                "consumer for peer {} no longer exists",
                peer_id
            );
        }
    }

    fn on_ice_candidate(
        &self,
        element: &super::WebRTCSink,
        peer_id: String,
        sdp_mline_index: u32,
        candidate: String,
    ) {
        let mut state = self.state.lock().unwrap();
        if let Err(err) =
            state
                .signaller
                .handle_ice(element, &peer_id, &candidate, Some(sdp_mline_index), None)
        {
            gst_warning!(
                CAT,
                "Failed to handle ICE for consumer {}: {}",
                peer_id,
                err
            );

            state.remove_consumer(element, &peer_id, true);
        }
    }

    /// Called by the signaller to add a new consumer
    pub fn add_consumer(&self, element: &super::WebRTCSink, peer_id: &str) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();

        if state.consumers.contains_key(peer_id) {
            return Err(anyhow!("We already have a consumer with id {}", peer_id));
        }

        gst_info!(CAT, obj: element, "Adding consumer {}", peer_id);

        let pipeline = gst::Pipeline::new(Some(&format!("consumer-pipeline-{}", peer_id)));

        let webrtcbin = make_element("webrtcbin", None)?;

        webrtcbin
            .set_property_from_str("bundle-policy", "max-bundle")
            .unwrap();

        pipeline.add(&webrtcbin).unwrap();

        let element_clone = element.downgrade();
        let peer_id_clone = peer_id.to_owned();
        webrtcbin
            .connect("on-negotiation-needed", false, move |_| {
                if let Some(element) = element_clone.upgrade() {
                    let this = Self::from_instance(&element);
                    this.on_negotiation_needed(&element, peer_id_clone.to_string());
                }

                None
            })
            .unwrap();

        let element_clone = element.downgrade();
        let peer_id_clone = peer_id.to_owned();
        webrtcbin
            .connect("on-ice-candidate", false, move |values| {
                if let Some(element) = element_clone.upgrade() {
                    let this = Self::from_instance(&element);
                    let sdp_mline_index = values[1].get::<u32>().expect("Invalid argument");
                    let candidate = values[2].get::<String>().expect("Invalid argument");
                    this.on_ice_candidate(
                        &element,
                        peer_id_clone.to_string(),
                        sdp_mline_index,
                        candidate,
                    );
                }
                None
            })
            .unwrap();

        let element_clone = element.downgrade();
        let peer_id_clone = peer_id.to_owned();
        webrtcbin.connect_notify(Some("connection-state"), move |webrtcbin, _pspec| {
            if let Some(element) = element_clone.upgrade() {
                let state = webrtcbin
                    .property("connection-state")
                    .unwrap()
                    .get::<gst_webrtc::WebRTCPeerConnectionState>()
                    .unwrap();

                match state {
                    gst_webrtc::WebRTCPeerConnectionState::Failed => {
                        let this = Self::from_instance(&element);
                        gst_warning!(
                            CAT,
                            obj: &element,
                            "Connection state for consumer {} failed",
                            peer_id_clone
                        );
                        let _ = this.remove_consumer(&element, &peer_id_clone, true);
                    }
                    _ => {
                        gst_log!(
                            CAT,
                            obj: &element,
                            "Connection state for consumer {} changed: {:?}",
                            peer_id_clone,
                            state
                        );
                    }
                }
            }
        });

        let element_clone = element.downgrade();
        let peer_id_clone = peer_id.to_owned();
        webrtcbin.connect_notify(Some("ice-connection-state"), move |webrtcbin, _pspec| {
            if let Some(element) = element_clone.upgrade() {
                let state = webrtcbin
                    .property("ice-connection-state")
                    .unwrap()
                    .get::<gst_webrtc::WebRTCICEConnectionState>()
                    .unwrap();
                let this = Self::from_instance(&element);

                match state {
                    gst_webrtc::WebRTCICEConnectionState::Failed => {
                        gst_warning!(
                            CAT,
                            obj: &element,
                            "Ice connection state for consumer {} failed",
                            peer_id_clone
                        );
                        let _ = this.remove_consumer(&element, &peer_id_clone, true);
                    }
                    _ => {
                        gst_log!(
                            CAT,
                            obj: &element,
                            "Ice connection state for consumer {} changed: {:?}",
                            peer_id_clone,
                            state
                        );
                    }
                }

                if state == gst_webrtc::WebRTCICEConnectionState::Completed {
                    let state = this.state.lock().unwrap();

                    if let Some(consumer) = state.consumers.get(&peer_id_clone) {
                        for webrtc_pad in consumer.webrtc_pads.values() {
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
        let peer_id_clone = peer_id.to_owned();
        webrtcbin.connect_notify(Some("ice-gathering-state"), move |webrtcbin, _pspec| {
            let state = webrtcbin
                .property("ice-gathering-state")
                .unwrap()
                .get::<gst_webrtc::WebRTCICEGatheringState>()
                .unwrap();

            if let Some(element) = element_clone.upgrade() {
                gst_log!(
                    CAT,
                    obj: &element,
                    "Ice gathering state for consumer {} changed: {:?}",
                    peer_id_clone,
                    state
                );
            }
        });

        let mut consumer = Consumer {
            pipeline: pipeline.clone(),
            webrtcbin,
            webrtc_pads: HashMap::new(),
            peer_id: peer_id.to_string(),
        };

        state.streams.iter().for_each(|(_, stream)| {
            consumer.request_webrtcbin_pad(
                element,
                stream.out_caps.as_ref().unwrap(),
                stream.sink_pad.name().as_str(),
            )
        });

        let clock = element.clock();

        pipeline.set_clock(clock.as_ref()).unwrap();
        pipeline.set_start_time(gst::ClockTime::NONE);
        pipeline.set_base_time(element.base_time().unwrap());

        let mut bus_stream = pipeline.bus().unwrap().stream();
        let element_clone = element.downgrade();
        let peer_id_clone = peer_id.to_owned();

        task::spawn(async move {
            while let Some(msg) = bus_stream.next().await {
                if let Some(element) = element_clone.upgrade() {
                    let this = Self::from_instance(&element);
                    match msg.view() {
                        gst::MessageView::Error(err) => {
                            gst_error!(CAT, "Consumer {} error: {}", peer_id_clone, err.error());
                            let _ = this.remove_consumer(&element, &peer_id_clone, true);
                        }
                        gst::MessageView::Eos(..) => {
                            gst_error!(
                                CAT,
                                "Unexpected end of stream for consumer {}",
                                peer_id_clone
                            );
                            let _ = this.remove_consumer(&element, &peer_id_clone, true);
                        }
                        _ => (),
                    }
                }
            }
        });

        pipeline.set_state(gst::State::Playing)?;

        state.consumers.insert(peer_id.to_string(), consumer);

        Ok(())
    }

    /// Called by the signaller to remove a consumer
    pub fn remove_consumer(
        &self,
        element: &super::WebRTCSink,
        peer_id: &str,
        signal: bool,
    ) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();

        if !state.consumers.contains_key(peer_id) {
            return Err(anyhow!("No consumer with ID {}", peer_id));
        }

        state.remove_consumer(element, peer_id, signal);

        Ok(())
    }

    fn on_remote_description_set(&self, element: &super::WebRTCSink, peer_id: String) {
        let state = self.state.lock().unwrap();
        let mut remove = false;

        if let Some(consumer) = state.consumers.get(&peer_id) {
            for webrtc_pad in consumer.webrtc_pads.values() {
                if let Some(producer) = state
                    .streams
                    .get(&webrtc_pad.stream_name)
                    .and_then(|stream| stream.producer.as_ref())
                {
                    if let Err(err) =
                        consumer.connect_input_stream(element, producer, webrtc_pad, &state.codecs)
                    {
                        gst_error!(
                            CAT,
                            obj: element,
                            "Failed to connect input stream {} for consumer {}: {}",
                            webrtc_pad.stream_name,
                            peer_id,
                            err
                        );
                        remove = true;
                        break;
                    }
                } else {
                    gst_error!(
                        CAT,
                        obj: element,
                        "No producer to connect consumer {} to",
                        peer_id,
                    );
                    remove = true;
                    break;
                }
            }
        }

        drop(state);

        if remove {
            let _ = self.remove_consumer(element, &peer_id, true);
        }
    }

    /// Called by the signaller with an ice candidate
    pub fn handle_ice(
        &self,
        _element: &super::WebRTCSink,
        peer_id: &str,
        sdp_mline_index: Option<u32>,
        _sdp_mid: Option<String>,
        candidate: &str,
    ) -> Result<(), Error> {
        let state = self.state.lock().unwrap();

        let sdp_mline_index = sdp_mline_index
            .ok_or_else(|| anyhow!("SDP mline index is not optional at this time"))?;

        if let Some(consumer) = state.consumers.get(peer_id) {
            gst_trace!(CAT, "adding ice candidate for peer {}", peer_id);
            consumer
                .webrtcbin
                .emit_by_name(
                    "add-ice-candidate",
                    &[&sdp_mline_index, &candidate.to_string()],
                )
                .unwrap();
            Ok(())
        } else {
            Err(anyhow!("No consumer with ID {}", peer_id))
        }
    }

    /// Called by the signaller with an answer to our offer
    pub fn handle_sdp(
        &self,
        element: &super::WebRTCSink,
        peer_id: &str,
        desc: &gst_webrtc::WebRTCSessionDescription,
    ) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();

        if let Some(consumer) = state.consumers.get_mut(peer_id) {
            let sdp = desc.sdp();

            for webrtc_pad in consumer.webrtc_pads.values_mut() {
                let media_idx = webrtc_pad.media_idx;
                /* TODO: support partial answer, webrtcbin doesn't seem
                 * very well equipped to deal with this at the moment */
                if let Some(media) = sdp.media(media_idx) {
                    if media.attribute_val("inactive").is_some() {
                        gst_warning!(CAT, "consumer {} refused media {}", peer_id, media_idx);
                        state.remove_consumer(element, peer_id, true);
                        return Err(anyhow!("consumer {} refused media {}", peer_id, media_idx));
                    }
                }

                if let Some(payload) = sdp
                    .media(webrtc_pad.media_idx)
                    .and_then(|media| media.format(0))
                    .and_then(|format| format.parse::<i32>().ok())
                {
                    webrtc_pad.payload = Some(payload);
                } else {
                    gst_warning!(
                        CAT,
                        "consumer {} did not provide valid payload for media index {}",
                        peer_id,
                        media_idx
                    );
                    state.remove_consumer(element, peer_id, true);
                    return Err(anyhow!(
                        "consumer {} did not provide valid payload for media index {}",
                        peer_id,
                        media_idx
                    ));
                }
            }

            let element = element.downgrade();
            let peer_id = peer_id.to_string();

            let promise = gst::Promise::with_change_func(move |reply| {
                gst_debug!(CAT, "received reply {:?}", reply);
                if let Some(element) = element.upgrade() {
                    let this = Self::from_instance(&element);

                    this.on_remote_description_set(&element, peer_id);
                }
            });

            consumer
                .webrtcbin
                .emit_by_name("set-remote-description", &[desc, &promise])
                .unwrap();

            Ok(())
        } else {
            Err(anyhow!("No consumer with ID {}", peer_id))
        }
    }

    async fn run_discovery_pipeline(
        _element: &super::WebRTCSink,
        codec: &Codec,
        caps: &gst::Caps,
    ) -> Result<gst::Structure, Error> {
        let pipe = PipelineWrapper(gst::Pipeline::new(None));

        let src = match codec.is_video {
            true => make_element("videotestsrc", None)?,
            false => make_element("audiotestsrc", None)?,
        };
        let capsfilter = make_element("capsfilter", None)?;

        pipe.0.add_many(&[&src, &capsfilter]).unwrap();
        src.link(&capsfilter)
            .with_context(|| format!("Running discovery pipeline for caps {}", caps))?;

        let pay = setup_encoding(&pipe.0, &capsfilter, codec, None)?;

        let sink = make_element("fakesink", None)?;

        pipe.0.add(&sink).unwrap();

        pay.link(&sink)
            .with_context(|| format!("Running discovery pipeline for caps {}", caps))?;

        capsfilter.set_property("caps", caps).unwrap();

        src.set_property("num-buffers", 1).unwrap();

        let mut stream = pipe.0.bus().unwrap().stream();

        pipe.0
            .set_state(gst::State::Playing)
            .with_context(|| format!("Running discovery pipeline for caps {}", caps))?;

        while let Some(msg) = stream.next().await {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    return Err(err.error().into());
                }
                gst::MessageView::Eos(_) => {
                    let caps = pay.static_pad("src").unwrap().current_caps().unwrap();

                    if let Some(s) = caps.structure(0) {
                        let mut s = s.to_owned();
                        s.remove_fields(&[
                            "timestamp-offset",
                            "seqnum-offset",
                            "ssrc",
                            "sprop-parameter-sets",
                        ]);
                        s.set("payload", codec.payload);
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

        let is_video = match sink_caps.structure(0).unwrap().name() {
            "video/x-raw" => true,
            "audio/x-raw" => false,
            _ => unreachable!(),
        };

        let mut payloader_caps = gst::Caps::new_empty();
        let payloader_caps_mut = payloader_caps.make_mut();

        let futs = codecs
            .iter()
            .filter(|(_, codec)| codec.is_video == is_video)
            .map(|(_, codec)| WebRTCSink::run_discovery_pipeline(element, &codec, &sink_caps));

        for ret in futures::future::join_all(futs).await {
            match ret {
                Ok(s) => {
                    payloader_caps_mut.append_structure(s);
                }
                Err(err) => {
                    /* We don't consider this fatal, as long as we end up with one
                     * potential codec for each input stream
                     */
                    gst_warning!(
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

    fn sink_event(&self, pad: &gst::Pad, element: &super::WebRTCSink, event: gst::Event) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::Caps(e) => {
                if let Some(caps) = pad.current_caps() {
                    if caps.is_strictly_equal(e.caps()) {
                        // Nothing changed
                        true
                    } else {
                        gst_error!(CAT, obj: pad, "Renegotiation is not supported");
                        false
                    }
                } else {
                    gst_info!(CAT, obj: pad, "Received caps event {:?}", e);

                    let mut all_pads_have_caps = true;

                    self.state
                        .lock()
                        .unwrap()
                        .streams
                        .iter_mut()
                        .for_each(|(_, mut stream)| {
                            if stream.sink_pad.upcast_ref::<gst::Pad>() == pad {
                                stream.in_caps = Some(e.caps().to_owned());
                            } else {
                                if stream.in_caps.is_none() {
                                    all_pads_have_caps = false;
                                }
                            }
                        });

                    if all_pads_have_caps {
                        let element_clone = element.downgrade();
                        task::spawn(async move {
                            if let Some(element) = element_clone.upgrade() {
                                let this = Self::from_instance(&element);
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
                                        gst_error!(CAT, obj: &element, "error: {}", err);
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

                    pad.event_default(Some(element), event)
                }
            }
            _ => pad.event_default(Some(element), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WebRTCSink {
    const NAME: &'static str = "RsWebRTCSink";
    type Type = super::WebRTCSink;
    type ParentType = gst::Bin;
    type Interfaces = (gst::ChildProxy,);
}

impl ObjectImpl for WebRTCSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_boxed(
                    "video-caps",
                    "Video encoder caps",
                    "Governs what video codecs will be proposed",
                    gst::Caps::static_type(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpec::new_boxed(
                    "audio-caps",
                    "Audio encoder caps",
                    "Governs what audio codecs will be proposed",
                    gst::Caps::static_type(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "video-caps" => {
                let mut settings = self.settings.lock().unwrap();
                settings.video_caps = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| gst::Caps::new_empty());
            }
            "audio-caps" => {
                let mut settings = self.settings.lock().unwrap();
                settings.audio_caps = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| gst::Caps::new_empty());
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "video-caps" => {
                let settings = self.settings.lock().unwrap();
                settings.video_caps.to_value()
            }
            "audio-caps" => {
                let settings = self.settings.lock().unwrap();
                settings.audio_caps.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

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
            let caps = gst::Caps::builder("video/x-raw").build();
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
        element: &Self::Type,
        templ: &gst::PadTemplate,
        _name: Option<String>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        if element.current_state() > gst::State::Ready {
            gst_error!(CAT, "element pads can only be requested before starting");
            return None;
        }

        let mut state = self.state.lock().unwrap();

        let name = if templ.name().starts_with("video_") {
            let name = format!("video_{}", state.video_serial);
            state.video_serial += 1;
            name
        } else {
            let name = format!("audio_{}", state.audio_serial);
            state.audio_serial += 1;
            name
        };

        let sink_pad = gst::GhostPad::builder_with_template(&templ, Some(name.as_str()))
            .event_function(|pad, parent, event| {
                WebRTCSink::catch_panic_pad_function(
                    parent,
                    || false,
                    |sink, element| sink.sink_event(pad.upcast_ref(), element, event),
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
            },
        );

        Some(sink_pad.upcast())
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if let gst::StateChange::ReadyToPaused = transition {
            if let Err(err) = self.prepare(element) {
                gst::element_error!(
                    element,
                    gst::StreamError::Failed,
                    ["Failed to prepare: {}", err]
                );
                return Err(gst::StateChangeError);
            }
        }

        let mut ret = self.parent_change_state(element, transition);

        match transition {
            gst::StateChange::PausedToReady => {
                if let Err(err) = self.unprepare(element) {
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
                state.maybe_start_signaller(element);
            }
            _ => (),
        }

        ret
    }
}

impl BinImpl for WebRTCSink {}

impl ChildProxyImpl for WebRTCSink {
    fn child_by_index(&self, _object: &Self::Type, _index: u32) -> Option<glib::Object> {
        None
    }

    fn children_count(&self, _object: &Self::Type) -> u32 {
        0
    }

    fn child_by_name(&self, _object: &Self::Type, name: &str) -> Option<glib::Object> {
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
