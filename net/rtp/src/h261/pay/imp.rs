// GStreamer RTP H.261 Payloader
//
// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtph261pay2
 * @see_also: rtph261depay2, avenc_h261
 *
 * Payloads an H.261 video stream into RTP packets as per
 * [RFC 4587][rfc-4587].
 *
 * Each input buffer must contain exactly one complete picture. Packets
 * start and end on macroblock boundaries; the macroblock layer is parsed
 * to find those boundaries. The GOB number, macroblock address
 * predictor, quantizer and reference motion vectors in effect at each
 * packet start are carried in the payload header so that a receiver can
 * decode the packet's macroblocks after a loss.
 *
 * The `I` and `V` hint flags are constant for the session: `V` is always
 * 1 and `I` is always 0, the combination RFC 4587 allows any conforming
 * implementation to send.
 *
 * [rfc-4587]: https://www.rfc-editor.org/rfc/rfc4587.html
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 videotestsrc ! video/x-raw,format=I420,width=176,height=144 ! avenc_h261 ! rtph261pay2 ! udpsink2 host=127.0.0.1 port=5004
 * ]| This will encode a test pattern as H.261 and payload it as RTP to
 * localhost port 5004.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::{glib, subclass::prelude::*};

use atomic_refcell::AtomicRefCell;
use std::sync::LazyLock;

use crate::basepay::{RtpBasePay2Ext, RtpBasePay2Impl};
use crate::h261::bitstream::{self, BitCursor, Gob, Macroblock, MbResult, PAD_BYTES};

/// Size of the H.261 payload header (RFC 4587 section 4.1): 4 bytes.
const PAYLOAD_HEADER_LEN: usize = 4;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtph261pay2",
        gst::DebugColorFlags::empty(),
        Some("RTP H.261 Payloader"),
    )
});

#[derive(Default)]
struct State {
    /// Bit position (0..7) at which the next input buffer's picture start
    /// code must be aligned, left over from the last packet of the previous
    /// picture.
    offset: u8,
}

#[derive(Default)]
pub struct RtpH261Pay {
    state: AtomicRefCell<State>,
}

impl RtpH261Pay {
    fn reset(&self) {
        *self.state.borrow_mut() = State::default();
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RtpH261Pay {
    const NAME: &'static str = "GstRtpH261Pay2";
    type Type = super::RtpH261Pay;
    type ParentType = crate::basepay::RtpBasePay2;
}

impl ObjectImpl for RtpH261Pay {}

impl GstObjectImpl for RtpH261Pay {}

impl ElementImpl for RtpH261Pay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP H.261 Payloader",
                "Codec/Payloader/Network/RTP",
                "Payload an H.261 video stream into RTP packets (RFC 4587)",
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
                &gst::Caps::builder("video/x-h261").build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("encoding-name", "H261")
                    .field("clock-rate", 90_000i32)
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl RtpBasePay2Impl for RtpH261Pay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["video"];

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.reset();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        self.reset();
        Ok(())
    }

    fn flush(&self) {
        self.reset();
    }

    fn set_sink_caps(&self, _caps: &gst::Caps) -> bool {
        // The sink pad template is a single fixed structure and carries no
        // stream variants to reflect: the output caps are constant.
        let src_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", "H261")
            .field("clock-rate", 90_000i32)
            .build();

        gst::debug!(CAT, imp = self, "Negotiated src caps: {src_caps:?}");
        self.obj().set_src_caps(&src_caps);

        true
    }

    fn handle_buffer(
        &self,
        buffer: &gst::Buffer,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let map = buffer.map_readable().map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Read,
                ["Failed to map buffer readable"]
            );

            gst::FlowError::Error
        })?;
        let data = map.as_slice();

        // The picture start code may be bit-unaligned in the input buffer;
        // its bit offset defines the alignment of the whole picture.
        let Some(psc_offset) = bitstream::picture_header_offset(data) else {
            gst::element_imp_error!(
                self,
                gst::StreamError::Format,
                ["Buffer does not start with an H.261 picture start code"]
            );
            return Err(gst::FlowError::Error);
        };

        let offset = self.state.borrow().offset as i32;
        let bits = shift_bits(data, offset - psc_offset as i32);
        // shift_bits always appends PAD_BYTES zero bytes.
        let data_bits = (bits.len() - PAD_BYTES) * 8;

        let mut cursor = BitCursor::new(&bits, data_bits);
        cursor.set_pos(offset as usize);

        let num_gobs = match bitstream::parse_picture_header(&mut cursor) {
            Ok(num_gobs) => num_gobs,
            Err(err) => {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Failed to parse picture header ({err}), dropping buffer"
                );
                self.obj().drop_buffers(id..=id);
                return Ok(gst::FlowSuccess::Ok);
            }
        };

        let gobs = match bitstream::scan_gobs(&bits, data_bits, num_gobs, cursor.pos()) {
            Ok(gobs) => gobs,
            Err(err) => {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Failed to scan group start codes ({err}), dropping buffer"
                );
                self.obj().drop_buffers(id..=id);
                return Ok(gst::FlowSuccess::Ok);
            }
        };

        let max_payload_size = (self.obj().max_payload_size() as usize)
            .checked_sub(PAYLOAD_HEADER_LEN)
            .filter(|size| *size > 0)
            .ok_or_else(|| {
                gst::element_imp_error!(
                    self,
                    gst::LibraryError::Settings,
                    ["MTU too small to add payload header"]
                );
                gst::FlowError::Error
            })?;

        self.packetize(
            &bits,
            offset as usize,
            &gobs,
            &mut cursor,
            max_payload_size,
            id,
        )
    }
}

impl RtpH261Pay {
    /// Split the picture into macroblock-aligned fragments and push one RTP
    /// packet per fragment (RFC 4587 section 3.2 and 4.1).
    fn packetize(
        &self,
        bits: &[u8],
        picture_start: usize,
        gobs: &[Gob],
        cursor: &mut BitCursor,
        max_payload_size: usize,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut startpos = picture_start;
        let mut gob_idx = 0usize;
        // Whether a packet for this input buffer was queued: when not, the
        // base class must be told the buffer produced nothing.
        let mut queued_any = false;
        // State of the last macroblock emitted in the current GOB; `mba`
        // is 0 until the GOB header has been parsed.
        let mut gob_last = Macroblock::default();
        // Last macroblock of the previous packet, for the payload header
        // predictor fields.
        let mut last_mb: Option<Macroblock> = None;

        loop {
            // Latest bit position the packet may end at.
            let limit = (startpos & !7).saturating_add(max_payload_size.saturating_mul(8));
            let mut endpos = limit;

            // A packet whose first bit is a GOB start code must zero the
            // predictor fields (RFC 4587 section 4.1).
            let begins_with_gob_header = startpos == gobs[gob_idx].start;

            // A macroblock sent whole can end past the start of the next
            // GOB, so skip the GOBs the previous packet already covered.
            while gob_idx + 1 < gobs.len() && gobs[gob_idx].end <= startpos {
                gob_idx += 1;
            }

            // Take whole GOBs as long as they fit in the packet.
            while gob_idx + 1 < gobs.len()
                && bitrange_bytes(startpos, gobs[gob_idx].end) <= max_payload_size
            {
                gob_idx += 1;
            }

            // A packet starting at or before the current GOB start takes the
            // GOB header along, and resets the prediction state from it.
            if startpos <= gobs[gob_idx].start {
                cursor.set_pos(gobs[gob_idx].start);
                match bitstream::parse_gob_header(cursor) {
                    Ok(hdr) => {
                        gob_last = Macroblock {
                            mba: 0,
                            gobn: hdr.gn,
                            quant: hdr.gquant,
                            ..Default::default()
                        };
                    }
                    Err(err) => {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Failed to parse GOB header ({err}), dropping remaining data"
                        );
                        self.state.borrow_mut().offset = (startpos % 8) as u8;
                        if !queued_any {
                            self.obj().drop_buffers(id..=id);
                        }
                        return Ok(gst::FlowSuccess::Ok);
                    }
                }
            }

            // Parse macroblocks until the packet is full. The first
            // macroblock is always included, even if it exceeds the limit:
            // a macroblock may not be split across packets.
            let mut marker = false;
            let mut included = 0usize;
            loop {
                match bitstream::parse_mb(cursor, &gob_last) {
                    Ok(MbResult::Parsed(mb)) => {
                        if cursor.pos() > limit {
                            if included > 0 {
                                cursor.set_pos(endpos);
                                break;
                            }
                            // The first macroblock of the GOB being split
                            // does not fit. When the packet already covers
                            // whole earlier GOBs, end at the GOB boundary
                            // instead of overshooting the MTU; a
                            // macroblock may not be split.
                            if startpos < gobs[gob_idx].start {
                                endpos = gobs[gob_idx].start;
                                cursor.set_pos(endpos);
                                break;
                            }
                        }
                        endpos = cursor.pos();
                        gob_last = mb;
                        included += 1;
                    }
                    Ok(MbResult::EndOfGob) => {
                        // The next group start code ends this GOB. An empty
                        // GOB ends up with the header alone in the packet.
                        endpos = cursor.pos();
                        break;
                    }
                    Ok(MbResult::EndOfFrame) => {
                        endpos = cursor.pos();
                        marker = true;
                        break;
                    }
                    Err(err) => {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Failed to parse macroblock layer ({err}), dropping remaining data"
                        );
                        self.state.borrow_mut().offset = (startpos % 8) as u8;
                        if !queued_any {
                            self.obj().drop_buffers(id..=id);
                        }
                        return Ok(gst::FlowSuccess::Ok);
                    }
                }
            }

            // A group start code that does not end the current GOB is left
            // unconsumed by the macroblock parser, so the packet would be
            // empty and the loop would never advance. The C payloader spins
            // forever here; stop and drop the rest of the picture.
            if endpos == startpos {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Unexpected group start code, dropping remaining data"
                );
                if !queued_any {
                    self.obj().drop_buffers(id..=id);
                }
                break;
            }

            let first_byte = startpos / 8;
            let payload = &bits[first_byte..bitrange_bytes(startpos, endpos) + first_byte];

            let header = payload_header(
                startpos,
                endpos,
                if begins_with_gob_header {
                    None
                } else {
                    last_mb
                },
            );

            gst::trace!(
                CAT,
                imp = self,
                "Queueing packet, {} bytes of payload, sbit={}, ebit={}, marker={}",
                payload.len(),
                startpos % 8,
                (8 - (endpos % 8)) % 8,
                marker
            );

            self.obj().queue_packet(
                id.into(),
                rtp_types::RtpPacketBuilder::new()
                    .marker_bit(marker)
                    .payload(&header[..])
                    .payload(payload),
            )?;
            queued_any = true;

            last_mb = Some(gob_last);
            if endpos >= gobs[gob_idx].end {
                gob_idx += 1;
            }
            startpos = endpos;

            if marker {
                break;
            }
            if gob_idx >= gobs.len() {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Picture data ended without a frame terminator, dropping remaining data"
                );
                break;
            }
        }

        // The next picture must be aligned at the bit position where this
        // one ended.
        self.state.borrow_mut().offset = (startpos % 8) as u8;

        Ok(gst::FlowSuccess::Ok)
    }
}

/// Build the 4-byte H.261 payload header (RFC 4587 section 4.1):
///
/// ```text
/// |SBIT |EBIT |I|V| GOBN  |   MBAP  |  QUANT  |  HMVD   |  VMVD   |
/// ```
///
/// `last` is the last macroblock of the previous packet; `None` (or a GOB
/// boundary) means the packet starts with a picture or GOB header, and all
/// predictor fields are zero.
fn payload_header(
    startpos: usize,
    endpos: usize,
    last: Option<Macroblock>,
) -> [u8; PAYLOAD_HEADER_LEN] {
    let sbit = (startpos % 8) as u8;
    let ebit = (8 - (endpos % 8)) as u8 % 8;

    let mut header = [0u8; PAYLOAD_HEADER_LEN];
    // I = 0, V = 1: the constant combination RFC 4587 allows conforming
    // implementations to always send.
    header[0] = (sbit << 5) | (ebit << 2) | 0x01;

    // mba 0 means "no macroblock yet"; mba 33 (a full GOB) is excluded
    // because the biased address 32 would not fit the 5-bit MBAP field,
    // and the packet after a full GOB starts a new GOB whose header
    // carries the state.
    if let Some(last) = last.filter(|mb| mb.mba != 0 && mb.mba != 33) {
        // The macroblock address predictor is biased by -1 to fit 5 bits.
        let mbap = last.mba - 1;
        header[1] = (last.gobn << 4) | (mbap >> 1);
        header[2] = ((mbap & 1) << 7) | (last.quant << 2) | (((last.mvx >> 3) as u8) & 0x03);
        header[3] = (((last.mvx & 7) as u8) << 5) | ((last.mvy as u8) & 0x1f);
    }

    header
}

/// Number of bytes covering the bit range `[first, last)`.
fn bitrange_bytes(first: usize, last: usize) -> usize {
    debug_assert!(first <= last);
    last.div_ceil(8) - first / 8
}

/// Repack `data` so that its first bit lands at bit position `shift`
/// (positive shifts insert leading zero bits, negative shifts drop leading
/// bits). The result is always padded with `PAD_BYTES` zero bytes past the
/// data for safe table peeks.
fn shift_bits(data: &[u8], shift: i32) -> Vec<u8> {
    // The shift is a picture start code bit offset, strictly within a byte.
    debug_assert!((-8..8).contains(&shift));
    let newlen = if shift > 0 {
        data.len() + 1
    } else {
        data.len()
    };
    let mut out = vec![0u8; newlen + PAD_BYTES];

    match shift {
        0 => out[..data.len()].copy_from_slice(data),
        s if s > 0 => {
            let s = s as u32;
            for (out, &b) in out.iter_mut().zip(data) {
                *out |= b >> s;
            }
            for (out, &b) in out.iter_mut().skip(1).zip(data) {
                *out |= b << (8 - s);
            }
        }
        s => {
            let s = (-s) as u32;
            for (out, &b) in out.iter_mut().zip(data) {
                *out |= b << s;
            }
            for (out, &b) in out[..newlen].iter_mut().skip(1).zip(data) {
                *out |= b >> (8 - s);
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h261::bitstream::Macroblock;

    /// The predictor fields follow RFC 4587 section 4.1: zero without a
    /// predictor, biased by -1 in MBAP, 5-bit two's complement for the
    /// motion vector differences, and suppressed after a full GOB.
    #[test]
    fn payload_header_predictor_fields() {
        assert_eq!(&payload_header(0, 80, None)[1..], &[0, 0, 0]);
        // sbit=4, ebit=4, I=0, V=1.
        assert_eq!(payload_header(4, 188, None)[0], 0x91);

        let mb = Macroblock {
            mba: 20,
            gobn: 2,
            quant: 13,
            mvx: -3,
            mvy: 7,
            ..Default::default()
        };
        assert_eq!(&payload_header(0, 80, Some(mb))[1..], &[0x29, 0xB7, 0xA7]);

        let mb = Macroblock {
            mba: 33,
            ..Default::default()
        };
        assert_eq!(&payload_header(0, 80, Some(mb))[1..], &[0, 0, 0]);
    }
}
