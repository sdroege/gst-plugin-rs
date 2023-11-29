// GStreamer RTP MPEG-1/MPEG-2 Video Elementary Stream Depayloader
//
// Copyright (C) 2023-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpmpvdepay2
 * @see_also: rtpmpvpay2, rtpmpvpay, rtpmpvdepay, mpeg2enc, avdec_mpeg2video, avenc_mpeg2video
 *
 * Depayload an MPEG-1 or MPEG-2 Video Elementary Stream from RTP packets as per [RFC 2250][rfc-2250].
 *
 * [rfc-2250]: https://www.rfc-editor.org/rfc/rfc2250#section-3
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 udpsrc address=127.0.0.1 port=5555 caps='application/x-rtp,media=video,clock-rate=90000,encoding-name=MPV' ! rtpjitterbuffer latency=100 ! rtpmpvdepay2 ! decodebin3 ! videoconvertscale ! autovideosink
 * ]| This will depayload and decode an incoming RTP MPEG-2 video stream. You can use the #rtpmpvpay2
 * and #mpeg2enc or avenc_mpeg2video elements to create such an RTP stream.
 *
 * Since: plugins-rs-0.16.0
 */
use std::sync::LazyLock;

use gst::subclass::prelude::*;

use crate::basedepay::RtpBaseDepay2Ext;

const RTP_MPEG_VIDEO_DEFAULT_PT: u32 = 32;

#[derive(Default)]
pub struct RtpMpegVideoDepay;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpmpvdepay2",
        gst::DebugColorFlags::empty(),
        Some("RTP MPEG-1 and MPEG-2 Video Depayloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpMpegVideoDepay {
    const NAME: &'static str = "GstRtpMpegVideoDepay2";
    type Type = super::RtpMpegVideoDepay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpMpegVideoDepay {}

impl GstObjectImpl for RtpMpegVideoDepay {}

impl ElementImpl for RtpMpegVideoDepay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP MPEG Video Elementary Stream Depayloader",
                "Codec/Depayloader/Network/RTP",
                "Depayload an MPEG-1 or MPEG-2 Elementary Stream from RTP packets (RFC 2250)",
                "Tim-Philipp Müller <tim centricular com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/mpeg")
                    .field("systemstream", false)
                    .field("mpegversion", gst::IntRange::new(1i32, 2))
                    .field("parsed", false)
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::builder_full()
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("encoding-name", "MPV")
                            .field("clock-rate", 90000i32)
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("depayload", RTP_MPEG_VIDEO_DEFAULT_PT as i32)
                            .field("clock-rate", 90000i32)
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

impl crate::basedepay::RtpBaseDepay2Impl for RtpMpegVideoDepay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["video"];

    fn set_sink_caps(&self, _caps: &gst::Caps) -> bool {
        // Todo: could theoretically also be MPEG-1. Ideally we'd look at the headers
        // and set the version accordingly instead of just claiming it's MPEG-2
        //
        let src_caps = gst::Caps::builder("video/mpeg")
            .field("mpegversion", 2i32)
            .field("systemstream", false)
            .field("parsed", false)
            .build();

        self.obj().set_src_caps(&src_caps);

        true
    }

    // Encapsulation of MPEG Video Elementary Streams:
    // https://datatracker.ietf.org/doc/html/rfc2250#section-3.1
    //
    fn handle_packet(
        &self,
        packet: &crate::basedepay::Packet,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let payload = packet.payload();

        // Need at least the MPEG video specific header
        if payload.is_empty() || payload.len() < 4 + (payload[0] & 0x04) as usize {
            gst::warning!(
                CAT,
                imp = self,
                "Payload too small: {} bytes, but need at least {} bytes",
                payload.len(),
                if !payload.is_empty() {
                    4 + (payload[0] & 0x04) as usize
                } else {
                    4
                },
            );

            self.obj().drop_packet(packet);

            return Ok(gst::FlowSuccess::Ok);
        }

        // MPEG video specific header
        // https://datatracker.ietf.org/doc/html/rfc2250#section-3.4
        //
        //  0                   1                   2                   3
        //  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
        //  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        //  |    MBZ  |T|         TR        | |N|S|B|E|  P  | | BFC | | FFC |
        //  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        //                                  AN              FBV     FFV
        //
        let mut hdr_len = 4;

        // Extended MPEG video specific header(s)
        //
        // 0                   1                   2                   3
        // 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
        // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        // |X|E|f_[0,0]|f_[0,1]|f_[1,0]|f_[1,1]| DC| PS|T|P|C|Q|V|A|R|H|G|D|
        // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        //
        if payload[0] & 0x04 != 0 {
            // T: MPEG-2 specific header extension present
            hdr_len += 4;
            if payload[7] & 1 != 0 {
                // D: Composite display information present
                hdr_len += 4;
            }
            if payload[4] & 0x40 != 0 {
                // E: Extensions present
                // 1 byte specifies the length of the extensions data in 32-bit words.
                // Just skip the length byte here. The extensions themselves come with full
                // sync codes and all so are perfectly valid from a bitstream perspective,
                // so we can just leave them in there for now. One day we might want to do some
                // more parsing and reconstruction of various possibly missing headers from the
                // MPEG video specific headers, but that's for another time, and it doesn't look
                // like anyone's really needed this so far either.
                hdr_len += 1;
            }
        };

        // Just push out the payloaded ES data as-is and let someone else do the parsing
        let mut outbuf = packet.payload_subbuffer(hdr_len..);

        // Marker flag indicates end of frame:
        // M bit: For video, set to 1 on packet containing MPEG frame end code, 0 otherwise.
        if packet.marker_bit() {
            let outbuf_ref = outbuf.get_mut().unwrap();
            outbuf_ref.set_flags(gst::BufferFlags::MARKER);
        }

        // E: Set when the last byte of the payload is the end of an MPEG slice.
        // We can pass that on to the parser via buffer flags, which will help
        // reduce latency.
        // FIXME: check this: whether MARKER flag means end of packet/slice for MPEG-2 or end of frame
        // Looks like mpegvideoparse currently doesn't interpret this flag at all..
        /*
        let end_of_slice = payload[2] & 0x08 != 0;

        if end_of_slice {
            let outbuf_ref = outbuf.get_mut().unwrap();
            outbuf_ref.set_flags(gst::BufferFlags::MARKER);
        }
        */

        gst::trace!(CAT, imp = self, "Finishing buffer {outbuf:?}");

        self.obj().queue_buffer(packet.into(), outbuf)
    }
}

impl RtpMpegVideoDepay {}
