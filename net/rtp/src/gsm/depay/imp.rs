// GStreamer RTP GSM Audio Depayloader
//
// Copyright (C) 2026 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpgsmdepay2
 * @see_also: rtpgsmpay2, rtpgsmdepay, rtpgsmpay, avdec_gsm, gsmenc, gsmdec
 *
 * Depayload a GSM Audio Stream from RTP packets as per [RFC 3551][rfc-3551].
 *
 * [rfc-3551]: https://datatracker.ietf.org/doc/html/rfc3551#section-4.5.8
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 udpsrc2 caps='application/x-rtp,media=audio,clock-rate=8000,encoding-name=GSM,payload=96' ! rtpjitterbuffer latency=250 ! rtpgsmdepay2 ! decodebin3 ! audioconvert ! audioresample ! autoaudiosink
 * ]| This will depayload an incoming RTP GSM audio stream and decode it and play it.
 * You can use the `rtpgsmpay2` element with `gsmenc` to create such an RTP stream.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::{glib, subclass::prelude::*};

use std::sync::LazyLock;

use crate::basedepay::{Packet, RtpBaseDepay2Ext, RtpBaseDepay2Impl};

#[derive(Default)]
pub struct RtpGsmDepay;

const CLOCK_RATE: i32 = 8000;
const FRAME_SIZE: usize = 33;
const FRAME_DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(20);

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpgsmdepay2",
        gst::DebugColorFlags::empty(),
        Some("RTP GSM Audio Depayloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpGsmDepay {
    const NAME: &'static str = "GstRtpGsmDepay";
    type Type = super::RtpGsmDepay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpGsmDepay {}

impl GstObjectImpl for RtpGsmDepay {}

impl ElementImpl for RtpGsmDepay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP GSM Audio Depayloader",
                "Codec/Depayloader/Network/RTP",
                "Depayload a GSM Audio Stream from RTP packets (RFC 3551)",
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
                            .field("payload", 3i32)
                            .field("clock-rate", CLOCK_RATE)
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "audio")
                            .field("encoding-name", "GSM")
                            .field("clock-rate", CLOCK_RATE)
                            .build(),
                    )
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("audio/x-gsm")
                    .field("channels", 1i32)
                    .field("rate", CLOCK_RATE)
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl RtpBaseDepay2Impl for RtpGsmDepay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["audio"];

    fn set_sink_caps(&self, _caps: &gst::Caps) -> bool {
        let src_caps = gst::Caps::builder("audio/x-gsm")
            .field("channels", 1i32)
            .field("rate", CLOCK_RATE)
            .build();

        self.obj().set_src_caps(&src_caps);

        true
    }

    fn handle_packet(&self, packet: &Packet) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut buffer = packet.payload_buffer();

        // TODO: validate buffer size is a multiple of 33 bytes?
        let n_frames = buffer.size() / FRAME_SIZE;

        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_duration(FRAME_DURATION * n_frames as u64);

        // mark start of talkspurt with RESYNC
        if packet.marker_bit() {
            buffer_ref.set_flags(gst::BufferFlags::RESYNC);
        }

        gst::trace!(CAT, imp = self, "Finishing buffer {buffer:?}");

        self.obj().queue_buffer(packet.into(), buffer)
    }
}
