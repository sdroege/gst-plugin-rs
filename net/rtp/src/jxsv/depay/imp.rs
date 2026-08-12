// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpjxsvdepay
 * @see_also: rtpjxsvpay, svtjpegxsenc, svtjpegxsdec
 *
 * Extracts a JPEG XS video stream from RTP packets as per [RFC 9134][rfc-9134].
 *
 * [rfc-9134]: https://www.rfc-editor.org/rfc/rfc9134.html
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 udpsrc caps='application/x-rtp, media=video, encoding-name=JXSV, clock-rate=90000, packetmode=(string)0, sampling=(string)YCbCr-4:2:2, depth=(string)10, width=(string)1920, height=(string)1080, exactframerate=(string)25' ! rtpjitterbuffer latency=50 ! rtpjxsvdepay ! svtjpegxsdec ! videoconvert ! autovideosink
 * ]| Depayload an incoming RTP JPEG XS video stream. The packetmode, width,
 * height, depth and sampling media type parameters are carried as SDP fmtp
 * parameters, i.e. as strings; svtjpegxsdec requires some of them to negotiate
 * its output.
 *
 * Since: plugins-rs-0.16.0
 */
use atomic_refcell::AtomicRefCell;
use gst::{glib, subclass::prelude::*};

use std::sync::LazyLock;

use crate::{
    basedepay::{PacketToBufferRelation, RtpBaseDepay2Ext},
    jxsv::{
        payload_header::{PayloadHeader, parse_exact_framerate},
        picture_segment::{codestream_offset_in_picture_segment, profile_level_warnings},
        profile_level::ProfileLevel,
    },
};

struct PendingFrame {
    data: Vec<u8>,
    frame_counter: u8,
    next_packet_index: u32,
    start_ext_seqnum: u64,
    ext_timestamp: u64,
}

#[derive(Clone, Default)]
struct StreamInfo {
    width: Option<i32>,
    height: Option<i32>,
    depth: Option<i32>,
    sampling: Option<String>,
    colorimetry: Option<String>,
    framerate: Option<gst::Fraction>,
    profile_level: ProfileLevel,
}

#[derive(Default)]
struct State {
    stream_info: StreamInfo,
    pending_frame: Option<PendingFrame>,
    last_ext_timestamp: Option<u64>,
    // Emit profile/level mismatch warnings only for the first complete frame.
    validated_first_frame: bool,
}

#[derive(Default)]
pub struct RtpJxsvDepay {
    state: AtomicRefCell<State>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpjxsvdepay",
        gst::DebugColorFlags::empty(),
        Some("RTP JPEG XS Depayloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpJxsvDepay {
    const NAME: &'static str = "GstRtpJxsvDepay";
    type Type = super::RtpJxsvDepay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpJxsvDepay {}

impl GstObjectImpl for RtpJxsvDepay {}

impl ElementImpl for RtpJxsvDepay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP JPEG XS depayloader",
                "Codec/Depayloader/Network/RTP",
                "Depayload a JPEG XS video stream from RTP packets (RFC 9134)",
                "Gareth Sylvester-Bradley <garethsb@nvidia.com>",
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
                &gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("encoding-name", crate::jxsv::RTP_ENCODING_NAME)
                    .field("clock-rate", 90_000i32)
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &crate::jxsv::media_pad_caps(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl crate::basedepay::RtpBaseDepay2Impl for RtpJxsvDepay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["video"];

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn flush(&self) {
        let mut state = self.state.borrow_mut();
        state.pending_frame = None;
        state.last_ext_timestamp = None;
        state.validated_first_frame = false;
    }

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let s = caps.structure(0).unwrap();
        let mut state = self.state.borrow_mut();
        state.stream_info = StreamInfo::default();

        // SDP fmtp parameters arrive as strings.
        let get_i32 = |field| {
            s.get::<&str>(field)
                .ok()
                .and_then(|value| value.parse::<i32>().ok())
        };

        // RFC 9134 default packetmode = 0
        let packetmode = match get_i32("packetmode") {
            Some(packetmode) => packetmode,
            None if !s.has_field("packetmode") => 0,
            None => {
                gst::error!(CAT, imp = self, "Invalid packetmode");
                return false;
            }
        };
        if packetmode != 0 {
            gst::error!(
                CAT,
                imp = self,
                "Only codestream packetization mode (packetmode=0) is supported"
            );
            return false;
        }

        // RFC 9134 default transmode = 1
        let transmode = match get_i32("transmode") {
            Some(transmode) => transmode,
            None if !s.has_field("transmode") => 1,
            None => {
                gst::error!(CAT, imp = self, "Invalid transmode");
                return false;
            }
        };
        if transmode != 1 {
            gst::error!(
                CAT,
                imp = self,
                "Only sequential transmission mode (transmode=1) is supported"
            );
            return false;
        }

        // width, height and depth arrive as SDP fmtp parameters, i.e. as strings
        // on the RTP caps, but are integers on the image/x-jxsc caps downstream.
        state.stream_info.width = get_i32("width");
        state.stream_info.height = get_i32("height");
        state.stream_info.depth = get_i32("depth");
        state.stream_info.sampling = s.get::<String>("sampling").ok();
        // SDP fmtp names are lower-cased on the RTP caps (same as rtpvrawdepay2).
        state.stream_info.colorimetry = s.get::<&str>("colorimetry").ok().and_then(|colorimetry| {
            let depth = state.stream_info.depth.and_then(|d| u8::try_from(d).ok());
            crate::fmtp_color_params::gst_colorimetry_from_fmtp_color_params(
                colorimetry,
                s.get::<&str>("tcs").ok(),
                s.get::<&str>("range").ok(),
                depth,
                crate::fmtp_color_params::FmtpColorDefaults::Rfc9134,
            )
        });

        if let Ok(exactframerate) = s.get::<&str>("exactframerate") {
            match parse_exact_framerate(exactframerate) {
                Ok(framerate) => state.stream_info.framerate = Some(framerate),
                Err(err) => {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Failed to parse exactframerate '{exactframerate}': {err}"
                    );
                }
            }
        }
        state.stream_info.profile_level = match ProfileLevel::from_caps(s) {
            Ok(profile_level) => profile_level,
            Err(err) => {
                gst::error!(CAT, imp = self, "{err}");
                return false;
            }
        };
        state.validated_first_frame = false;

        let stream_info = state.stream_info.clone();
        drop(state);

        let caps = Self::output_caps(&stream_info);
        gst::debug!(CAT, imp = self, "Setting caps {caps:?}");
        self.obj().set_src_caps(&caps);

        true
    }

    fn handle_packet(
        &self,
        packet: &crate::basedepay::Packet,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();
        let payload = packet.payload();

        if payload.len() < PayloadHeader::SIZE {
            gst::warning!(
                CAT,
                imp = self,
                "RTP packet too small for JXSV payload header"
            );
            state.pending_frame = None;
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        let header = match PayloadHeader::parse(&payload[..PayloadHeader::SIZE]) {
            Ok(header) => header,
            Err(err) => {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Failed to parse JXSV payload header: {err}"
                );
                state.pending_frame = None;
                self.obj().drop_packet(packet);
                return Ok(gst::FlowSuccess::Ok);
            }
        };

        if !header.transmission_mode || header.packetization_mode || header.interlaced != 0 {
            gst::warning!(
                CAT,
                imp = self,
                "Unsupported JXSV payload header flags: {header:?}"
            );
            state.pending_frame = None;
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        if header.last != packet.marker_bit() {
            gst::warning!(
                CAT,
                imp = self,
                "JXSV L bit ({}) does not match RTP marker bit ({})",
                header.last,
                packet.marker_bit()
            );
            state.pending_frame = None;
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        let picture_segment = &payload[PayloadHeader::SIZE..];
        let packet_index = u32::from(header.sep_counter) * 2048 + u32::from(header.p_counter);

        if state.last_ext_timestamp != Some(packet.ext_timestamp()) && state.pending_frame.is_some()
        {
            gst::warning!(
                CAT,
                imp = self,
                "New RTP timestamp before previous frame completed"
            );
            state.pending_frame = None;
        }
        state.last_ext_timestamp = Some(packet.ext_timestamp());

        // Packet index 0 (SEP*2048 + P) marks the start of a frame. Mid-stream
        // joins wait for that rather than collecting from an arbitrary packet.
        if state.pending_frame.is_none() {
            if packet_index != 0 {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Received continuation packet without frame start"
                );
                self.obj().drop_packet(packet);
                return Ok(gst::FlowSuccess::Ok);
            }

            state.pending_frame = Some(PendingFrame {
                data: Vec::new(),
                frame_counter: header.frame_counter,
                next_packet_index: 0,
                start_ext_seqnum: packet.ext_seqnum(),
                ext_timestamp: packet.ext_timestamp(),
            });
        }

        let pending_frame = state.pending_frame.as_mut().unwrap();

        if pending_frame.frame_counter != header.frame_counter
            || pending_frame.ext_timestamp != packet.ext_timestamp()
        {
            gst::warning!(
                CAT,
                imp = self,
                "Frame counter or timestamp mismatch within frame"
            );
            state.pending_frame = None;
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        if packet_index != pending_frame.next_packet_index {
            gst::warning!(
                CAT,
                imp = self,
                "Expected packet index {} but got {}",
                pending_frame.next_packet_index,
                packet_index
            );
            state.pending_frame = None;
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        pending_frame.data.extend_from_slice(picture_segment);
        pending_frame.next_packet_index = packet_index + 1;

        if !packet.marker_bit() {
            return Ok(gst::FlowSuccess::Ok);
        }

        let mut pending_frame = state.pending_frame.take().unwrap();

        if !state.validated_first_frame {
            for warning in
                profile_level_warnings(&pending_frame.data, &state.stream_info.profile_level)
            {
                gst::warning!(CAT, imp = self, "{warning}");
            }
            state.validated_first_frame = true;
        }

        let offset = match codestream_offset_in_picture_segment(&pending_frame.data) {
            Ok(offset) => offset,
            Err(err) => {
                gst::warning!(CAT, imp = self, "Invalid JPEG XS picture segment: {err}");
                self.obj().drop_packet(packet);
                return Ok(gst::FlowSuccess::Ok);
            }
        };
        let data = pending_frame.data.split_off(offset);

        let buffer = gst::Buffer::from_mut_slice(data);
        self.obj().queue_buffer(
            PacketToBufferRelation::Seqnums(pending_frame.start_ext_seqnum..=packet.ext_seqnum()),
            buffer,
        )
    }
}

impl RtpJxsvDepay {
    fn output_caps(stream_info: &StreamInfo) -> gst::Caps {
        // width, height, depth and sampling are all optional at the RTP layer in
        // RFC 9134, so pass through only those signalled by the SDP fmtp
        // parameters. A downstream decoder may require some of them to negotiate.
        let builder = gst::Caps::builder(crate::jxsv::MEDIA_TYPE_JXSC)
            .field("alignment", "frame")
            .field("interlace-mode", "progressive")
            .field_if_some("width", stream_info.width)
            .field_if_some("height", stream_info.height)
            .field_if_some("depth", stream_info.depth)
            .field_if_some("sampling", stream_info.sampling.as_deref())
            .field_if_some("colorimetry", stream_info.colorimetry.as_deref())
            .field_if_some("framerate", stream_info.framerate);

        stream_info.profile_level.add_to_caps(builder).build()
    }
}
