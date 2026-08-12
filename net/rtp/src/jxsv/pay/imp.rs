// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpjxsvpay
 * @see_also: rtpjxsvdepay, svtjpegxsenc, svtjpegxsdec
 *
 * Payload a JPEG XS video stream into RTP packets as per [RFC 9134][rfc-9134].
 *
 * [rfc-9134]: https://www.rfc-editor.org/rfc/rfc9134.html
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 ... ! svtjpegxsenc ! rtpjxsvpay ! udpsink host=127.0.0.1 port=5004
 * ]| Payload a bare JPEG XS codestream from `svtjpegxsenc`, wrapping each encoded
 * frame into an RFC 9134 picture segment before RTP packetization. Sink caps
 * require a usable framerate; `codestream-length` (from `svtjpegxsenc`) or else
 * PIH `Lcod` is used with that framerate for jpvi `brat`.
 *
 * Since: plugins-rs-0.16.0
 */
use atomic_refcell::AtomicRefCell;
use gst::{glib, subclass::prelude::*};
use std::cmp;

use std::sync::LazyLock;

use crate::{
    basepay::RtpBasePay2Ext,
    jxsv::{
        payload_header::{PayloadHeader, format_exact_framerate},
        picture_segment::PictureSegmentBuilder,
        profile_level::ProfileLevel,
    },
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpjxsvpay",
        gst::DebugColorFlags::empty(),
        Some("RTP JPEG XS Payloader"),
    )
});

#[derive(Default)]
struct State {
    frame_counter: u8,
    segment_builder: Option<PictureSegmentBuilder>,
    profile_level: ProfileLevel,
}

#[derive(Default)]
pub struct RtpJxsvPay {
    state: AtomicRefCell<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for RtpJxsvPay {
    const NAME: &'static str = "GstRtpJxsvPay";
    type Type = super::RtpJxsvPay;
    type ParentType = crate::basepay::RtpBasePay2;
}

impl ObjectImpl for RtpJxsvPay {}

impl GstObjectImpl for RtpJxsvPay {}

impl ElementImpl for RtpJxsvPay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP JPEG XS payloader",
                "Codec/Payloader/Network/RTP",
                "Payload a JPEG XS video stream to RTP packets (RFC 9134)",
                "Gareth Sylvester-Bradley <garethsb@nvidia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = crate::jxsv::media_pad_caps();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("encoding-name", crate::jxsv::RTP_ENCODING_NAME)
                    .field("clock-rate", 90_000i32)
                    .field("packetmode", "0")
                    .field("transmode", "1")
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basepay::RtpBasePay2Impl for RtpJxsvPay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["video"];

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        gst::debug!(CAT, imp = self, "received caps {caps:?}");

        let s = caps.structure(0).unwrap();

        let alignment = s.get::<&str>("alignment").unwrap_or("frame");
        if alignment != "frame" {
            gst::error!(
                CAT,
                imp = self,
                "Only frame-aligned JPEG XS is supported, got alignment={alignment}"
            );
            return false;
        }

        let interlace_mode = s.get::<&str>("interlace-mode").unwrap_or("progressive");
        if interlace_mode != "progressive" {
            gst::error!(
                CAT,
                imp = self,
                "Only progressive JPEG XS is supported, got interlace-mode={interlace_mode}"
            );
            return false;
        }

        let profile_level = match ProfileLevel::from_caps(s) {
            Ok(profile_level) => profile_level,
            Err(err) => {
                gst::error!(CAT, imp = self, "{err}");
                return false;
            }
        };

        let segment_builder = match PictureSegmentBuilder::from_caps(caps) {
            Ok(builder) => builder,
            Err(err) => {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to build JPEG XS picture segment template: {err}"
                );
                return false;
            }
        };

        // packetmode, transmode, width, height and depth are all carried as SDP
        // fmtp parameters, i.e. as strings, on the RTP caps.
        let mut caps_builder = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("clock-rate", 90_000i32)
            .field("encoding-name", crate::jxsv::RTP_ENCODING_NAME)
            .field("packetmode", "0")
            .field("transmode", "1");

        // width, height and depth are integers on the JPEG XS caps.
        if let Ok(width) = s.get::<i32>("width") {
            caps_builder = caps_builder.field("width", width.to_string());
        }
        if let Ok(height) = s.get::<i32>("height") {
            caps_builder = caps_builder.field("height", height.to_string());
        }
        if let Ok(depth) = s.get::<i32>("depth") {
            caps_builder = caps_builder.field("depth", depth.to_string());
        }
        if let Ok(sampling) = s.get::<&str>("sampling") {
            caps_builder = caps_builder.field("sampling", sampling);
        }
        // RFC 9134 colorimetry / TCS / RANGE from GStreamer colorimetry
        // (named presets or numeric r:m:t:p).
        if let Ok(colorimetry) = s.get::<&str>("colorimetry")
            && let Some(fmtp) =
                crate::fmtp_color_params::fmtp_color_params_from_gst_colorimetry(colorimetry)
        {
            caps_builder = caps_builder.field("colorimetry", fmtp.colorimetry);
            // SDP fmtp names are lower-cased on the RTP caps (same as rtpvrawpay2).
            if let Some(tcs) = fmtp.tcs {
                caps_builder = caps_builder.field("tcs", tcs);
            }
            if let Some(range) = fmtp.range {
                caps_builder = caps_builder.field("range", range);
            }
        }
        if let Some(framerate) = s
            .get::<gst::Fraction>("framerate")
            .ok()
            .filter(|fps| *fps > gst::Fraction::new(0, 1))
        {
            caps_builder = caps_builder.field("exactframerate", format_exact_framerate(framerate));
        }
        caps_builder = profile_level.add_to_caps(caps_builder);

        self.obj().set_src_caps(&caps_builder.build());

        let mut state = self.state.borrow_mut();
        state.segment_builder = Some(segment_builder);
        state.profile_level = profile_level;
        // Caps can change while PLAYING (base payloader drains first). Restart
        // the RFC 9134 payload-header frame counter (F, 5 bits) so it does not
        // continue across the renegotiation.
        state.frame_counter = 0;

        true
    }

    fn handle_buffer(
        &self,
        buffer: &gst::Buffer,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();
        let max_payload_size = self.obj().max_payload_size();

        gst::trace!(CAT, imp = self, "received buffer of size {}", buffer.size());

        let map = buffer.map_readable().map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Read,
                ["Failed to map buffer readable"]
            );
            gst::FlowError::Error
        })?;

        let builder = state.segment_builder.as_ref().ok_or_else(|| {
            gst::element_imp_error!(
                self,
                gst::LibraryError::Settings,
                ["JPEG XS picture segment builder not configured"]
            );
            gst::FlowError::Error
        })?;

        let picture_segment = builder
            .wrap_codestream(map.as_ref(), buffer.pts())
            .map(|(picture_segment, warnings)| {
                for warning in warnings {
                    gst::warning!(CAT, imp = self, "{warning}");
                }
                picture_segment
            })
            .map_err(|err| {
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Format,
                    ["Failed to build JPEG XS picture segment: {err}"]
                );
                gst::FlowError::Error
            })?;

        let frame_counter = state.frame_counter;
        let max_data_per_packet = max_payload_size
            .checked_sub(PayloadHeader::SIZE as u32)
            .ok_or_else(|| {
                gst::element_imp_error!(
                    self,
                    gst::LibraryError::Settings,
                    ["Too small MTU configured for stream"]
                );
                gst::FlowError::Error
            })?;

        let mut data = picture_segment.as_slice();
        let mut packet_index = 0u32;

        while !data.is_empty() {
            let payload_size = cmp::min(data.len(), max_data_per_packet as usize);
            let last = data.len() == payload_size;
            let header = PayloadHeader::progressive_codestream(frame_counter, packet_index, last);
            let header_bytes = header.pack().map_err(|err| {
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Encode,
                    ["Failed to pack JPEG XS RTP payload header: {err}"]
                );
                gst::FlowError::Error
            })?;

            self.obj().queue_packet(
                id.into(),
                rtp_types::RtpPacketBuilder::new()
                    .marker_bit(last)
                    .payload(header_bytes.as_slice())
                    .payload(&data[..payload_size]),
            )?;

            data = &data[payload_size..];
            packet_index += 1;
        }

        state.frame_counter = state.frame_counter.wrapping_add(1) & 0x1f;

        Ok(gst::FlowSuccess::Ok)
    }
}
