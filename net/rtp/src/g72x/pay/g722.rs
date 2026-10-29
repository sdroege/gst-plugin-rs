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

use crate::{
    baseaudiopay::RtpBaseAudioPay2Ext,
    basepay::{RtpBasePay2Ext, RtpBasePay2ImplExt},
};

/**
 * SECTION:element-rtpg7222
 * @see_also: rtpg722depay2
 *
 * Payloads G.722 encoded audio into RTP packets as per [RFC 3551][rfc-3551].
 *
 * [rfc-3551]: https://www.rfc-editor.org/rfc/rfc3551.html#section-4.5.2
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 audiotestsrc wave=ticks ! audio/x-raw,rate=8000,channels=1 ! avenc_g722 ! rtpg722pay2 ! udpsink2 host=127.0.0.1 port=5004
 * ]| This will generate an G.722 audio test signal and payload it as RTP and send it out
 * as UDP to localhost port 5004.
 *
 * Since: plugins-rs-0.16.0
 */

#[derive(Default)]
pub struct RtpG722Pay;

#[glib::object_subclass]
impl ObjectSubclass for RtpG722Pay {
    const NAME: &'static str = "GstRtpG722Pay2";
    type Type = super::RtpG722Pay;
    type ParentType = crate::baseaudiopay::RtpBaseAudioPay2;
}

impl ObjectImpl for RtpG722Pay {}

impl GstObjectImpl for RtpG722Pay {}

impl ElementImpl for RtpG722Pay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP G.722 Payloader",
                "Codec/Payloader/Network/RTP",
                "Payload G.722 Audio into RTP packets (RFC 3551)",
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
                &gst::Caps::builder("audio/G722")
                    .field("channels", 1i32)
                    .field("rate", 16000i32)
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
                            .field("payload", 9i32)
                            .field("clock-rate", 8000i32)
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "audio")
                            .field("encoding-name", "G722")
                            .field("encoding-params", "1")
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

impl crate::basepay::RtpBasePay2Impl for RtpG722Pay {
    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let s = caps.structure(0).unwrap();

        let channels = s.get::<i32>("channels").unwrap();

        let src_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "G722")
            .field("encoding-params", channels.to_string())
            .field("channels", channels)
            .field("clock-rate", 8000i32)
            .build();

        self.obj().set_src_caps(&src_caps);
        self.obj().set_bpf(channels as usize);

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
}

impl crate::baseaudiopay::RtpBaseAudioPay2Impl for RtpG722Pay {}
