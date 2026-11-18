// GStreamer RTP H.261 Depayloader
//
// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtph261depay2
 * @see_also: rtph261pay2, avdec_h261
 *
 * Extracts H.261 video frames from RTP packets as per
 * [RFC 4587][rfc-4587].
 *
 * Packets carry raw H.261 bitstream fragments that start and end on
 * macroblock boundaries; the depayloader splices them back together using
 * the `SBIT`/`EBIT` bit positions and outputs one buffer per picture,
 * delimited by the RTP marker bit. A picture is only emitted once its
 * picture start code has been seen, so streams joined mid-picture start
 * emitting at the next picture boundary.
 *
 * [rfc-4587]: https://www.rfc-editor.org/rfc/rfc4587.html
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 udpsrc2 caps='application/x-rtp,media=video,encoding-name=H261,clock-rate=90000' ! rtph261depay2 ! avdec_h261 ! autovideosink
 * ]| This example pipeline will depayload and decode an RTP H.261 video
 * stream. Refer to the rtph261pay2 example to create the RTP stream.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::{glib, subclass::prelude::*};

use atomic_refcell::AtomicRefCell;
use std::sync::LazyLock;

use crate::basedepay::{PacketToBufferRelation, RtpBaseDepay2Ext, RtpBaseDepay2Impl};

/// Size of the H.261 payload header (RFC 4587 section 4.1): 4 bytes.
const PAYLOAD_HEADER_LEN: usize = 4;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtph261depay2",
        gst::DebugColorFlags::empty(),
        Some("RTP H.261 Depayloader"),
    )
});

/// Picture being assembled from packet payloads.
#[derive(Default)]
struct PendingFrame {
    start_ext_seqnum: u64,
    ext_timestamp: u64,
    data: Vec<u8>,
    /// Trailing bits of the last byte of the last packet, to be merged with
    /// the first byte of the next packet.
    leftover: Option<u8>,
}

#[derive(Default)]
struct State {
    pending_frame: Option<PendingFrame>,
}

/// Whether the data starts with a picture start code at bit offset `sbit`.
fn starts_with_picture_start_code(data: &[u8], sbit: u32) -> bool {
    // Detection needs four data bytes; a first fragment carrying one to
    // three data bytes is not recognised as a picture start and the
    // picture is dropped.
    data.len() >= 4
        && (u32::from_be_bytes([data[0], data[1], data[2], data[3]]) << sbit) >> 12 == 0x10
}

#[derive(Default)]
pub struct RtpH261Depay {
    state: AtomicRefCell<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for RtpH261Depay {
    const NAME: &'static str = "GstRtpH261Depay2";
    type Type = super::RtpH261Depay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpH261Depay {}

impl GstObjectImpl for RtpH261Depay {}

impl ElementImpl for RtpH261Depay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP H.261 Depayloader",
                "Codec/Depayloader/Network/RTP",
                "Extract an H.261 video stream from RTP packets (RFC 4587)",
                "Edward Hervey <edward@centricular.com>",
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
                    .field("encoding-name", "H261")
                    .field("clock-rate", 90_000i32)
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/x-h261").build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl RtpBaseDepay2Impl for RtpH261Depay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["video"];

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn drain(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Incomplete pictures are dropped; a picture can only be completed
        // with its final (marker) packet.
        self.state.borrow_mut().pending_frame = None;

        Ok(gst::FlowSuccess::Ok)
    }

    fn flush(&self) {
        *self.state.borrow_mut() = State::default();
    }

    fn set_sink_caps(&self, _caps: &gst::Caps) -> bool {
        // H.261 has no stream variants to derive from the SDP parameters;
        // the output caps are constant.
        let src_caps = gst::Caps::builder("video/x-h261").build();

        gst::debug!(CAT, imp = self, "Negotiated src caps: {src_caps:?}");
        self.obj().set_src_caps(&src_caps);

        true
    }

    fn handle_packet(
        &self,
        packet: &crate::basedepay::Packet,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        if packet.discont() {
            gst::debug!(
                CAT,
                imp = self,
                "Discontinuity, dropping incomplete picture"
            );
            *state = State::default();
        }

        let payload = packet.payload();
        if payload.len() <= PAYLOAD_HEADER_LEN {
            gst::warning!(
                CAT,
                imp = self,
                "Dropping packet with payload too small for payload header"
            );
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        // H.261 payload header (RFC 4587 section 4.1).
        let sbit = (payload[0] >> 5) as u32;
        let ebit = ((payload[0] >> 2) & 0x07) as u32;
        // The I flag signals that the whole stream is intra coded, not
        // that this particular picture is intra coded.
        let intra_only = payload[0] & 0x02 != 0;
        let data = &payload[PAYLOAD_HEADER_LEN..];

        // A packet of a new picture may have arrived after the marker packet
        // of the pending one was lost: the pending picture will never
        // complete. Drop it and require the current packet to start a new
        // picture.
        if state
            .pending_frame
            .as_ref()
            .is_some_and(|frame| frame.ext_timestamp != packet.ext_timestamp())
        {
            gst::warning!(
                CAT,
                imp = self,
                "RTP timestamp changed before picture completion, dropping incomplete picture"
            );
            state.pending_frame = None;
        }

        // Accumulation only starts at a picture start code.
        if state.pending_frame.is_none() && !starts_with_picture_start_code(data, sbit) {
            gst::debug!(
                CAT,
                imp = self,
                "No picture start code yet, dropping packet"
            );
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        let pending_frame = state.pending_frame.get_or_insert_with(|| PendingFrame {
            start_ext_seqnum: packet.ext_seqnum(),
            ext_timestamp: packet.ext_timestamp(),
            ..Default::default()
        });

        // Splice the packet into the picture: the first byte continues the
        // previous packet's last byte (SBIT unused leading bits, EBIT
        // unused trailing bits), so the two partial bytes merge into one.
        let mut first = data[0];
        if sbit != 0 {
            first &= 0xFF >> sbit;
            if let Some(leftover) = pending_frame.leftover.take() {
                first |= leftover;
            }
        } else {
            // The previous packet ended on a byte boundary: its trailing
            // byte is complete and the next packet must not overwrite it.
            if let Some(leftover) = pending_frame.leftover.take() {
                pending_frame.data.push(leftover);
            }
        }
        pending_frame.data.push(first);
        if ebit == 0 {
            pending_frame.data.extend_from_slice(&data[1..]);
        } else if data.len() == 1 {
            // The merged byte is also the last byte: it is carried over
            // to the next packet whole instead of being pushed.
            pending_frame.data.pop();
            pending_frame.leftover = Some(first & (0xFF << ebit));
        } else {
            pending_frame
                .data
                .extend_from_slice(&data[1..data.len() - 1]);
            pending_frame.leftover = Some(data[data.len() - 1] & (0xFF << ebit));
        }

        if !packet.marker_bit() {
            return Ok(gst::FlowSuccess::Ok);
        }

        if let Some(leftover) = pending_frame.leftover.take() {
            pending_frame.data.push(leftover);
        }

        let pending_frame = state
            .pending_frame
            .take()
            .expect("pending frame was just inserted");

        let mut buffer = gst::Buffer::from_mut_slice(pending_frame.data);
        // A fresh buffer carries no flags, so only inter pictures are
        // flagged.
        if !intra_only {
            buffer
                .get_mut()
                .unwrap()
                .set_flags(gst::BufferFlags::DELTA_UNIT);
        }

        self.obj().queue_buffer(
            PacketToBufferRelation::Seqnums(pending_frame.start_ext_seqnum..=packet.ext_seqnum()),
            buffer,
        )
    }
}
