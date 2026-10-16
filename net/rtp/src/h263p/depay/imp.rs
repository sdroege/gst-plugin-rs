// GStreamer RTP H.263+ Depayloader
//
// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtph263pdepay2
 * @see_also: rtph263ppay2, avdec_h263
 *
 * Extracts an H.263+ (H.263-1998) or H.263++ (H.263-2000) video stream from
 * RTP packets as per [RFC 4629][rfc-4629].
 *
 * [rfc-4629]: https://www.rfc-editor.org/rfc/rfc4629.html
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 udpsrc2 port=5004 caps="application/x-rtp,media=video,encoding-name=H263-1998,clock-rate=90000" ! rtph263pdepay2 ! avdec_h263 ! autovideosink
 * ]| This will receive RTP packets on localhost port 5004 and decode the
 * H.263+ stream inside.
 *
 * Since: plugins-rs-0.16.0
 */
use atomic_refcell::AtomicRefCell;

use bitstream_io::{BigEndian, BitRead as _, BitReader};

use gst::{glib, subclass::prelude::*};

use std::sync::LazyLock;

use crate::basedepay::{PacketToBufferRelation, RtpBaseDepay2Ext, RtpBaseDepay2Impl};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtph263pdepay2",
        gst::DebugColorFlags::empty(),
        Some("RTP H.263+ Depayloader"),
    )
});

/// Frame that is currently being assembled from packet payloads.
struct PendingFrame {
    start_ext_seqnum: u64,
    ext_timestamp: u64,
    data: Vec<u8>,
}

struct State {
    /// Set until a packet with the P bit has been seen, i.e. until the start of
    /// a segment has been observed.
    wait_start: bool,
    pending_frame: Option<PendingFrame>,
}

impl Default for State {
    fn default() -> Self {
        State {
            wait_start: true,
            pending_frame: None,
        }
    }
}

#[derive(Default)]
pub struct RtpH263PDepay {
    state: AtomicRefCell<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for RtpH263PDepay {
    const NAME: &'static str = "GstRtpH263PDepay2";
    type Type = super::RtpH263PDepay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpH263PDepay {}

impl GstObjectImpl for RtpH263PDepay {}

impl ElementImpl for RtpH263PDepay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP H.263+ Depayloader",
                "Codec/Depayloader/Network/RTP",
                "Extracts an H.263+/H.263++ video stream from RTP packets (RFC 4629)",
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
                &gst::Caps::builder_full()
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("encoding-name", "H263-1998")
                            .field("clock-rate", 90_000i32)
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "video")
                            .field("encoding-name", "H263-2000")
                            .field("clock-rate", 90_000i32)
                            .build(),
                    )
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/x-h263")
                    .field("variant", "itu")
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl RtpBaseDepay2Impl for RtpH263PDepay {
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
        // Incomplete frames are dropped; a frame can only be completed with
        // its final (marker) packet.
        self.state.borrow_mut().pending_frame = None;

        Ok(gst::FlowSuccess::Ok)
    }

    fn flush(&self) {
        *self.state.borrow_mut() = State::default();
    }

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let s = caps.structure(0).unwrap();

        // The sink pad template pins 'encoding-name' to a fixed string.
        let encoding_name = s.get::<&str>("encoding-name").unwrap();

        let h263version = if encoding_name.eq_ignore_ascii_case("H263-2000") {
            "h263pp"
        } else if encoding_name.eq_ignore_ascii_case("H263-1998") {
            // H263-1998 is H.263+ if any of the SDP parameters announcing
            // H.263+ annexes is present (RFC 4629 section 9).
            let plus_annex = ["f", "i", "j", "t"]
                .iter()
                .any(|param| s.get::<&str>(*param).is_ok_and(|v| v == "1"))
                || ["k", "n", "p"]
                    .iter()
                    .any(|param| s.get::<&str>(*param).is_ok());
            if plus_annex { "h263p" } else { "h263" }
        } else {
            gst::error!(
                CAT,
                imp = self,
                "Unsupported encoding-name '{encoding_name}'"
            );
            return false;
        };

        let src_caps = gst::Caps::builder("video/x-h263")
            .field("variant", "itu")
            .field("h263version", h263version)
            .build();
        gst::debug!(CAT, imp = self, "Setting src caps {src_caps:?}");
        self.obj().set_src_caps(&src_caps);

        true
    }

    fn handle_packet(
        &self,
        packet: &crate::basedepay::Packet,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        if packet.discont() {
            gst::debug!(CAT, imp = self, "Discontinuity, dropping incomplete frame");
            state.pending_frame = None;
            state.wait_start = true;
        }

        let payload = packet.payload();
        if payload.len() < 2 {
            gst::warning!(
                CAT,
                imp = self,
                "Dropping packet with payload too small for payload header"
            );
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        // General H.263+ payload header (RFC 4629 section 5.1):
        //
        //  0                   1
        //  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5
        // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        // |   RR    |P|V|   PLEN    |PEBIT|
        // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
        let p = payload[0] & 0x04 != 0;
        let v = payload[0] & 0x02 != 0;
        let plen = (((payload[0] & 0x01) as usize) << 5) | ((payload[1] >> 3) as usize);
        let _pebit = payload[1] & 0x07;

        // FIXME: the attached picture header (PLEN > 0) could restore picture
        // parameters when the picture start is lost (RFC 4629 sections 5.1
        // and 6.1); it is currently skipped along with the VRC byte.
        let header_len = 2 + usize::from(v) + plen;
        // The payload must hold the payload header plus at least one data
        // byte (with P=1 the third start code byte, with P=0 continuation
        // data).
        if payload.len() <= header_len {
            gst::warning!(
                CAT,
                imp = self,
                "Dropping packet with payload too small for payload header"
            );
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        // A marker packet of the previous frame may have been lost: a packet
        // with a different timestamp means the pending frame is incomplete
        // and will never complete. The same applies to a follow-on packet
        // (P=0), which never starts a frame: appending it would splice two
        // frames together.
        if let Some(pending) = &state.pending_frame
            && pending.ext_timestamp != packet.ext_timestamp()
        {
            gst::warning!(
                CAT,
                imp = self,
                "RTP timestamp changed before frame completion, dropping incomplete frame"
            );
            state.pending_frame = None;
        }

        if p {
            state.wait_start = false;
        } else if state.wait_start || state.pending_frame.is_none() {
            gst::debug!(
                CAT,
                imp = self,
                "Waiting for picture start, dropping packet"
            );
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        let pending_frame = state.pending_frame.get_or_insert(PendingFrame {
            start_ext_seqnum: packet.ext_seqnum(),
            ext_timestamp: packet.ext_timestamp(),
            data: Vec::new(),
        });

        // A packet with P=1 has the two leading zero bytes of its start code
        // omitted; restore them.
        if p {
            pending_frame.data.extend_from_slice(&[0x00, 0x00]);
        }
        pending_frame.data.extend_from_slice(&payload[header_len..]);

        if !packet.marker_bit() {
            return Ok(gst::FlowSuccess::Ok);
        }

        let pending_frame = state.pending_frame.take().expect("no pending frame");
        let mut buffer = gst::Buffer::from_mut_slice(pending_frame.data);
        let intra = picture_is_intra(buffer.map_readable().unwrap().as_slice());
        // A fresh buffer carries no flags, so only inter pictures are flagged.
        if matches!(intra, Some(false)) {
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

/// Parse the H.263 picture header and return whether the picture is intra
/// coded.
///
/// Returns `None` if the header could not be parsed.
fn picture_is_intra(data: &[u8]) -> Option<bool> {
    let mut r = BitReader::endian(data, BigEndian);

    // PSC (H.263 5.1.1): 22 bits, fixed value 0000 0000 0000 0000 1 00000.
    if r.read::<22, u32>().ok()? != 0x20 {
        gst::trace!(CAT, "No picture start code");
        return None;
    }
    // TR (5.1.2): 8 bits.
    let _tr = r.read::<8, u8>().ok()?;

    // PTYPE (5.1.3): bits 1-2 are '10'; bits 6-8 are the source format,
    // '111' selects the extended PTYPE.
    let ptype = r.read::<8, u8>().ok()?;
    if ptype & 0xC0 != 0x80 {
        return None;
    }

    if ptype & 0x07 != 0x07 {
        // Baseline PTYPE: bit 9 is the picture coding type, "0" INTRA.
        let rest = r.read::<5, u8>().ok()?;
        return Some(rest & 0x10 == 0);
    }

    // Extended PTYPE: PLUSPTYPE (5.1.4) = UFEP (3 bits), optional OPPTYPE
    // (18 bits, only when UFEP is "001"), MPPTYPE (9 bits).
    let ufep = r.read::<3, u8>().ok()?;
    if ufep > 1 {
        return None;
    }
    if ufep == 1 {
        let _opptype = r.read::<18, u32>().ok()?;
    }
    let mpptype = r.read::<9, u16>().ok()?;
    // Bits 7-9 are '0', '0' and the emulation prevention '1'.
    if mpptype & 0x07 != 0x01 {
        return None;
    }
    // Bits 1-3 are the picture type code: "000" is INTRA, "100" is Annex O
    // enhanced intra (self-contained, decodes without a reference picture).
    Some(matches!(mpptype >> 6, 0 | 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bit layout: PSC (22 bits, fixed 0000 0000 0000 0000 1 00000) + TR (8
    // bits) + PTYPE. PTYPE starts at bit 30, so it is not byte aligned.
    //
    // Baseline, TR=1, SRC='010' (QCIF), INTRA:
    // b2 = 1 00000 | TR[7:6], b3 = TR[5:0] | '10', b4 = '000' '010' '0' '0',
    // b5 = mode bits.
    const BASELINE_INTRA: [u8; 6] = [0x00, 0x00, 0x80, 0x06, 0x08, 0x00];

    #[test]
    fn intra_detection_baseline() {
        assert_eq!(picture_is_intra(&BASELINE_INTRA), Some(true));

        // Same but coding type bit 9 = '1' (INTER): b4 = '000' '010' '1' '0'.
        let inter = [0x00, 0x00, 0x80, 0x06, 0x0A, 0x00];
        assert_eq!(picture_is_intra(&inter), Some(false));
    }

    #[test]
    fn intra_detection_extended() {
        // PTYPE bits 6-8 = '111' selects extended PTYPE (b4 = '000111' +
        // UFEP[1:2]). UFEP = "000", MPPTYPE = '000'(I) '0000' '001'.
        let intra = [0x00, 0x00, 0x80, 0x06, 0x1C, 0x00, 0x40];
        assert_eq!(picture_is_intra(&intra), Some(true));

        // MPPTYPE = '001'(P) '0000' '001'.
        let inter = [0x00, 0x00, 0x80, 0x06, 0x1C, 0x10, 0x40];
        assert_eq!(picture_is_intra(&inter), Some(false));

        // MPPTYPE = '100'(Annex O enhanced intra) '0000' '001'.
        let ei = [0x00, 0x00, 0x80, 0x06, 0x1C, 0x40, 0x40];
        assert_eq!(picture_is_intra(&ei), Some(true));

        // UFEP = "001": OPPTYPE (18 bits, zero) precedes MPPTYPE.
        let intra_ufep1 = [0x00, 0x00, 0x80, 0x06, 0x1C, 0x80, 0x00, 0x00, 0x10];
        assert_eq!(picture_is_intra(&intra_ufep1), Some(true));
    }

    #[test]
    fn invalid_picture_headers() {
        // No picture start code
        assert_eq!(
            picture_is_intra(&[0x00, 0x00, 0x00, 0x06, 0x08, 0x00]),
            None
        );
        // PTYPE bits 1-2 are not '10'
        assert_eq!(
            picture_is_intra(&[0x00, 0x00, 0x80, 0x04, 0x08, 0x00]),
            None
        );
        // Too short
        assert_eq!(picture_is_intra(&[0x00, 0x00, 0x80]), None);
        // Invalid UFEP (> 1): UFEP = "010"
        assert_eq!(
            picture_is_intra(&[0x00, 0x00, 0x80, 0x06, 0x1D, 0x00, 0x40]),
            None
        );
        // MPPTYPE without the emulation prevention bit 9
        assert_eq!(
            picture_is_intra(&[0x00, 0x00, 0x80, 0x06, 0x1C, 0x00, 0x00]),
            None
        );
    }
}
