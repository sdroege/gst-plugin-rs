//
// Copyright (C) 2026 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*, subclass::prelude::*};

use std::sync::LazyLock;

use crate::basedepay::RtpBaseDepay2Ext;

/**
 * SECTION:element-rtpg722depay2
 * @see_also: rtpg722pay2
 *
 * Extracts G722 encoded audio from RTP packets as per [RFC 3551][rfc-3551].
 *
 * [rfc-3551]: https://www.rfc-editor.org/rfc/rfc3551.html#section-4.5.2
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 udpsrc2 caps='application/x-rtp, media=audio, clock-rate=8000, payload=8' ! rtpjitterbuffer latency=50 ! rtpg722depay2 ! avenc_g722 ! audioconvert ! audioresample ! autoaudiosink
 * ]| This will depayload an incoming RTP G722 audio stream. You can use the #rtppcmapay2 and
 * avenc_g722 elements to create such an RTP stream.
 *
 * Since: plugins-rs-0.16.0
 */

#[derive(Default)]
pub struct RtpG722Depay;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpg722depay2",
        gst::DebugColorFlags::empty(),
        Some("RTP G722 Depayloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpG722Depay {
    const NAME: &'static str = "GstRtpG722Depay2";
    type Type = super::RtpG722Depay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpG722Depay {}

impl GstObjectImpl for RtpG722Depay {}

impl ElementImpl for RtpG722Depay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP G.722 Depayloader",
                "Codec/Depayloader/Network/RTP",
                "Depayload G.722 from RTP packets (RFC 3551)",
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
                            .field("payload", 9i32)
                            .field("clock-rate", 8000i32)
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "audio")
                            .field("clock-rate", 8000i32)
                            .field("encoding-name", "G722")
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
                &gst::Caps::builder("audio/G722")
                    .field("channels", 1i32)
                    .field("rate", 16000i32)
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basedepay::RtpBaseDepay2Impl for RtpG722Depay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["audio"];

    fn set_sink_caps(&self, _caps: &gst::Caps) -> bool {
        let clock_rate = 16000;

        let src_caps = gst::Caps::builder("audio/G722")
            .field("channels", 1i32)
            .field("rate", clock_rate)
            .build();

        self.obj().set_src_caps(&src_caps);

        true
    }

    fn handle_packet(
        &self,
        packet: &crate::basedepay::Packet,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut buffer = packet.payload_buffer();

        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_duration(
            (buffer_ref.size() as u64)
                .mul_div_floor(*gst::ClockTime::SECOND, 8000)
                .map(gst::ClockTime::from_nseconds),
        );

        // mark start of talkspurt with RESYNC
        if packet.marker_bit() {
            buffer_ref.set_flags(gst::BufferFlags::RESYNC);
        }

        gst::trace!(CAT, imp = self, "Finishing buffer {buffer:?}");

        self.obj().queue_buffer(packet.into(), buffer)
    }
}
