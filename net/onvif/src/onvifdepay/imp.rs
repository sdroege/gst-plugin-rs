use gst::glib;
use gst::subclass::prelude::*;
use gst_rtp::subclass::prelude::*;
use once_cell::sync::Lazy;
use std::sync::Mutex;

#[derive(Default)]
struct State {
    // Aggregate payloads to form a complete XML document
    adapter: gst_base::UniqueAdapter,
}

#[derive(Default)]
pub struct OnvifDepay {
    state: Mutex<State>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "onvifdepay",
        gst::DebugColorFlags::empty(),
        Some("ONVIF metadata depayloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for OnvifDepay {
    const NAME: &'static str = "GstOnvifDepay";
    type Type = super::OnvifDepay;
    type ParentType = gst_rtp::RTPBaseDepayload;
}

impl ObjectImpl for OnvifDepay {}

impl GstObjectImpl for OnvifDepay {}

impl ElementImpl for OnvifDepay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "ONVIF metadata RTP depayloader",
                "Depayloader/Network/RTP",
                "ONVIF metadata RTP depayloader",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_caps = gst::Caps::builder("application/x-rtp")
                .field("media", "application")
                .field("payload", gst::IntRange::new(96, 127))
                .field("clock-rate", 90000)
                .field("encoding-name", "VND.ONVIF.METADATA")
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder("application/x-onvif-metadata")
                .field("encoding", "utf8")
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl RTPBaseDepayloadImpl for OnvifDepay {
    fn process_rtp_packet(
        &self,
        element: &Self::Type,
        rtp_buffer: &gst_rtp::RTPBuffer<gst_rtp::rtp_buffer::Readable>,
    ) -> Option<gst::Buffer> {
        // Retrieve the payload subbuffer
        let payload_buffer = match rtp_buffer.payload_buffer() {
            Ok(buffer) => buffer,
            Err(..) => {
                gst::element_error!(
                    element,
                    gst::ResourceError::Read,
                    ["Failed to retrieve RTP buffer payload"]
                );

                return None;
            }
        };

        let mut state = self.state.lock().unwrap();

        if rtp_buffer
            .buffer()
            .flags()
            .contains(gst::BufferFlags::DISCONT)
        {
            gst::debug!(CAT, obj: element, "processing discont RTP buffer");
            state.adapter.clear();
        }

        // Now store in the adapter
        state.adapter.push(payload_buffer);

        if !rtp_buffer.is_marker() {
            return None;
        }

        // We have found the last chunk for this document, empty the adapter
        let available = state.adapter.available();
        let buffer = match state.adapter.take_buffer(available) {
            Ok(buffer) => buffer,
            Err(err) => {
                gst::element_error!(
                    element,
                    gst::ResourceError::Read,
                    ["Failed to empty adapter: {}", err]
                );

                return None;
            }
        };

        // Sanity check the document
        let map = buffer.map_readable().unwrap();

        let utf8 = match std::str::from_utf8(map.as_ref()) {
            Ok(s) => s,
            Err(err) => {
                gst::warning!(
                    CAT,
                    obj: element,
                    "Failed to decode payload as UTF-8: {}",
                    err
                );

                return None;
            }
        };

        let forward = {
            let mut forward = false;

            for token in xmlparser::Tokenizer::from(utf8) {
                match token {
                    Ok(token) => match token {
                        xmlparser::Token::Comment { .. } => {
                            continue;
                        }
                        xmlparser::Token::Declaration { .. } => {
                            continue;
                        }
                        xmlparser::Token::ElementStart { local, .. } => {
                            if local.as_str() == "MetadataStream" {
                                forward = true;
                            }
                            break;
                        }
                        _ => {
                            forward = false;
                            break;
                        }
                    },
                    Err(err) => {
                        gst::warning!(CAT, obj: element, "Invalid XML in payload: {}", err);

                        return None;
                    }
                }
            }

            forward
        };

        // Incomplete, wait for the next document
        if !forward {
            gst::warning!(
                CAT,
                obj: element,
                "document must start with tt:MetadataStream element",
            );

            return None;
        }

        drop(map);

        Some(buffer)
    }
}
