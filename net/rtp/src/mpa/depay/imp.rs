// GStreamer RTP MPEG Audio Depayloader
//
// Copyright (C) 2023-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpmpadepay2
 * @see_also: rtpmpapay2, rtpmpadepay, rtpmpapay, mpg123audiodec, lamemp3enc
 *
 * Depayload an MPEG Audio Elementary Stream from RTP packets as per [RFC 2038][rfc-2038]
 * and [RFC 2250][rfc-2250].
 *
 * [rfc-2038]: https://www.rfc-editor.org/rfc/rfc2038.html#section-3.5
 * [rfc-2250]: https://www.rfc-editor.org/rfc/rfc2250.html#section-3.2
 *
 * ## Example pipeline
 *
 * ```shell
 * gst-launch-1.0 udpsrc caps='application/x-rtp,media=audio,clock-rate=90000,encoding-name=MPA,payload=96' ! rtpjitterbuffer latency=250 ! rtpmpadepay2 ! decodebin3 ! audioconvert ! audioresample ! autoaudiosink
 * ```
 *
 * This will depayload an incoming RTP MPEG audio elementary stream (e.g. mp3).
 * You can use the #rtpmpapay2 or #rtpmpapay elements to create such an RTP stream.
 *
 * Since: plugins-rs-0.16.0
 */
use atomic_refcell::AtomicRefCell;

use gst::{glib, prelude::*, subclass::prelude::*};
use std::fmt::Debug;
use std::ops::RangeInclusive;
use std::sync::LazyLock;

use crate::basedepay::{Packet, PacketToBufferRelation, RtpBaseDepay2Ext, RtpBaseDepay2Impl};

use crate::mpa::mpeg_audio_utils::PeekData::{FramedData, PartialData};
use crate::mpa::mpeg_audio_utils::{FrameHeader, peek_frame_header};

#[derive(Default)]
pub struct RtpMpegAudioDepay {
    state: AtomicRefCell<State>,
}

struct FragmentedFrame {
    data: Vec<u8>,
    seqnums: RangeInclusive<u64>,
    ext_timestamp: u64,
    marker: bool,
    expected_len: Option<usize>, // None for freeformat mp3 frame. IMPROVE: could use NonZeroUsize
}

impl Debug for FragmentedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FragmentedFrame")
            .field(
                "data",
                &format_args!(
                    "{} bytes {}..",
                    self.data.len(),
                    // FIXME: can drop the as_slice() here once https://gitlab.freedesktop.org/gstreamer/gstreamer-rs/-/merge_requests/1650/diffs is in
                    self.data.as_slice().dump_range(..4)
                ),
            )
            .field("seqnum", &self.seqnums)
            .field("ext_timestamp", &self.ext_timestamp)
            .field("marker", &self.marker)
            .field("expected_len", &self.expected_len)
            .finish()
    }
}

#[derive(Default)]
struct State {
    last_frame_header: Option<FrameHeader>,
    partial_frame: Option<FragmentedFrame>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpmpadepay2",
        gst::DebugColorFlags::empty(),
        Some("RTP MPEG Audio Depayloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpMpegAudioDepay {
    const NAME: &'static str = "GstRtpMpegAudioDepay";
    type Type = super::RtpMpegAudioDepay;
    type ParentType = crate::basedepay::RtpBaseDepay2;
}

impl ObjectImpl for RtpMpegAudioDepay {}

impl GstObjectImpl for RtpMpegAudioDepay {}

impl ElementImpl for RtpMpegAudioDepay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP MPEG Audio Depayloader",
                "Codec/Depayloader/Network/RTP",
                "Depayload an MPEG Audio Elementary Stream from RTP packets (RFC 2038, RFC 2250)",
                "Tim-Philipp Müller <tim centricular com>",
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
                        // clock-rate should be 90000 according to spec, but Wowza
                        // sometimes sends other rates, or at least used to back when.
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "audio")
                            .field("clock-rate", gst::IntRange::new(1i32, i32::MAX))
                            .field("encoding-name", "MPA")
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "audio")
                            .field("payload", 14i32)
                            .field("clock-rate", 90000i32)
                            .build(),
                    )
                    .build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("audio/mpeg")
                    .field("mpegversion", 1i32)
                    .field("mpegaudioversion", gst::IntRange::new(1, 3))
                    .field("layer", gst::IntRange::new(1, 3))
                    .field("channels", gst::IntRange::new(1, 2))
                    .field("rate", gst::IntRange::new(8000, 48000))
                    .field("parsed", true)
                    .build(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl RtpBaseDepay2Impl for RtpMpegAudioDepay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["audio"];

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();

        Ok(())
    }

    fn drain(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        // Discard any partial frame data we might still have around UNLESS it was a freeformat
        // frame where we don't know the length, in which case we'll assume that the frame is
        // finished on drain. That assumption might be wrong, but then decoders will just skip it.
        if let Some(partial_frame) = state.partial_frame.as_ref() {
            let free_format = partial_frame.expected_len.is_none();

            if free_format {
                gst::log!(
                    CAT,
                    imp = self,
                    "Finishing freeformat frame {:?}",
                    partial_frame
                );
                self.finish_fragmented_frame(&mut state, false)?;
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }

    // Encapsulation of MPEG Audio Elementary Streams:
    // https://www.rfc-editor.org/rfc/rfc2038.html#section-3.5
    // https://www.rfc-editor.org/rfc/rfc2250.html#section-3.2
    //
    // We either get 1-N whole mpeg audio frames in an RTP packet,
    // or a single mpeg audio frame split over multiple RTP packets.
    fn handle_packet(&self, packet: &Packet) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();

        if packet.discont() {
            state.partial_frame = None;
        }

        let payload = packet.payload();

        if payload.len() < 4 {
            gst::warning!(
                CAT,
                imp = self,
                "Payload too small: {} bytes, but need at least 4 bytes",
                payload.len()
            );
            state.partial_frame = None;
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        let frag_offset = u16::from_be_bytes([payload[2], payload[3]]) as usize;

        // Skip header
        let payload = &payload[4..];

        if frag_offset != 0 {
            gst::log!(
                CAT,
                imp = self,
                "Payload is continuation of previous packet, fragment offset {frag_offset}. \
                 Current partial frame: {:?}",
                state.partial_frame,
            );

            return self.handle_continuation_fragment(&mut state, packet, frag_offset, payload);
        }

        // Start of new frame(s)

        // Discard any partial frame data we might still have around UNLESS it was a freeformat
        // frame where we don't know the length, so had to wait for the next packet with
        // frag_offset 0 to know that the frame is finished. For non-freeformat this shouldn't
        // ever happen unless the stream is malformed, as in case of missing packets we would have
        // purged any pending partial frame already based on the packet's discont flag above.
        if let Some(partial_frame) = state.partial_frame.as_ref() {
            let free_format = partial_frame.expected_len.is_none();

            if free_format {
                gst::log!(
                    CAT,
                    imp = self,
                    "Finishing freeformat frame {:?}",
                    partial_frame
                );
                self.finish_fragmented_frame(&mut state, false)?;
            } else {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Dropping unfinished partial frame {:?}",
                    partial_frame
                );

                self.obj().drop_packets(partial_frame.seqnums.clone());
            }

            state.partial_frame = None;
        }

        // Check mpeg audio frame header
        let Ok(frame_header) = peek_frame_header(PartialData(payload)) else {
            gst::warning!(
                CAT,
                imp = self,
                "Could not parse frame header, dropping packet"
            );
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        };

        // Set or update output caps
        if state.last_frame_header.as_ref() != Some(&frame_header) {
            self.update_src_caps(&frame_header);
        }

        // Start of a fragmented frame or unfragmented payload?
        //
        match frame_header.frame_len {
            // No length means it's a single or partial freeformat frame where we won't know
            // if it's finished until we see the start of the next unit (frag_offset 0).
            None => self.handle_initial_fragment(&mut state, packet, &frame_header, payload),

            // Payloader shorter than expected frame length? -> fragmented frame
            Some(expected_frame_len) if expected_frame_len > payload.len() => {
                self.handle_initial_fragment(&mut state, packet, &frame_header, payload)
            }

            // .. otherwise we have an unfragmented payload, i.e. one or more complete frames
            _ => self.handle_unfragmented_payload(&mut state, packet, payload),
        }
    }
}

impl RtpMpegAudioDepay {
    fn finish_fragmented_frame(
        &self,
        state: &mut State,
        marker_bit: bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let partial_frame = state.partial_frame.take().unwrap();

        // This worked for the start of the fragmented frame, so can't really fail now
        let frame_header = peek_frame_header(FramedData(&partial_frame.data)).unwrap();

        let expected_len = frame_header.frame_len.unwrap();

        // Too much data?
        if partial_frame.data.len() > expected_len {
            gst::warning!(
                CAT,
                imp = self,
                "Partial frame finished, but have {} bytes more data than expected!",
                partial_frame.data.len() - expected_len,
            );
        }

        let mut outbuf = gst::Buffer::from_mut_slice(partial_frame.data);

        let outbuf_ref = outbuf.get_mut().unwrap();

        // Single frame
        outbuf_ref.set_duration(
            (frame_header.samples_per_frame as u64)
                .mul_div_floor(*gst::ClockTime::SECOND, frame_header.sample_rate as u64)
                .map(gst::ClockTime::from_nseconds),
        );

        state.last_frame_header = Some(frame_header);

        // Marker flag indicates start of talkspurt
        if partial_frame.marker || marker_bit {
            outbuf_ref.set_flags(gst::BufferFlags::RESYNC);
        }

        gst::trace!(CAT, imp = self, "Finishing buffer {outbuf:?}");

        self.obj().queue_buffer(
            PacketToBufferRelation::Seqnums(partial_frame.seqnums),
            outbuf,
        )
    }

    fn handle_continuation_fragment(
        &self,
        state: &mut State,
        packet: &Packet,
        frag_offset: usize,
        payload: &[u8],
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let Some(partial_frame) = state.partial_frame.as_mut() else {
            // This can happen if we lost the packet(s) with earlier fragments of this frame.
            gst::debug!(
                CAT,
                imp = self,
                "Frame fragment, but no partial frame pending"
            );
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        };

        // This would indicate a malformed stream, as in case of missing packets we would
        // have purged any pending partial frame already based on the packet's discont flag.
        if partial_frame.data.len() != frag_offset {
            gst::warning!(
                CAT,
                imp = self,
                "Frame fragment with offset {frag_offset}, but pending partial frame has {} bytes.",
                partial_frame.data.len(),
            );
            state.partial_frame = None;
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        // This would indicate a malformed stream, as in case of missing packets we would
        // have purged any pending partial frame already based on the packet's discont flag.
        if partial_frame.ext_timestamp != packet.ext_timestamp() {
            gst::warning!(
                CAT,
                imp = self,
                "Frame continuation fragment timestamp {} doesn't match existing partial fragment timestamp {}",
                packet.ext_timestamp(),
                partial_frame.ext_timestamp,
            );
            state.partial_frame = None;
            self.obj().drop_packet(packet);
            return Ok(gst::FlowSuccess::Ok);
        }

        partial_frame.data.extend_from_slice(payload);

        // Update end seqnum
        partial_frame.seqnums = *partial_frame.seqnums.start()..=packet.ext_seqnum();

        // If we don't know the expected length (freeformat mp3) we need to wait for the start
        // of the next frame (frag_offset 0) to know when the current partial frame is finished.
        let Some(expected_len) = partial_frame.expected_len else {
            gst::log!(
                CAT,
                imp = self,
                "Partial freeformat frame, waiting for more data or start of next frame {partial_frame:?}"
            );
            return Ok(gst::FlowSuccess::Ok);
        };

        // Check if we have enough data now
        if partial_frame.data.len() < expected_len {
            gst::log!(
                CAT,
                imp = self,
                "Partial frame still unfinished, waiting for more data {partial_frame:?}"
            );
            return Ok(gst::FlowSuccess::Ok);
        }

        // Fragmented frame is complete
        self.finish_fragmented_frame(state, packet.marker_bit())
    }

    fn handle_initial_fragment(
        &self,
        state: &mut State,
        packet: &Packet,
        frame_header: &FrameHeader,
        payload: &[u8],
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if frame_header.frame_len.is_none() {
            gst::log!(
                CAT,
                imp = self,
                "Start of (possibly fragmented) freeformat frame, packet {packet:?}"
            );
        } else {
            gst::log!(
                CAT,
                imp = self,
                "Start of fragmented frame, packet {packet:?}"
            );
        }

        let initial_capacity = match frame_header.frame_len {
            Some(frame_len) => frame_len,
            None => 2 * payload.len(),
        };
        let mut data = Vec::with_capacity(initial_capacity);
        data.extend_from_slice(payload);

        state.partial_frame = Some(FragmentedFrame {
            data,
            seqnums: packet.ext_seqnum()..=packet.ext_seqnum(),
            ext_timestamp: packet.ext_timestamp(),
            marker: packet.marker_bit(),
            expected_len: frame_header.frame_len,
        });

        state.last_frame_header = Some(frame_header.clone());

        gst::trace!(CAT, imp = self, "Partial frame {:?}", state.partial_frame);

        Ok(gst::FlowSuccess::Ok)
    }

    fn handle_unfragmented_payload(
        &self,
        state: &mut State,
        packet: &Packet,
        payload: &[u8],
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut payload = payload;

        // One or more complete frames!
        let mut n_frames = 0;

        // We handled the single freeformat frame without known length case above, so here we now
        // have either a single normal frame, multiple normal frames, or multiple freeformat frames.
        while let Ok(frame_header) = peek_frame_header(FramedData(payload)) {
            n_frames += 1;

            let format_changed = state.last_frame_header.as_ref() != Some(&frame_header);

            gst::log!(
                CAT,
                imp = self,
                "Frame {n_frames}: {frame_header:?}, {}",
                if format_changed { "format_changed" } else { "" },
            );

            if format_changed {
                self.update_src_caps(&frame_header);

                state.last_frame_header = Some(frame_header.clone());
            }

            let frame_len = frame_header.frame_len.expect("frame_len");

            // "Multiple audio frames may be encapsulated within one RTP packet. In this case,
            //  an integral number of audio frames must be contained within the packet"
            if frame_len > payload.len() {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Not enough data for frame of length {}, only {} bytes of payload left",
                    frame_len,
                    payload.len(),
                );
                break;
            }

            (_, payload) = payload.split_at(frame_len);
        }

        gst::log!(
            CAT,
            imp = self,
            "Packet with {n_frames} MPEG audio frame(s)"
        );

        if !payload.is_empty() {
            gst::warning!(
                CAT,
                imp = self,
                "{} bytes leftover in payload",
                payload.len()
            );
        }

        // Push out everything we got
        let mut outbuf = packet.payload_subbuffer(4..);

        let outbuf_ref = outbuf.get_mut().unwrap();

        // We assume all frames in the packet have the same number of samples and sample rate
        outbuf_ref.set_duration(
            (n_frames as u64 * first_frame_header.samples_per_frame as u64)
                .mul_div_floor(
                    *gst::ClockTime::SECOND,
                    first_frame_header.sample_rate as u64,
                )
                .map(gst::ClockTime::from_nseconds),
        );

        // Marker flag indicates start of talkspurt
        if packet.marker_bit() {
            outbuf_ref.set_flags(gst::BufferFlags::RESYNC);
        }

        gst::trace!(CAT, imp = self, "Finishing buffer {outbuf:?}");

        self.obj().queue_buffer(packet.into(), outbuf)
    }

    fn update_src_caps(&self, new_frame_header: &FrameHeader) {
        let src_caps = gst::Caps::builder("audio/mpeg")
            .field("mpegversion", 1i32)
            .field("mpegaudioversion", new_frame_header.version as i32)
            .field("layer", new_frame_header.layer as i32)
            .field("rate", new_frame_header.sample_rate as i32)
            .field("channels", new_frame_header.channels as i32)
            .field("parsed", true)
            .build();

        gst::info!(CAT, imp = self, "Setting output caps {src_caps}..");
        self.obj().set_src_caps(&src_caps);
    }
}
