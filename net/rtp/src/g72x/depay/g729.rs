//
// Copyright (C) 2026 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, subclass::prelude::*};

use std::sync::LazyLock;

use crate::basedepay::RtpBaseDepay2Ext;

/**
 * SECTION:element-rtpg729depay2
 * @see_also: rtpg729pay2
 *
 * Extracts G729 encoded audio from RTP packets as per [RFC 3551][rfc-3551].
 *
 * [rfc-3551]: https://www.rfc-editor.org/rfc/rfc3551.html#section-4.5.6
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 udpsrc2 caps='application/x-rtp, media=audio, clock-rate=8000, payload=18' ! rtpjitterbuffer latency=50 ! rtpg729depay2 ! avenc_g729 ! audioconvert ! audioresample ! autoaudiosink
 * ]| This will depayload an incoming RTP G729 audio stream. You can use the #rtpg729pay2 and
 * avenc_g729 elements to create such an RTP stream.
 *
 * Since: plugins-rs-0.19.0
 */

#[derive(Default)]
pub struct RtpG729Depay;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpg729depay2",
        gst::DebugColorFlags::empty(),
        Some("RTP G729 Depayloader"),
    )
});

const CLOCK_RATE: i32 = 8000;
const FRAME_SIZE: gst::ClockTime = gst::ClockTime::from_mseconds(10);

#[glib::object_subclass]
impl ObjectSubclass for RtpG729Depay {
    const NAME: &'static str = "GstRtpG729Depay2";
    type Type = super::RtpG729Depay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpG729Depay {}

impl GstObjectImpl for RtpG729Depay {}

impl ElementImpl for RtpG729Depay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP G.729 Depayloader",
                "Codec/Depayloader/Network/RTP",
                "Depayload G.729 from RTP packets (RFC 3551)",
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
                            .field("payload", 18i32)
                            .field("clock-rate", CLOCK_RATE)
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "audio")
                            .field("clock-rate", CLOCK_RATE)
                            .field("encoding-name", "G729")
                            .build(),
                    )
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("audio/G729")
                    .field("channels", 1i32)
                    .field("rate", 8000i32)
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basedepay::RtpBaseDepay2Impl for RtpG729Depay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["audio"];

    fn set_sink_caps(&self, _caps: &gst::Caps) -> bool {
        let src_caps = gst::Caps::builder("audio/G729")
            .field("channels", 1i32)
            .field("rate", CLOCK_RATE)
            .build();

        self.obj().set_src_caps(&src_caps);

        true
    }

    fn handle_packet(
        &self,
        packet: &crate::basedepay::Packet,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut buffer = packet.payload_buffer();
        let buffer_size = buffer.size();

        let frames = buffer_size / 10;
        let frame_rem = buffer_size % 10;
        let has_cn = frame_rem == 2;
        if ![0, 2].contains(&frame_rem) {
            gst::element_error!(
                self.obj(),
                gst::StreamError::WrongType,
                ("Invalid input buffer size"),
                [
                    "Invalid buffer size, should be multiple of 10 with an optional comfort frame size of 2 but it is {buffer_size}",
                ]
            );
            return Err(gst::FlowError::Error);
        }
        let frames_and_comfort = if has_cn { frames + 1 } else { frames };

        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_duration(FRAME_SIZE * frames_and_comfort as u64);

        // mark start of talkspurt with RESYNC
        if packet.marker_bit() {
            buffer_ref.set_flags(gst::BufferFlags::RESYNC);
        }

        gst::trace!(CAT, imp = self, "Finishing buffer {buffer:?}");

        self.obj().queue_buffer(packet.into(), buffer)
    }
}
