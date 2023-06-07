use std::{
    collections::{BTreeMap, HashMap},
    ops::Deref,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Error};
use gst::{glib, prelude::*};
use once_cell::sync::Lazy;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtcutils",
        gst::DebugColorFlags::empty(),
        Some("WebRTC Utils"),
    )
});

pub fn gvalue_to_json(val: &gst::glib::Value) -> Option<serde_json::Value> {
    match val.type_() {
        glib::Type::STRING => Some(val.get::<String>().unwrap().into()),
        glib::Type::BOOL => Some(val.get::<bool>().unwrap().into()),
        glib::Type::I32 => Some(val.get::<i32>().unwrap().into()),
        glib::Type::U32 => Some(val.get::<u32>().unwrap().into()),
        glib::Type::I_LONG | glib::Type::I64 => Some(val.get::<i64>().unwrap().into()),
        glib::Type::U_LONG | glib::Type::U64 => Some(val.get::<u64>().unwrap().into()),
        glib::Type::F32 => Some(val.get::<f32>().unwrap().into()),
        glib::Type::F64 => Some(val.get::<f64>().unwrap().into()),
        _ => {
            if let Ok(s) = val.get::<gst::Structure>() {
                serde_json::to_value(
                    s.iter()
                        .filter_map(|(name, value)| {
                            gvalue_to_json(value).map(|value| (name.to_string(), value))
                        })
                        .collect::<HashMap<String, serde_json::Value>>(),
                )
                .ok()
            } else if let Ok(a) = val.get::<gst::Array>() {
                serde_json::to_value(
                    a.iter()
                        .filter_map(|value| gvalue_to_json(value))
                        .collect::<Vec<serde_json::Value>>(),
                )
                .ok()
            } else if let Some((_klass, values)) = gst::glib::FlagsValue::from_value(val) {
                Some(
                    values
                        .iter()
                        .map(|value| value.nick())
                        .collect::<Vec<&str>>()
                        .join("+")
                        .into(),
                )
            } else if let Ok(value) = val.serialize() {
                Some(value.as_str().into())
            } else {
                None
            }
        }
    }
}

fn json_to_gststructure(val: &serde_json::Value) -> Option<glib::SendValue> {
    match val {
        serde_json::Value::Bool(v) => Some(v.to_send_value()),
        serde_json::Value::Number(n) => {
            if n.is_u64() {
                Some(n.as_u64().unwrap().to_send_value())
            } else if n.is_i64() {
                Some(n.as_i64().unwrap().to_send_value())
            } else if n.is_f64() {
                Some(n.as_f64().unwrap().to_send_value())
            } else {
                todo!("Unhandled case {n:?}");
            }
        }
        serde_json::Value::String(v) => Some(v.to_send_value()),
        serde_json::Value::Array(v) => {
            let array = v
                .iter()
                .filter_map(json_to_gststructure)
                .collect::<Vec<glib::SendValue>>();
            Some(gst::Array::from_values(array).to_send_value())
        }
        serde_json::Value::Object(v) => Some(serialize_json_object(v).to_send_value()),
        _ => None,
    }
}

pub fn serialize_json_object(val: &serde_json::Map<String, serde_json::Value>) -> gst::Structure {
    let mut res = gst::Structure::new_empty("v");

    val.iter().for_each(|(k, v)| {
        if let Some(gvalue) = json_to_gststructure(v) {
            res.set_value(k, gvalue);
        }
    });

    res
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

#[derive(Debug)]
struct DecodingInfo {
    has_decoder: AtomicBool,
}

impl Clone for DecodingInfo {
    fn clone(&self) -> Self {
        Self {
            has_decoder: AtomicBool::new(self.has_decoder.load(Ordering::SeqCst)),
        }
    }
}

#[derive(Clone, Debug)]
struct EncodingInfo {
    encoder: gst::ElementFactory,
    payloader: gst::ElementFactory,
    output_filter: Option<gst::Caps>,
}

#[derive(Clone, Debug)]
pub struct Codec {
    pub name: String,
    pub caps: gst::Caps,
    pub stream_type: gst::StreamType,

    payload_type: Option<i32>,
    decoding_info: Option<DecodingInfo>,
    encoding_info: Option<EncodingInfo>,
}

impl Codec {
    pub fn new(
        name: &str,
        stream_type: gst::StreamType,
        caps: &gst::Caps,
        decoders: &glib::List<gst::ElementFactory>,
        encoders: &glib::List<gst::ElementFactory>,
        payloaders: &glib::List<gst::ElementFactory>,
    ) -> Self {
        let has_decoder = Self::has_decoder_for_caps(caps, decoders);
        let encoder = Self::get_encoder_for_caps(caps, encoders);
        let payloader = Self::get_payloader_for_codec(name, payloaders);

        let encoding_info = if let (Some(encoder), Some(payloader)) = (encoder, payloader) {
            Some(EncodingInfo {
                encoder,
                payloader,
                output_filter: None,
            })
        } else {
            None
        };

        Self {
            caps: caps.clone(),
            stream_type,
            name: name.into(),
            payload_type: None,

            decoding_info: Some(DecodingInfo {
                has_decoder: AtomicBool::new(has_decoder),
            }),

            encoding_info,
        }
    }

    pub fn can_encode(&self) -> bool {
        self.encoding_info.is_some()
    }

    pub fn set_pt(&mut self, pt: i32) {
        self.payload_type = Some(pt);
    }

    pub fn new_decoding(
        name: &str,
        stream_type: gst::StreamType,
        caps: &gst::Caps,
        decoders: &glib::List<gst::ElementFactory>,
    ) -> Self {
        let has_decoder = Self::has_decoder_for_caps(caps, decoders);

        Self {
            caps: caps.clone(),
            stream_type,
            name: name.into(),
            payload_type: None,

            decoding_info: Some(DecodingInfo {
                has_decoder: AtomicBool::new(has_decoder),
            }),

            encoding_info: None,
        }
    }

    pub fn has_decoder(&self) -> bool {
        if self.decoding_info.is_none() {
            return false;
        }

        let decoder_info = self.decoding_info.as_ref().unwrap();
        if decoder_info.has_decoder.load(Ordering::SeqCst) {
            true
        } else if Self::has_decoder_for_caps(
            &self.caps,
            // Replicating decodebin logic
            &gst::ElementFactory::factories_with_type(
                gst::ElementFactoryType::DECODER,
                gst::Rank::Marginal,
            ),
        ) {
            // Check if new decoders have been installed meanwhile
            decoder_info.has_decoder.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    fn get_encoder_for_caps(
        caps: &gst::Caps,
        decoders: &glib::List<gst::ElementFactory>,
    ) -> Option<gst::ElementFactory> {
        decoders
            .iter()
            .find(|factory| {
                factory.static_pad_templates().iter().any(|template| {
                    let template_caps = template.caps();
                    template.direction() == gst::PadDirection::Src
                        && !template_caps.is_any()
                        && caps.can_intersect(&template_caps)
                })
            })
            .cloned()
    }

    fn get_payloader_for_codec(
        codec: &str,
        payloaders: &glib::List<gst::ElementFactory>,
    ) -> Option<gst::ElementFactory> {
        payloaders
            .iter()
            .find(|factory| {
                factory.static_pad_templates().iter().any(|template| {
                    let template_caps = template.caps();

                    if template.direction() != gst::PadDirection::Src || template_caps.is_any() {
                        return false;
                    }

                    template_caps.iter().any(|s| {
                        s.has_field("encoding-name")
                            && s.get::<gst::List>("encoding-name").map_or_else(
                                |_| {
                                    if let Ok(encoding_name) = s.get::<&str>("encoding-name") {
                                        encoding_name == codec
                                    } else {
                                        false
                                    }
                                },
                                |encoding_names| {
                                    encoding_names.iter().any(|v| {
                                        v.get::<&str>()
                                            .map_or(false, |encoding_name| encoding_name == codec)
                                    })
                                },
                            )
                    })
                })
            })
            .cloned()
    }

    fn has_decoder_for_caps(caps: &gst::Caps, decoders: &glib::List<gst::ElementFactory>) -> bool {
        decoders.iter().any(|factory| {
            factory.static_pad_templates().iter().any(|template| {
                let template_caps = template.caps();
                template.direction() == gst::PadDirection::Sink
                    && !template_caps.is_any()
                    && caps.can_intersect(&template_caps)
            })
        })
    }

    pub fn is_video(&self) -> bool {
        matches!(self.stream_type, gst::StreamType::VIDEO)
    }

    pub fn payload(&self) -> Option<i32> {
        self.payload_type
    }

    pub fn build_encoder(&self) -> Option<Result<gst::Element, Error>> {
        self.encoding_info.as_ref().map(|info| {
            info.encoder
                .create()
                .build()
                .with_context(|| format!("Creating payloader {}", info.encoder.name()))
        })
    }

    pub fn build_payloader(&self, pt: u32) -> Option<gst::Element> {
        self.encoding_info.as_ref().map(|info| {
            let mut res = info
                .payloader
                .create()
                .property("mtu", 1200_u32)
                .property("pt", pt);

            if ["vp8enc", "vp9enc"].contains(&self.encoder_name().unwrap().as_str()) {
                res = res.property_from_str("picture-id-mode", "15-bit");
            }

            res.build().unwrap()
        })
    }

    pub fn raw_converter_filter(&self) -> Result<gst::Element, Error> {
        let caps = if self.is_video() {
            let mut structure_builder = gst::Structure::builder("video/x-raw")
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1));

            if self
                .encoder_name()
                .map(|e| e.as_str() == "nvh264enc")
                .unwrap_or(false)
            {
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

        gst::ElementFactory::make("capsfilter")
            .property("caps", &caps)
            .build()
            .with_context(|| "Creating capsfilter caps")
    }

    pub fn encoder_factory(&self) -> Option<gst::ElementFactory> {
        self.encoding_info.as_ref().map(|info| info.encoder.clone())
    }

    pub fn encoder_name(&self) -> Option<String> {
        self.encoding_info
            .as_ref()
            .map(|info| info.encoder.name().to_string())
    }

    pub fn set_output_filter(&mut self, caps: gst::Caps) {
        if let Some(info) = self.encoding_info.as_mut() {
            info.output_filter = Some(caps);
        }
    }

    pub fn output_filter(&self) -> Option<gst::Caps> {
        self.encoding_info
            .as_ref()
            .and_then(|info| info.output_filter.clone())
    }

    pub fn build_parser(&self) -> Result<Option<gst::Element>, Error> {
        match self.name.as_str() {
            "H264" => make_element("h264parse", None),
            "H265" => make_element("h265parse", None),
            _ => return Ok(None),
        }
        .map(Some)
    }

    pub fn parser_caps(&self, force_profile: bool) -> gst::Caps {
        let codec_caps_name = self.caps.structure(0).unwrap().name();
        match self.name.as_str() {
            "H264" => {
                if force_profile {
                    gst::debug!(
                        CAT,
                        "No H264 profile requested, selecting constrained-baseline"
                    );

                    gst::Caps::builder(codec_caps_name)
                        .field("stream-format", "avc")
                        .field("profile", "constrained-baseline")
                        .build()
                } else {
                    gst::Caps::builder(codec_caps_name)
                        .field("stream-format", "avc")
                        .build()
                }
            }
            "H265" => gst::Caps::new_empty_simple("video/x-h265"),
            _ => gst::Caps::new_any(),
        }
    }
}

pub static AUDIO_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::new_empty_simple("audio/x-raw"));
pub static OPUS_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::new_empty_simple("audio/x-opus"));

pub static VIDEO_CAPS: Lazy<gst::Caps> = Lazy::new(|| {
    gst::Caps::builder_full_with_any_features()
        .structure(gst::Structure::new_empty("video/x-raw"))
        .build()
});
pub static VP8_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::new_empty_simple("video/x-vp8"));
pub static VP9_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::new_empty_simple("video/x-vp9"));
pub static H264_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::new_empty_simple("video/x-h264"));
pub static H265_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::new_empty_simple("video/x-h265"));

pub static RTP_CAPS: Lazy<gst::Caps> =
    Lazy::new(|| gst::Caps::new_empty_simple("application/x-rtp"));

#[derive(Debug, Clone)]
pub struct Codecs(Vec<Codec>);

impl Deref for Codecs {
    type Target = Vec<Codec>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Codecs {
    pub fn to_map(&self) -> BTreeMap<i32, Codec> {
        self.0
            .iter()
            .map(|codec| (codec.payload().unwrap(), codec.clone()))
            .collect()
    }

    pub fn from_map(codecs: &BTreeMap<i32, Codec>) -> Self {
        Self(codecs.values().cloned().collect())
    }

    pub fn find_for_encoded_caps(&self, caps: &gst::Caps) -> Option<Codec> {
        self.iter()
            .find(|codec| codec.caps.can_intersect(caps) && codec.encoding_info.is_some())
            .cloned()
    }
}

static CODECS: Lazy<Codecs> = Lazy::new(|| {
    let decoders = gst::ElementFactory::factories_with_type(
        gst::ElementFactoryType::DECODER,
        gst::Rank::Marginal,
    );

    let encoders = gst::ElementFactory::factories_with_type(
        gst::ElementFactoryType::ENCODER,
        gst::Rank::Marginal,
    );

    let payloaders = gst::ElementFactory::factories_with_type(
        gst::ElementFactoryType::PAYLOADER,
        gst::Rank::Marginal,
    );

    Codecs(vec![
        Codec::new(
            "OPUS",
            gst::StreamType::AUDIO,
            &OPUS_CAPS,
            &decoders,
            &encoders,
            &payloaders,
        ),
        Codec::new(
            "VP8",
            gst::StreamType::VIDEO,
            &VP8_CAPS,
            &decoders,
            &encoders,
            &payloaders,
        ),
        Codec::new(
            "H264",
            gst::StreamType::VIDEO,
            &H264_CAPS,
            &decoders,
            &encoders,
            &payloaders,
        ),
        Codec::new(
            "VP9",
            gst::StreamType::VIDEO,
            &VP9_CAPS,
            &decoders,
            &encoders,
            &payloaders,
        ),
        Codec::new(
            "H265",
            gst::StreamType::VIDEO,
            &H265_CAPS,
            &decoders,
            &encoders,
            &payloaders,
        ),
    ])
});

impl Codecs {
    pub fn find(encoding_name: &str) -> Option<Codec> {
        CODECS
            .iter()
            .find(|codec| codec.name == encoding_name)
            .cloned()
    }

    pub fn video_codecs() -> Vec<Codec> {
        CODECS
            .iter()
            .filter(|codec| codec.stream_type == gst::StreamType::VIDEO)
            .cloned()
            .collect()
    }

    pub fn audio_codecs() -> Vec<Codec> {
        CODECS
            .iter()
            .filter(|codec| codec.stream_type == gst::StreamType::AUDIO)
            .cloned()
            .collect()
    }

    pub fn video_codec_names() -> Vec<String> {
        CODECS
            .iter()
            .filter(|codec| codec.stream_type == gst::StreamType::VIDEO)
            .map(|codec| codec.name.clone())
            .collect()
    }

    pub fn audio_codec_names() -> Vec<String> {
        CODECS
            .iter()
            .filter(|codec| codec.stream_type == gst::StreamType::AUDIO)
            .map(|codec| codec.name.clone())
            .collect()
    }

    /// List all codecs that can be used for encoding the given caps and assign
    /// a payload type to each of them. This is useful to initiate SDP negotiation.
    pub fn list_encoders<'a>(caps: impl IntoIterator<Item = &'a gst::StructureRef>) -> Codecs {
        let mut payload = 96..128;

        Codecs(
            caps.into_iter()
                .filter_map(move |s| {
                    let caps = gst::Caps::builder_full().structure(s.to_owned()).build();

                    CODECS
                        .iter()
                        .find(|codec| {
                            codec
                                .encoding_info
                                .as_ref()
                                .map_or(false, |_| codec.caps.can_intersect(&caps))
                        })
                        .and_then(|codec| {
                            /* Assign a payload type to the codec */
                            if let Some(pt) = payload.next() {
                                let mut codec = codec.clone();

                                codec.payload_type = Some(pt);

                                Some(codec)
                            } else {
                                gst::warning!(
                                    CAT,
                                    "Too many formats for available payload type range, ignoring {}",
                                    s
                                );
                                None
                            }
                        })
                })
                .collect()
        )
    }
}

pub fn is_raw_caps(caps: &gst::Caps) -> bool {
    assert!(caps.is_fixed());
    ["video/x-raw", "audio/x-raw"].contains(&caps.structure(0).unwrap().name().as_str())
}

pub fn cleanup_codec_caps(mut caps: gst::Caps) -> gst::Caps {
    assert!(caps.is_fixed());

    if let Some(s) = caps.make_mut().structure_mut(0) {
        if ["video/x-h264", "video/x-h265"].contains(&s.name().as_str()) {
            s.remove_fields(["codec_data"]);
        } else if ["video/x-vp8", "video/x-vp9"].contains(&s.name().as_str()) {
            s.remove_fields(["profile"]);
        } else if s.name() == "audio/x-opus" {
            s.remove_fields(["streamheader"]);
        }
    }

    caps
}
