//
// Copyright (C) 2026 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use atomic_refcell::AtomicRefCell;
use gst::{glib, prelude::*, subclass::prelude::*};

use std::sync::{LazyLock, Mutex};

use crate::basedepay::RtpBaseDepay2Ext;

/**
 * SECTION:element-rtpg726depay2
 * @see_also: rtpg726pay2
 *
 * Extracts G726 encoded audio from RTP packets as per [RFC 3551][rfc-3551].
 *
 * [rfc-3551]: https://www.rfc-editor.org/rfc/rfc3551.html#section-4.5.4
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 udpsrc2 caps='application/x-rtp, media=audio, clock-rate=8000, payload=8' ! rtpjitterbuffer latency=50 ! rtpg726depay2 ! avenc_g726 ! audioconvert ! audioresample ! autoaudiosink
 * ]| This will depayload an incoming RTP G722 audio stream. You can use the #rtpg726pay2 and
 * avenc_g726 elements to create such an RTP stream.
 *
 * Since: plugins-rs-0.16.0
 */

#[derive(Default)]
pub struct RtpG726Depay {
    settings: Mutex<Settings>,
    state: AtomicRefCell<State>,
}

#[derive(Default)]
struct Settings {
    force_aal2: bool,
}

#[derive(Default)]
struct State {
    aal2: bool,
    bitrate: i32,
    block_align: u8,
}

const CLOCK_RATE: i32 = 8000;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpg726depay2",
        gst::DebugColorFlags::empty(),
        Some("RTP G726 Depayloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpG726Depay {
    const NAME: &'static str = "GstRtpG726Depay2";
    type Type = super::RtpG726Depay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpG726Depay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("force-aal2")
                    .nick("Force AAL2")
                    .blurb("Force AAL2 encoding for compatibility with bad depayloaders")
                    .build(),
            ]
        });
        &PROPERTIES
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "force-aal2" => self.settings.lock().unwrap().force_aal2.to_value(),
            _ => unreachable!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "force-aal2" => self.settings.lock().unwrap().force_aal2 = value.get().unwrap(),
            _ => unreachable!(),
        }
    }
}

impl GstObjectImpl for RtpG726Depay {}

impl ElementImpl for RtpG726Depay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP G.726 Depayloader",
                "Codec/Depayloader/Network/RTP",
                "Depayload G.726 from RTP packets (RFC 3551)",
                "Matthew Waters <matthew@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::builder_full()
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "audio")
                            .field("clock-rate", CLOCK_RATE)
                            .field(
                                "encoding-name",
                                gst::List::from_iter(
                                    [
                                        "G726",
                                        "G726-16",
                                        "G726-24",
                                        "G726-32",
                                        "G726-40",
                                        "AAL2-G726-16",
                                        "AAL2-G726-24",
                                        "AAL2-G726-32",
                                        "AAL2-G726-40",
                                    ]
                                    .iter()
                                    .map(|s| s.to_send_value()),
                                ),
                            )
                            .field("encoding-params", "1")
                            .build(),
                    )
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("audio/x-adpcm")
                    .field("channels", 1i32)
                    .field("rate", 8000i32)
                    .field(
                        "bitrate",
                        gst::List::from_iter(
                            [16000i32, 24000i32, 32000i32, 40000i32].map(|b| b.to_send_value()),
                        ),
                    )
                    .field(
                        "block_align",
                        gst::List::from_iter([2i32, 3, 4, 5].map(|b| b.to_send_value())),
                    )
                    .field("layout", "g726")
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basedepay::RtpBaseDepay2Impl for RtpG726Depay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["audio"];

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let s = caps.structure(0).unwrap();

        let settings = self.settings.lock().unwrap();
        let mut state = self.state.borrow_mut();

        let encoding_name = s.get::<&str>("encoding-name").unwrap_or("G726");
        if encoding_name.starts_with("AAL2-") {
            state.aal2 = true;
        } else {
            state.aal2 = settings.force_aal2;
        }
        match encoding_name {
            "AAL2-G726-16" | "G726-16" => {
                state.bitrate = 16000;
                state.block_align = 2;
            }
            "AAL2-G726-24" | "G726-24" => {
                state.bitrate = 24000;
                state.block_align = 3;
            }
            "AAL2-G726-32" | "G726-32" | "G726" => {
                state.bitrate = 32000;
                state.block_align = 4;
            }
            "AAL2-G726-40" | "G726-40" => {
                state.bitrate = 40000;
                state.block_align = 5;
            }
            name => {
                gst::error!(CAT, "Unknown encoding-name {name}");
                return false;
            }
        }

        let src_caps = gst::Caps::builder("audio/x-adpcm")
            .field("channels", 1i32)
            .field("rate", CLOCK_RATE)
            .field("layout", "g726")
            .field("bitrate", state.bitrate)
            .field("block_align", state.block_align as i32)
            .build();

        self.obj().set_src_caps(&src_caps);

        true
    }

    fn handle_packet(
        &self,
        packet: &crate::basedepay::Packet,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut buffer = packet.payload_buffer();

        let state = self.state.borrow();

        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_duration(
            (buffer_ref.size() as u64)
                .mul_div_floor(*gst::ClockTime::SECOND * 8, state.bitrate as u64)
                .map(gst::ClockTime::from_nseconds),
        );

        if !state.aal2 {
            let Ok(mut map) = buffer_ref.map_writable() else {
                gst::element_error!(
                    self.obj(),
                    gst::StreamError::Failed,
                    ["Failed to map input buffer writable"]
                );
                return Err(gst::FlowError::Error);
            };

            crate::g72x::utils::g726_code_swap(map.as_mut_slice(), false, state.bitrate);
        }

        // mark start of talkspurt with RESYNC
        if packet.marker_bit() {
            buffer_ref.set_flags(gst::BufferFlags::RESYNC);
        }

        gst::trace!(CAT, imp = self, "Finishing buffer {buffer:?}");

        self.obj().queue_buffer(packet.into(), buffer)
    }
}
