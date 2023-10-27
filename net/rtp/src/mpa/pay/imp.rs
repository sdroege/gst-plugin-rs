// GStreamer RTP MPEG Audio Payloader
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpmpapay2
 * @see_also: rtpmpapay2, rtpmpapay, rtpmpapay, mpg123audiodec, lamemp3enc
 *
 * Payload an MPEG Audio Elementary Stream into RTP packets as per [RFC 2038][rfc-2038]
 * and [RFC 2250][rfc-2250]. Also see the [IANA media-type page for MPA][iana-mpa].
 *
 * [rfc-2038]: https://www.rfc-editor.org/rfc/rfc2038.html#section-3.5
 * [rfc-2250]: https://www.rfc-editor.org/rfc/rfc2250.html#section-3.2
 * [iana-mpa]: https://www.iana.org/assignments/media-types/audio/MPA
 *
 * ## Aggregation Modes
 *
 * The default aggregation mode is `auto`: If upstream is live, the payloader will send out all
 * audio frames immediately, even if they don't completely fill a packet, in order to minimise
 * latency. If upstream is not live, the payloader will by default aggregate audio frames until
 * it has completely filled an RTP packet as per the configured MTU size or the `max-ptime`
 * property if it is set (it is not set by default).
 *
 * The aggregation mode can be controlled via the `aggregate-mode` property.
 *
 * ## Example pipeline
 *
 * ```shell
 * gst-launch-1.0 audiotestsrc wave=ticks ! lamemp3enc ! mpegaudioparse ! rtpmpapay2 ! udpsink host=127.0.0.1 port=5004
 * ```
 *
 * This will encode an audio test signal to MP3 and then payload the encoded audio
 * into RTP packets and send them out via UDP to localhost (IPv4) port 5004.
 * You can use the #rtpmpadepay2 or #rtpmpadepay elements to depayload such a stream, and
 * the #mpg123audiodec or avdec_mp3 element to decode the depayloaded stream.
 *
 * Since: plugins-rs-0.16.0
 */
use atomic_refcell::AtomicRefCell;

use std::collections::VecDeque;

use std::sync::{Arc, Mutex};

use gst::{glib, prelude::*, subclass::prelude::*};

use std::sync::LazyLock;

use crate::basepay::{
    PacketToBufferRelation, RtpBasePay2Ext, RtpBasePay2Impl, RtpBasePay2ImplExt, TimestampOffset,
};

use crate::mpa::mpeg_audio_utils::PeekData::FramedData;
use crate::mpa::mpeg_audio_utils::{FrameHeader, peek_frame_header};

use super::RtpMpegAudioPayAggregateMode;

const RTP_MPA_DEFAULT_PT: u32 = 14;

// https://www.rfc-editor.org/rfc/rfc2250#section-3.5
const AUDIO_SPECIFIC_HEADER_LEN: usize = 4;

#[derive(Clone)]
struct Settings {
    max_ptime: Option<gst::ClockTime>,
    aggregate_mode: RtpMpegAudioPayAggregateMode,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            aggregate_mode: RtpMpegAudioPayAggregateMode::Auto,
            max_ptime: None,
        }
    }
}

#[derive(Default)]
pub struct RtpMpegAudioPay {
    state: AtomicRefCell<State>,
    settings: Mutex<Settings>,
    is_live: Mutex<Option<bool>>,
}

#[derive(Debug)]
struct QueuedFrame {
    // Id of the input buffer this frame came from
    id: u64,

    // Time offset to the timestamp of the buffer this input buffer came from
    // (will be non-zero if the input buffer contained multiple audio frames)
    pts_offset: u64,

    // Mapped buffer data and offset into the buffer data
    buffer: Arc<gst::MappedBuffer<gst::buffer::Readable>>,
    offset: usize,

    // Audio frame header
    header: FrameHeader,
}

impl QueuedFrame {
    fn duration(&self) -> u64 {
        self.header.duration()
    }

    fn len(&self) -> usize {
        // We always feed whole frames, so this will always be set even for freeformat mp3s.
        self.header.frame_len.unwrap()
    }

    fn data(&self) -> &[u8] {
        let end_offset = self.offset + self.len();
        &self.buffer[self.offset..end_offset]
    }
}

#[derive(Default)]
struct State {
    // Queued audio frames (we collect until min-ptime/max-ptime is hit or the packet is full)
    queued_frames: VecDeque<QueuedFrame>,

    // Desired "packet time", i.e. packet duration, from the downstream caps, if set
    ptime: Option<gst::ClockTime>,
    max_ptime: Option<gst::ClockTime>,

    discont_pending: bool,
}

impl State {
    fn discont_pending(&mut self) -> bool {
        // Clear discont_pending and return current value
        std::mem::replace(&mut self.discont_pending, false)
    }
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpmpapay2",
        gst::DebugColorFlags::empty(),
        Some("RTP MPEG Audio Payloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpMpegAudioPay {
    const NAME: &'static str = "GstRtpMpegAudioPay";
    type Type = super::RtpMpegAudioPay;
    type ParentType = crate::basepay::RtpBasePay2;
}

impl ObjectImpl for RtpMpegAudioPay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecEnum::builder_with_default("aggregate-mode", Settings::default().aggregate_mode)
                    .nick("Aggregate Mode")
                    .blurb("Whether to send out audio frames immediately or aggregate them until a packet is full.")
                    .build(),
                // Using same type/semantics as C payloaders
                glib::ParamSpecInt64::builder("max-ptime")
                    .nick("Maximum Packet Time")
                    .blurb("Maximum duration of the packet data in ns (-1 = unlimited up to MTU)")
                    .default_value(
                        Settings::default()
                            .max_ptime
                            .map(gst::ClockTime::nseconds)
                            .map(|x| x as i64)
                            .unwrap_or(-1),
                    )
                    .minimum(-1)
                    .maximum(i64::MAX)
                    .mutable_playing()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "aggregate-mode" => {
                settings.aggregate_mode = value
                    .get::<RtpMpegAudioPayAggregateMode>()
                    .expect("type checked upstream");
            }
            "max-ptime" => {
                let new_max_ptime = match value.get::<i64>().unwrap() {
                    -1 => None,
                    v @ 0.. => Some(gst::ClockTime::from_nseconds(v as u64)),
                    _ => unreachable!(),
                };
                let changed = settings.max_ptime != new_max_ptime;
                settings.max_ptime = new_max_ptime;
                drop(settings);

                if changed {
                    let _ = self
                        .obj()
                        .post_message(gst::message::Latency::builder().src(&*self.obj()).build());
                }
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "aggregate-mode" => settings.aggregate_mode.to_value(),
            "max-ptime" => (settings
                .max_ptime
                .map(gst::ClockTime::nseconds)
                .map(|x| x as i64)
                .unwrap_or(-1))
            .to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        self.obj().set_property("pt", RTP_MPA_DEFAULT_PT);
    }
}

impl GstObjectImpl for RtpMpegAudioPay {}

impl ElementImpl for RtpMpegAudioPay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP MPEG Audio Payloader",
                "Codec/Payloader/Network/RTP",
                "Payload an MPEG Audio Elementary Stream into RTP packets (RFC 2038, RFC 2250)",
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
                &gst::Caps::builder("audio/mpeg")
                    .field("mpegversion", 1i32)
                    .field("parsed", true)
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
                            .field("encoding-name", "MPA")
                            .field("clock-rate", 90000i32)
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "audio")
                            .field("payload", RTP_MPA_DEFAULT_PT as i32)
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

impl RtpBasePay2Impl for RtpMpegAudioPay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["audio"];

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let s = caps.structure(0).unwrap();

        let mut src_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "MPA")
            .field("clock-rate", 90000i32);

        // https://www.iana.org/assignments/media-types/audio/MPA
        if let Ok(layer) = s.get::<i32>("layer") {
            src_caps = src_caps.field("layer", layer.to_string());
        }

        if let Ok(rate) = s.get::<i32>("rate") {
            src_caps = src_caps.field("samplerate", rate.to_string());
        }

        self.obj().set_src_caps(&src_caps.build());

        true
    }

    fn negotiate(&self, mut src_caps: gst::Caps) {
        // Fixate as a first step
        src_caps.fixate();

        let s = src_caps.structure(0).unwrap();

        // Negotiate ptime/maxptime with downstream and use them in combination with the
        // properties. See https://www.iana.org/assignments/media-types/audio/MPA
        let ptime = s
            .get::<u32>("ptime")
            .ok()
            .map(u64::from)
            .map(gst::ClockTime::from_mseconds);

        let max_ptime = s
            .get::<u32>("maxptime")
            .ok()
            .map(u64::from)
            .map(gst::ClockTime::from_mseconds);

        self.parent_negotiate(src_caps);

        let mut state = self.state.borrow_mut();
        state.ptime = ptime;
        state.max_ptime = max_ptime;
        drop(state);
    }

    // Encapsulation of MPEG Audio Elementary Streams:
    // https://www.rfc-editor.org/rfc/rfc2038.html#section-3.5
    // https://www.rfc-editor.org/rfc/rfc2250.html#section-3.2
    //
    // We either put 1-N whole mpeg audio frames into a single RTP packet,
    // or split a single mpeg audio frame over multiple RTP packets.
    //
    fn handle_buffer(
        &self,
        buffer: &gst::Buffer,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();
        let mut settings = self.settings.lock().unwrap();

        // https://www.rfc-editor.org/rfc/rfc2250.html#section-2.1
        // Set marker flag whenever there's a discontinuity (might be timestamp or data).
        // Technically it's supposed to be set at the start of a talk spurt (see RFC 2250 Errata).
        // Todo: could/should do more elaborate timestamp/discont tracking like baseaudiopay does.
        if buffer
            .flags()
            .intersects(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
        {
            gst::debug!(
                CAT,
                imp = self,
                "Discont or marker on input {buffer:?}, pushing out any pending frames"
            );
            self.send_packets(&settings, &mut state, SendPacketMode::ForcePending)?;
            state.discont_pending = true;
        }

        let map = buffer.clone().into_mapped_buffer_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "Can't map buffer readable");
            gst::FlowError::Error
        })?;

        // Arc so we can share the MappedBuffer amongst multiple frames.
        // Todo: could probably do something more clever to avoid the heap
        // allocation in the case where the input buffer contains a single
        // audio frame only (which is probably the normal case).
        let map = Arc::new(map);

        let data = map.as_slice();

        let mut pts_offset = 0;
        let mut map_offset = 0;

        loop {
            let Ok(frame_hdr) = peek_frame_header(FramedData(&data[map_offset..])) else {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Failed to parse MPEG audio frame header for {buffer:?} at offset {map_offset}",
                );

                if map_offset > 0 {
                    break;
                }

                self.send_packets(&settings, &mut state, SendPacketMode::ForcePending)?;
                state.discont_pending = true;

                self.obj().drop_buffers(..=id);
                return Ok(gst::FlowSuccess::Ok);
            };

            let queued_frame = QueuedFrame {
                id,
                pts_offset,
                buffer: map.clone(),
                offset: map_offset,
                header: frame_hdr,
            };

            let frame_len = queued_frame.len();
            let frame_dur = queued_frame.duration();

            if map_offset + frame_len > data.len() {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Short audio frame for {buffer:?} at offset {map_offset}"
                );
            }

            pts_offset += frame_dur;
            map_offset += frame_len;

            state.queued_frames.push_back(queued_frame);

            if map_offset >= data.len() {
                break;
            }
        }

        // Make sure we have queried upstream liveness if needed
        if settings.aggregate_mode == RtpMpegAudioPayAggregateMode::Auto {
            self.ensure_upstream_liveness(&mut settings);
        }

        self.send_packets(&settings, &mut state, SendPacketMode::WhenReady)
    }

    fn drain(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.state.borrow_mut();

        self.send_packets(&settings, &mut state, SendPacketMode::ForcePending)
    }

    fn flush(&self) {
        let mut state = self.state.borrow_mut();
        state.queued_frames.clear();
    }

    #[allow(clippy::single_match)]
    fn src_query(&self, query: &mut gst::QueryRef) -> bool {
        let res = self.parent_src_query(query);
        if !res {
            return false;
        }

        match query.view_mut() {
            gst::QueryViewMut::Latency(query) => {
                let settings = self.settings.lock().unwrap();

                let (is_live, mut min, mut max) = query.result();

                {
                    let mut live_guard = self.is_live.lock().unwrap();

                    if Some(is_live) != *live_guard {
                        gst::info!(CAT, imp = self, "Upstream is live: {is_live}");
                        *live_guard = Some(is_live);
                    }
                }

                if self.effective_aggregate_mode(&settings)
                    == RtpMpegAudioPayAggregateMode::Aggregate
                {
                    if let Some(max_ptime) = settings.max_ptime {
                        min += max_ptime;
                        max.opt_add_assign(max_ptime);
                    } else if is_live {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Aggregating packets in live mode, but no max_ptime configured. \
                            Configured latency may be too low!"
                        );
                    }
                    query.set(is_live, min, max);
                }
            }
            _ => (),
        }

        true
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        *self.is_live.lock().unwrap() = None;

        self.parent_start()
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        *self.is_live.lock().unwrap() = None;

        self.parent_stop()
    }
}

#[derive(Debug, PartialEq)]
enum SendPacketMode {
    WhenReady,
    ForcePending,
}

impl RtpMpegAudioPay {
    fn send_packets(
        &self,
        settings: &Settings,
        state: &mut State,
        send_mode: SendPacketMode,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let agg_mode = self.effective_aggregate_mode(settings);

        let max_payload_size = self.obj().max_payload_size() as usize - AUDIO_SPECIFIC_HEADER_LEN;

        // Send out packets if there's enough data for one (or more), or if forced.
        while let Some(first) = state.queued_frames.front() {
            // Frame length will always be set here since we have whole frames as input
            let frame_len = first.len();

            // Big audio frame that needs to be split across multiple packets?
            if frame_len > max_payload_size {
                let first = state.queued_frames.pop_front().unwrap();
                let mut data = first.buffer.as_slice();
                let mut frag_offset = 0;
                let id = first.id;

                while frag_offset < frame_len {
                    let left = frame_len - frag_offset;
                    let bytes_in_this_packet = std::cmp::min(left, max_payload_size);

                    // 0                   1                   2                   3
                    // 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
                    // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
                    // |       MBZ (must be zero)      |          Frag_offset          |
                    // +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
                    let audio_specific_header = (frag_offset as u32).to_be_bytes();

                    self.obj().queue_packet(
                        id.into(),
                        rtp_types::RtpPacketBuilder::new()
                            .payload(audio_specific_header.as_slice())
                            .payload(&data[0..bytes_in_this_packet])
                            .marker_bit(state.discont_pending()),
                    )?;

                    data = &data[bytes_in_this_packet..];
                    frag_offset += bytes_in_this_packet;
                }
                continue;
            }

            let n_frames = state.queued_frames.len();

            let queue_size = state.queued_frames.iter().map(|f| f.len()).sum::<usize>();

            let queue_duration = state
                .queued_frames
                .iter()
                .map(|f| f.duration())
                .sum::<u64>();

            // We optimistically add average size/duration to send out packets as early as possible
            // if we estimate that the next frame would likely overflow our accumulation limits.
            let avg_size = queue_size / n_frames;
            let avg_duration = queue_duration / n_frames as u64;

            let max_ptime = self.calc_effective_max_ptime(settings, state);

            let is_ready = send_mode == SendPacketMode::ForcePending
                || agg_mode != RtpMpegAudioPayAggregateMode::Aggregate
                || queue_size + avg_size > max_payload_size
                || (max_ptime.is_some() && queue_duration + avg_duration > max_ptime.unwrap());

            gst::log!(
                CAT,
                imp = self,
                "Queued: size {queue_size}, duration ~{}ms, mode: {:?} + {:?} => ready: {}",
                queue_duration / 1_000_000,
                agg_mode,
                send_mode,
                is_ready,
            );

            if !is_ready {
                gst::log!(CAT, imp = self, "Not ready yet, waiting for more data");
                break;
            }

            gst::trace!(CAT, imp = self, "Creating packet..");

            let pts_offset = gst::ClockTime::from_nseconds(first.pts_offset);

            let id = first.id;
            let mut end_id = first.id;

            let mut acc_duration = 0;
            let mut acc_size = 0;

            let mut packet = rtp_types::RtpPacketBuilder::new().marker_bit(state.discont_pending());

            let audio_specific_header = [0u8, 0, 0, 0];

            packet = packet.payload(audio_specific_header.as_slice());

            for frame in &state.queued_frames {
                gst::trace!(
                    CAT,
                    imp = self,
                    "{frame:?}, accumulated size {acc_size} duration ~{}ms",
                    acc_duration / 1_000_000,
                );

                // If this frame would overflow the packet, bail out and send out what we have.
                //
                // Don't take into account the max_ptime for the first frame, since it could be
                // lower than the frame duration in which case we would never payload anything.
                //
                // For the size check in bytes we know that the first frame will fit the mtu,
                // because we already checked for the "audio frame bigger than mtu" scenario above.
                if acc_size + frame.len() > max_payload_size
                    || (max_ptime.is_some()
                        && acc_duration > 0
                        && acc_duration + frame.duration() > max_ptime.unwrap())
                {
                    break;
                }

                // ... otherwise add frame to the packet
                packet = packet.payload(frame.data());
                end_id = frame.id;

                acc_size += frame.len();
                acc_duration += frame.duration();

                // .. otherwise check if there are more frames we can add to the packet
            }

            self.obj().queue_packet(
                PacketToBufferRelation::IdsWithOffset {
                    ids: (id..=end_id),
                    timestamp_offset: TimestampOffset::Pts(pts_offset),
                },
                packet,
            )?;

            // Now pop off all the frames we used
            while acc_size > 0 {
                let frame = state.queued_frames.pop_front().unwrap();
                acc_size -= frame.len();
            }
        }

        gst::log!(
            CAT,
            imp = self,
            "All done for now, {} frames queued",
            state.queued_frames.len()
        );

        if send_mode == SendPacketMode::ForcePending {
            self.obj().finish_pending_packets()?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn effective_aggregate_mode(&self, settings: &Settings) -> RtpMpegAudioPayAggregateMode {
        match settings.aggregate_mode {
            RtpMpegAudioPayAggregateMode::Auto => match self.is_live() {
                Some(true) => RtpMpegAudioPayAggregateMode::ZeroLatency,
                Some(false) => RtpMpegAudioPayAggregateMode::Aggregate,
                None => RtpMpegAudioPayAggregateMode::ZeroLatency,
            },
            mode => mode,
        }
    }

    fn is_live(&self) -> Option<bool> {
        *self.is_live.lock().unwrap()
    }

    // Query upstream live-ness if needed, in case of aggregate-mode=auto
    fn ensure_upstream_liveness(&self, settings: &mut Settings) {
        if settings.aggregate_mode != RtpMpegAudioPayAggregateMode::Auto || self.is_live().is_some()
        {
            return;
        }

        let mut q = gst::query::Latency::new();
        let is_live = if self.obj().sink_pad().peer_query(&mut q) {
            let (is_live, _, _) = q.result();
            is_live
        } else {
            false
        };

        *self.is_live.lock().unwrap() = Some(is_live);

        gst::info!(CAT, imp = self, "Upstream is live: {is_live}");
    }

    // We can get max ptime or ptime recommendations/restrictions from multiple places, e.g. the
    // "max-ptime" property, but also from "maxptime" or "ptime" values from downstream / an SDP.
    //
    // Here we look at the various values and decide on an effective max ptime value.
    //
    // We'll just return the lowest of any set values.
    //
    fn calc_effective_max_ptime(&self, settings: &Settings, state: &State) -> Option<u64> {
        [settings.max_ptime, state.max_ptime, state.ptime]
            .into_iter()
            .filter(|v| v.is_some())
            .min()
            .map(|t| t.unwrap().nseconds())
    }
}
