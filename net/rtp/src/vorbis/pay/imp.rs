// GStreamer RTP Vorbis Payloader
//
// Copyright (C) 2023-2026 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-rtpvorbispay2
 * @see_also: rtpvorbisdepay2, rtpvorbispay, rtpvorbisdepay, vorbisdec, vorbisenc
 *
 * Payloads an Vorbis audio stream into RTP packets as per [RFC 5215][rfc-5215].
 *
 * [rfc-5215]: https://www.rfc-editor.org/rfc/rfc5215.html
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
 * |[
 * gst-launch audiotestsrc wave=ticks ! vorbisenc ! rtpvorbispay2 ! udpsink host=127.0.0.1 port=5004
 * ]| This will encode an audio test signal to Vorbis and then payload the encoded audio
 * into RTP packets and send them out via UDP to localhost (IPv4) port 5004.
 * You can use the #rtpvorbisdepay2 or #rtpvorbisdepay elements to depayload such a stream, and
 * the #vorbisdec element to decode the depayloaded stream. Note that if you don't have external
 * signalling (RTSP, SDP, WebRTC, RTP caps with a `configuration` field) you may need to set the
 * `config-interval` property on the payloader/sender to enable in-band header transmission.
 *
 * Since: plugins-rs-0.16
 */
use atomic_refcell::AtomicRefCell;

use std::collections::VecDeque;

use std::sync::Mutex;

use gst::{glib, prelude::*, subclass::prelude::*};

use std::sync::LazyLock;

use crate::basepay::{PacketToBufferRelation, RtpBasePay2Ext, RtpBasePay2Impl, RtpBasePay2ImplExt};

use crate::vorbis::config::*;
use crate::vorbis::packet_header::*;

use super::RtpVorbisPayAggregateMode;

#[derive(Clone)]
struct Settings {
    max_ptime: Option<gst::ClockTime>,
    aggregate_mode: RtpVorbisPayAggregateMode,
    config_interval: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            aggregate_mode: RtpVorbisPayAggregateMode::Auto,
            config_interval: 0u32,
            max_ptime: None,
        }
    }
}

#[derive(Debug)]
struct QueuedFrame {
    // Id of the input buffer this frame came from
    id: u64,

    // Mapped buffer data
    buffer: gst::MappedBuffer<gst::buffer::Readable>,

    // Duration
    duration: Option<gst::ClockTime>,

    // Length as big-endian bytes, useful for packetisation later
    len_bytes: [u8; 2],
}

impl QueuedFrame {
    fn duration(&self) -> Option<u64> {
        self.duration.map(|t| t.nseconds())
    }

    fn len(&self) -> usize {
        self.buffer.len()
    }

    fn len_bytes(&self) -> &[u8; 2] {
        &self.len_bytes
    }

    fn data(&self) -> &[u8] {
        &self.buffer[0..]
    }
}

#[derive(Default)]
struct State {
    // Queued audio frames (we collect until max-ptime or the packet mtu is hit)
    queued_frames: VecDeque<QueuedFrame>,

    // Active vorbis config
    config: Option<VorbisConfig>,

    // The last time we sent an in-band config
    last_inband_config_time: Option<std::time::Instant>,
}

#[derive(Default)]
pub struct RtpVorbisPay {
    state: AtomicRefCell<State>,
    settings: Mutex<Settings>,
    is_live: Mutex<Option<bool>>,
}

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpvorbispay2",
        gst::DebugColorFlags::empty(),
        Some("RTP Vorbis Payloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpVorbisPay {
    const NAME: &'static str = "GstRtpVorbisPay2";
    type Type = super::RtpVorbisPay;
    type ParentType = crate::basepay::RtpBasePay2;
}

impl ObjectImpl for RtpVorbisPay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecEnum::builder_with_default("aggregate-mode", Settings::default().aggregate_mode)
                    .nick("Aggregate Mode")
                    .blurb("Whether to send out audio frames immediately or aggregate them until a packet is full.")
                    .build(),
                glib::ParamSpecUInt::builder("config-interval")
                    .nick("Config Interval")
                    .blurb("Regularly send Vorbis configuration headers in-band instead of relying on external signalling (0 = disabled)")
                    .default_value(Settings::default().config_interval)
                    .maximum(3600)
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
                settings.aggregate_mode = value.get::<RtpVorbisPayAggregateMode>().unwrap();
            }
            "config-interval" => {
                settings.config_interval = value.get::<u32>().unwrap();
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
            "config-interval" => settings.config_interval.to_value(),
            "max-ptime" => (settings
                .max_ptime
                .map(gst::ClockTime::nseconds)
                .map(|x| x as i64)
                .unwrap_or(-1))
            .to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for RtpVorbisPay {}

impl ElementImpl for RtpVorbisPay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP Vorbis Payloader",
                "Codec/Payloader/Network/RTP",
                "Payload a Vorbis audio stream into RTP packets (RFC 5215)",
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
                &gst::Caps::builder("audio/x-vorbis")
                    .field("channels", gst::IntRange::new(1i32, 255))
                    .field("rate", gst::IntRange::new(1i32, 200_000))
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
                            .field("encoding-name", "VORBIS")
                            .field("clock-rate", gst::IntRange::new(1i32, i32::MAX))
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

impl RtpBasePay2Impl for RtpVorbisPay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["audio"];
    const DROP_HEADER_BUFFERS: bool = true;

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let s = caps.structure(0).unwrap();

        // There isn't really any scenario in GStreamer where we have vorbis caps w/o streamheader
        let Ok(Some(streamheaders)) = s.get_optional::<gst::ArrayRef>("streamheader") else {
            gst::error!(CAT, imp = self, "Vorbis caps without streamheader field");
            return false;
        };

        let headers = match VorbisHeaders::from_streamheaders(streamheaders) {
            Ok(headers) => headers,
            Err(err) => {
                gst::error!(
                    CAT,
                    imp = self,
                    "Could not parse Vorbis streamheaders: {err:?}"
                );
                return false;
            }
        };

        let ident = headers.create_ident();

        let new_config = VorbisConfig::new(ident, headers);

        let config_string = match new_config.configuration_string() {
            Ok(config_string) => config_string,
            Err(err) => {
                gst::error!(
                    CAT,
                    imp = self,
                    "Could not create packed Vorbis configuration string: {err}"
                );
                return false;
            }
        };

        let channels = new_config.channels();
        let rate = new_config.rate();

        let mut state = self.state.borrow_mut();
        let settings = self.settings.lock().unwrap();

        if let Some(old_config) = state.config.replace(new_config) {
            let old_ident = old_config.ident();

            if ident == old_ident {
                gst::log!(
                    CAT,
                    imp = self,
                    "Vorbis configuration unchanged, ident {ident:04x?}"
                );
                return true;
            }

            gst::info!(
                CAT,
                imp = self,
                "Vorbis configuration changed from {old_ident:04x?} to {ident:04x?}"
            );

            if settings.config_interval == 0 {
                // Possible but unlikely that new caps will be handled right with out-of-band signalling
                gst::warning!(
                    CAT,
                    imp = self,
                    "Vorbis configuration changed, but in-band configuration sending disabled"
                );
            } else {
                // Queue the new updated headers in-band now
                if let Err(err) = self.send_inband_config(&mut state) {
                    gst::warning!(CAT, imp = self, "Failed to send in-band headers: {err:?}");
                    return false;
                }
            }
        } else {
            gst::info!(
                CAT,
                imp = self,
                "Vorbis configuration is {ident:04x?}, {channels}ch @ {rate}"
            );
        }

        // https://www.iana.org/assignments/media-types/audio/vorbis
        let src_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "VORBIS")
            .field("clock-rate", rate as i32)
            .field("configuration", config_string)
            // No mention of this in the RFC, but C payloader adds this
            .field("encoding-params", channels.to_string())
            .build();

        self.obj().set_src_caps(&src_caps);

        true
    }

    // https://www.rfc-editor.org/rfc/rfc5215.html#section-5
    // https://www.rfc-editor.org/errata/rfc5215
    //
    // We either put 1-N whole Vorbis audio frames in an RTP packet,
    // or a single Vorbis audio frame (or config) split over multiple RTP packets.
    //
    // The marker flag is unused in Vorbis according to the spec and should always
    // be zero, so we don't do anything with it.
    //
    fn handle_buffer(
        &self,
        buffer: &gst::Buffer,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();
        let mut settings = self.settings.lock().unwrap();

        let map = buffer.clone().into_mapped_buffer_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "Can't map buffer readable");
            gst::FlowError::Error
        })?;

        let buffer_size = map.size();

        if buffer_size > u16::MAX as usize {
            gst::error!(CAT, imp = self, "Vorbis packet is too large: {buffer:?}");
            Err(gst::FlowError::Error)?
        }

        // If in-band header sending is enabled, re-send the headers regularly
        if settings.config_interval > 0 {
            let send_now = if let Some(t) = state.last_inband_config_time {
                t.elapsed() >= std::time::Duration::from_secs(settings.config_interval as u64)
            } else {
                true
            };
            if send_now {
                gst::info!(
                    CAT,
                    imp = self,
                    "Config has changed, need to send new headers in-band now"
                );
                self.send_inband_config(&mut state)?;
            }
        }

        let queued_frame = QueuedFrame {
            id,
            buffer: map,
            duration: buffer.duration(),
            len_bytes: (buffer_size as u16).to_be_bytes(),
        };

        state.queued_frames.push_back(queued_frame);

        // Make sure we have queried upstream liveness if needed
        if settings.aggregate_mode == RtpVorbisPayAggregateMode::Auto {
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

                if self.effective_aggregate_mode(&settings) == RtpVorbisPayAggregateMode::Aggregate
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

        // Make sure configured MTU is large enough
        let max_payload_size = self.obj().max_payload_size() as usize;

        if max_payload_size <= VORBIS_SPECIFIC_HEADER_LEN + 16 * 2 {
            return Err(gst::error_msg!(
                gst::LibraryError::Settings,
                ("Configured MTU is too small")
            ));
        }

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        *self.is_live.lock().unwrap() = None;

        Ok(())
    }
}

// https://www.rfc-editor.org/rfc/rfc5215.html#section-2.2
const VORBIS_SPECIFIC_HEADER_LEN: usize = 4;

#[derive(Debug, PartialEq)]
enum SendPacketMode {
    WhenReady,
    ForcePending,
}

impl RtpVorbisPay {
    fn send_packets(
        &self,
        settings: &Settings,
        state: &mut State,
        send_mode: SendPacketMode,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // No active config can happen if we're draining before we got caps
        let Some(active_config) = state.config.as_ref() else {
            return Ok(gst::FlowSuccess::Ok);
        };
        let ident = active_config.ident();

        let agg_mode = self.effective_aggregate_mode(settings);

        let max_payload_size = self.obj().max_payload_size() as usize
            - VORBIS_SPECIFIC_HEADER_LEN
            - 15 * std::mem::size_of::<u16>();

        // Let's see what's ready to be sent out
        while let Some(first) = state.queued_frames.front() {
            // Big Vorbis frame that needs to be split across multiple packets?
            if first.len() > max_payload_size {
                let first = state.queued_frames.pop_front().unwrap();
                let frame = first;

                self.send_fragmented_payload(
                    frame.buffer.as_slice(),
                    ident,
                    VorbisDataType::RawPacket,
                    PacketToBufferRelation::Ids(frame.id..=frame.id),
                )?;

                continue;
            }

            let n_frames = state.queued_frames.len();

            let queue_size = state.queued_frames.iter().map(|f| f.len()).sum::<usize>();

            // If we don't have durations and max_ptime is set, don't aggregate
            let have_durations = state.queued_frames.iter().all(|f| f.duration().is_some());

            let queue_duration = if have_durations {
                state
                    .queued_frames
                    .iter()
                    .map(|f| f.duration().unwrap())
                    .sum::<u64>()
            } else {
                0
            };

            // We optimistically add average size/duration to send out packets as early as possible
            // if we estimate that the next frame would likely overflow our accumulation limits.
            let avg_size = queue_size / n_frames;
            let avg_duration = queue_duration / n_frames as u64;

            let max_ptime = settings.max_ptime.map(|t| t.nseconds());

            // Send out packets if there's enough data for one (or more), or if forced.
            let is_ready = send_mode == SendPacketMode::ForcePending
                || agg_mode != RtpVorbisPayAggregateMode::Aggregate
                || state.queued_frames.len() >= 15
                || queue_size + avg_size > max_payload_size
                || (max_ptime.is_some()
                    && (!have_durations || (queue_duration + avg_duration > max_ptime.unwrap())));

            gst::log!(
                CAT,
                imp = self,
                "Queued: n {}, size {queue_size}, duration ~{}ms, mode: {:?} + {:?} => ready: {}",
                state.queued_frames.len(),
                queue_duration / 1_000_000,
                agg_mode,
                send_mode,
                is_ready
            );

            if !is_ready {
                gst::log!(CAT, imp = self, "Not ready yet, waiting for more data");
                break;
            }

            gst::trace!(CAT, imp = self, "Creating packet..");

            let id = first.id;
            let mut end_id = first.id;

            let mut acc_duration = 0;
            let mut acc_size = 0;

            let mut n_frames = 0;

            // Figure out how many frames to put into the packet. Limit is 15.
            for (i, frame) in state.queued_frames.iter().enumerate().take(15) {
                gst::trace!(
                    CAT,
                    imp = self,
                    "{frame:?}, accumulated size {acc_size} duration ~{}ms",
                    acc_duration / 1_000_000
                );

                // If this frame would overflow the packet, bail out and send out what we have.
                //
                // Don't take into account the max_ptime for the first frame, since it could be
                // lower than the frame duration in which case we would never payload anything.
                //
                // For the size check in bytes we know that the first frame will fit the mtu,
                // because we already checked for the "audio frame bigger than mtu" scenario above.
                //
                if i > 0
                    && (acc_size + frame.len() > max_payload_size
                        || (max_ptime.is_some()
                            && have_durations
                            && acc_duration > 0
                            && acc_duration + frame.duration().unwrap() > max_ptime.unwrap()))
                {
                    break;
                }

                n_frames = i + 1;

                acc_size += frame.len();
                acc_duration += frame.duration().unwrap_or(0);

                // .. otherwise check if there are more frames we can add to the packet
            }

            gst::trace!(
                CAT,
                imp = self,
                "Packing {n_frames} Vorbis frames into packet"
            );

            let mut packet = rtp_types::RtpPacketBuilder::new();

            let vorbis_specific_header = PacketHeaderBuilder::new(ident)
                .frag_type(FragType::NotFragmented)
                .data_type(VorbisDataType::RawPacket)
                .n_packets(n_frames)
                .build()
                .unwrap();

            packet = packet.payload(vorbis_specific_header.as_slice());

            for frame in state.queued_frames.iter().take(n_frames) {
                packet = packet.payload(frame.len_bytes()).payload(frame.data());

                end_id = frame.id;
            }

            self.obj()
                .queue_packet(PacketToBufferRelation::Ids(id..=end_id), packet)?;

            // Now pop off all the frames we used
            for _ in 0..n_frames {
                let _ = state.queued_frames.pop_front().unwrap();
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

    // https://www.rfc-editor.org/rfc/rfc5215.html#section-5.1
    //
    fn send_fragmented_payload(
        &self,
        payload_data: &[u8],
        ident: u32,
        vdt: VorbisDataType,
        packet_to_buffer_relation: PacketToBufferRelation,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let max_payload_size = self.obj().max_payload_size() as usize
            - VORBIS_SPECIFIC_HEADER_LEN
            - std::mem::size_of::<u16>();

        let payload_len = payload_data.len();
        let mut payload_data = &payload_data[0..];
        let mut is_first = true;

        while !payload_data.is_empty() {
            let bytes_left = payload_data.len();
            let bytes_in_this_packet = std::cmp::min(bytes_left, max_payload_size);

            let (frag_type, n_packets) = if payload_len <= max_payload_size {
                (FragType::NotFragmented, 1)
            } else if is_first {
                (FragType::Start, 0)
            } else if bytes_left <= max_payload_size {
                (FragType::End, 0)
            } else {
                (FragType::Continuation, 0)
            };

            let vorbis_specific_header = PacketHeaderBuilder::new(ident)
                .data_type(vdt)
                .frag_type(frag_type)
                .n_packets(n_packets)
                .build()
                .unwrap();

            let length_bytes = (bytes_in_this_packet as u16).to_be_bytes();

            self.obj().queue_packet(
                packet_to_buffer_relation.clone(),
                rtp_types::RtpPacketBuilder::new()
                    .payload(vorbis_specific_header.as_slice())
                    .payload(length_bytes.as_slice())
                    .payload(&payload_data[0..bytes_in_this_packet]),
            )?;

            payload_data = &payload_data[bytes_in_this_packet..];
            is_first = false;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn send_inband_config(&self, state: &mut State) -> Result<gst::FlowSuccess, gst::FlowError> {
        let config = state.config.as_ref().unwrap();

        let packed_headers = match config.to_packed() {
            Ok(headers) => headers,
            Err(err) => {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Failed to create packed Vorbis headers: {err:?}"
                );
                return Err(gst::FlowError::Error);
            }
        };

        state.last_inband_config_time = Some(std::time::Instant::now());

        self.send_fragmented_payload(
            &packed_headers,
            config.ident(),
            VorbisDataType::PackedConfig,
            PacketToBufferRelation::OutOfBand,
        )
    }

    fn effective_aggregate_mode(&self, settings: &Settings) -> RtpVorbisPayAggregateMode {
        match settings.aggregate_mode {
            RtpVorbisPayAggregateMode::Auto => match self.is_live() {
                Some(true) => RtpVorbisPayAggregateMode::ZeroLatency,
                Some(false) => RtpVorbisPayAggregateMode::Aggregate,
                None => RtpVorbisPayAggregateMode::ZeroLatency,
            },
            mode => mode,
        }
    }

    fn is_live(&self) -> Option<bool> {
        *self.is_live.lock().unwrap()
    }

    // Query upstream live-ness if needed, in case of aggregate-mode=auto
    fn ensure_upstream_liveness(&self, settings: &mut Settings) {
        if settings.aggregate_mode != RtpVorbisPayAggregateMode::Auto || self.is_live().is_some() {
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
}
