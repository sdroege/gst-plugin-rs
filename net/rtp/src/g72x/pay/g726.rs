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

use crate::{
    baseaudiopay::RtpBaseAudioPay2Ext,
    basepay::{RtpBasePay2Ext, RtpBasePay2ImplExt},
};

/**
 * SECTION:element-rtpg7262
 * @see_also: rtpg726depay2
 *
 * Payloads G.726 encoded audio into RTP packets as per [RFC 3551][rfc-3551].
 *
 * [rfc-3551]: https://www.rfc-editor.org/rfc/rfc3551.html#section-4.5.4
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 audiotestsrc wave=ticks ! audio/x-raw,rate=8000,channels=1 ! avenc_g726 ! rtpg726pay2 ! udpsink2 host=127.0.0.1 port=5004
 * ]| This will generate an G.726 audio test signal and payload it as RTP and send it out
 * as UDP to localhost port 5004.
 *
 * Since: plugins-rs-0.16.0
 */

#[derive(Default)]
struct Settings {
    force_aal2: bool,
}

#[derive(Default)]
struct State {
    aal2: bool,
    bitrate: i32,
}

#[derive(Default)]
pub struct RtpG726Pay {
    settings: Mutex<Settings>,
    state: AtomicRefCell<State>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpg726pay2",
        gst::DebugColorFlags::empty(),
        Some("RTP G726 Payloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpG726Pay {
    const NAME: &'static str = "GstRtpG726Pay2";
    type Type = super::RtpG726Pay;
    type ParentType = crate::baseaudiopay::RtpBaseAudioPay2;
}

impl ObjectImpl for RtpG726Pay {
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

impl GstObjectImpl for RtpG726Pay {}

impl ElementImpl for RtpG726Pay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP G.726 Payloader",
                "Codec/Payloader/Network/RTP",
                "Payload G.726 Audio into RTP packets (RFC 3551)",
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
                &gst::Caps::builder("audio/x-adpcm")
                    .field("channels", 1i32)
                    .field("rate", 8000i32)
                    .field(
                        "bitrate",
                        gst::List::from_iter(
                            [16000i32, 24000i32, 32000i32, 40000i32].map(|b| b.to_send_value()),
                        ),
                    )
                    .field("layout", "g726")
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder_full()
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "audio")
                            .field(
                                "encoding-name",
                                gst::List::from_iter(
                                    [
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
                            .field("clock-rate", 8000i32)
                            .build(),
                    )
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basepay::RtpBasePay2Impl for RtpG726Pay {
    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let s = caps.structure(0).unwrap();

        let bitrate = s.get::<i32>("bitrate").unwrap_or(32000);

        let Some(name) = encoding_name_for_bitrate(bitrate) else {
            gst::error!(CAT, "Invalid bitrate {bitrate} specified");
            return false;
        };
        let s = gst::Structure::builder("application/x-rtp")
            .field("media", "audio")
            .field("clock-rate", 8000i32)
            .field("encoding-name", name)
            .build();

        let peer_caps = self.obj().src_pad().query_caps(None);
        let settings = self.settings.lock().unwrap();
        let mut state = self.state.borrow_mut();
        let src_caps = if peer_caps.is_any() {
            state.aal2 = settings.force_aal2;
            gst::Caps::builder_full().structure(s).build()
        } else {
            let mut aal2_s = s.clone();
            aal2_s.set("encoding-name", format!("AAL2-{name}"));
            let possible_caps = gst::Caps::builder_full()
                .structure(s.clone())
                .structure(aal2_s.clone())
                .build();
            let chosen_caps = peer_caps.intersect(&possible_caps);
            if chosen_caps.is_empty() {
                gst::error!(CAT, "Could not negotiate format");
                return false;
            }
            let encoding_name = chosen_caps
                .structure(0)
                .unwrap()
                .get::<&str>("encoding-name")
                .unwrap();
            if encoding_name.starts_with("AAL2-") {
                state.aal2 = true;
                gst::Caps::builder_full().structure(aal2_s).build()
            } else {
                state.aal2 = settings.force_aal2;
                gst::Caps::builder_full().structure(s).build()
            }
        };
        drop(settings);
        state.bitrate = bitrate;
        drop(state);

        self.obj().set_src_caps(&src_caps);
        self.obj().set_bits_per_frame((bitrate / 8000) as usize);

        true
    }

    #[allow(clippy::single_match)]
    fn sink_query(&self, query: &mut gst::QueryRef) -> bool {
        match query.view_mut() {
            gst::QueryViewMut::Caps(query) => {
                let mut caps = self.obj().sink_pad().pad_template_caps();

                if let Some(filter) = query.filter() {
                    caps = filter.intersect_with_mode(&caps, gst::CapsIntersectMode::First);
                }

                query.set_result(&caps);

                return true;
            }
            _ => (),
        }

        self.parent_sink_query(query)
    }

    fn handle_buffer(
        &self,
        buffer: &gst::Buffer,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let state = self.state.borrow_mut();
        if !state.aal2 {
            // can be done in place if we were provided a mutable reference.
            let mut copy = buffer.copy();
            {
                let copy = copy.make_mut();
                let mut map = copy.map_writable().unwrap();

                crate::g72x::utils::g726_code_swap(map.as_mut_slice(), true, state.bitrate);
            }
            self.parent_handle_buffer(&copy, id)
        } else {
            self.parent_handle_buffer(buffer, id)
        }
    }
}

impl crate::baseaudiopay::RtpBaseAudioPay2Impl for RtpG726Pay {}

struct BitrateEncoding {
    bitrate: i32,
    encoding_name: &'static str,
}

static BITRATE_ENCODING_TABLE: [BitrateEncoding; 4] = [
    BitrateEncoding {
        bitrate: 16000,
        encoding_name: "G726-16",
    },
    BitrateEncoding {
        bitrate: 24000,
        encoding_name: "G726-24",
    },
    BitrateEncoding {
        bitrate: 32000,
        encoding_name: "G726-32",
    },
    BitrateEncoding {
        bitrate: 40000,
        encoding_name: "G726-40",
    },
];

fn encoding_name_for_bitrate(bitrate: i32) -> Option<&'static str> {
    BITRATE_ENCODING_TABLE
        .iter()
        .find(|entry| entry.bitrate == bitrate)
        .map(|entry| entry.encoding_name)
}
