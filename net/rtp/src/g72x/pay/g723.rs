//
// Copyright (C) 2026 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use atomic_refcell::AtomicRefCell;
use gst::{glib, prelude::*, subclass::prelude::*};

use std::{
    collections::VecDeque,
    sync::{LazyLock, Mutex},
};

use crate::{
    audio_discont::{AudioDiscont, AudioDiscontConfiguration},
    basepay::{PacketToBufferRelation, RtpBasePay2Ext, RtpBasePay2ImplExt, TimestampOffset},
    g72x::utils::{G723Frame, g723_bytes_to_frames},
};

/**
 * SECTION:element-rtpg7232
 * @see_also: rtpg723depay2
 *
 * Payloads G.723 encoded audio into RTP packets as per [RFC 3551][rfc-3551].
 *
 * [rfc-3551]: https://www.rfc-editor.org/rfc/rfc3551.html#section-4.5.3
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 audiotestsrc wave=ticks ! audio/x-raw,rate=8000,channels=1 ! avenc_g723_1 ! rtpg723pay2 ! udpsink2 host=127.0.0.1 port=5004
 * ]| This will generate an G.723 audio test signal and payload it as RTP and send it out
 * as UDP to localhost port 5004.
 *
 * Since: plugins-rs-0.13.0
 */

#[derive(Clone)]
struct Settings {
    max_ptime: Option<gst::ClockTime>,
    min_ptime: gst::ClockTime,
    ptime_multiple: gst::ClockTime,
    audio_discont: AudioDiscontConfiguration,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            max_ptime: None,
            min_ptime: FRAME_SIZE,
            ptime_multiple: gst::ClockTime::ZERO,
            audio_discont: AudioDiscontConfiguration::default(),
        }
    }
}

#[derive(Debug)]
struct QueuedBuffer {
    /// ID of the buffer.
    id: u64,
    /// The mapped buffer itself.
    buffer: gst::MappedBuffer<gst::buffer::Readable>,
    /// Offset into the buffer that was not consumed yet.
    frame_offset: usize,
    /// List of frames in this buffer.
    frames: Vec<(usize, G723Frame)>,
}

#[derive(Default)]
struct State {
    /// Currently configured clock rate.
    clock_rate: Option<u32>,

    /// Desired "packet time", i.e. packet duration, from the caps, if set.
    ptime: Option<gst::ClockTime>,
    max_ptime: Option<gst::ClockTime>,

    /// Currently queued buffers.
    queued_buffers: VecDeque<QueuedBuffer>,

    audio_discont: AudioDiscont,
}

const FRAME_SIZE: gst::ClockTime = gst::ClockTime::from_mseconds(30);

#[derive(Default)]
pub struct RtpG723Pay {
    settings: Mutex<Settings>,
    state: AtomicRefCell<State>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpg723pay2",
        gst::DebugColorFlags::empty(),
        Some("RTP G.723 Audio Payloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpG723Pay {
    const NAME: &'static str = "GstRtpG723Pay2";
    type Type = super::RtpG723Pay;
    type ParentType = crate::basepay::RtpBasePay2;
}

impl ObjectImpl for RtpG723Pay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let mut properties = vec![
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
                // Using same type/semantics as C payloaders
                glib::ParamSpecInt64::builder("min-ptime")
                    .nick("Minimum Packet Time")
                    .blurb("Minimum duration of the packet data in ns (can't go above MTU)")
                    .default_value(Settings::default().min_ptime.nseconds() as i64)
                    .minimum(FRAME_SIZE.nseconds() as i64)
                    .maximum(i64::MAX)
                    .mutable_playing()
                    .build(),
                // Using same type/semantics as C payloaders
                glib::ParamSpecInt64::builder("ptime-multiple")
                    .nick("Packet Time Multiple")
                    .blurb("Force buffers to be multiples of this duration in ns (0 disables). Must be a multiple of 30ms.")
                    .default_value(Settings::default().ptime_multiple.nseconds() as i64)
                    .minimum(0)
                    .maximum(i64::MAX)
                    .mutable_playing()
                    .build(),
            ];

            properties.extend_from_slice(&AudioDiscontConfiguration::create_pspecs());

            properties
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        if self
            .settings
            .lock()
            .unwrap()
            .audio_discont
            .set_property(value, pspec)
        {
            return;
        }

        match pspec.name() {
            "max-ptime" => {
                let v = value.get::<i64>().unwrap();
                self.settings.lock().unwrap().max_ptime =
                    (v != -1).then_some(gst::ClockTime::from_nseconds(v as u64).max(FRAME_SIZE));
            }
            "min-ptime" => {
                let v = gst::ClockTime::from_nseconds(value.get::<i64>().unwrap() as u64);

                let mut settings = self.settings.lock().unwrap();
                let changed = settings.min_ptime != v;
                settings.min_ptime = v;
                drop(settings);

                if changed {
                    let _ = self
                        .obj()
                        .post_message(gst::message::Latency::builder().src(&*self.obj()).build());
                }
            }
            "ptime-multiple" => {
                let val = gst::ClockTime::from_nseconds(value.get::<i64>().unwrap() as u64);
                self.settings.lock().unwrap().ptime_multiple = val - val % FRAME_SIZE;
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        if let Some(value) = self.settings.lock().unwrap().audio_discont.property(pspec) {
            return value;
        }

        match pspec.name() {
            "max-ptime" => (self
                .settings
                .lock()
                .unwrap()
                .max_ptime
                .map(gst::ClockTime::nseconds)
                .map(|x| x as i64)
                .unwrap_or(-1))
            .to_value(),
            "min-ptime" => (self.settings.lock().unwrap().min_ptime.nseconds() as i64).to_value(),
            "ptime-multiple" => {
                (self.settings.lock().unwrap().ptime_multiple.nseconds() as i64).to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for RtpG723Pay {}

impl ElementImpl for RtpG723Pay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP G.723 Payloader",
                "Codec/Payloader/Network/RTP",
                "Payload G.723 Audio into RTP packets (RFC 3551)",
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
                &gst::Caps::builder("audio/G723")
                    .field("channels", 1i32)
                    .field("rate", 8000i32)
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
                            .field("payload", 4i32)
                            .field("clock-rate", 8000i32)
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "audio")
                            .field("encoding-name", "G723")
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

impl crate::basepay::RtpBasePay2Impl for RtpG723Pay {
    const ALLOWED_META_TAGS: &'static [&'static str] = &["audio"];

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn negotiate(&self, mut src_caps: gst::Caps) {
        // Fixate here as a first step
        src_caps.fixate();

        let s = src_caps.structure(0).unwrap();

        // Negotiate ptime/maxptime with downstream and use them in combination with the
        // properties. See https://datatracker.ietf.org/doc/html/rfc4566#section-6
        let ptime = s
            .get::<u32>("ptime")
            .ok()
            .map(u64::from)
            .map(gst::ClockTime::from_mseconds)
            .map(|v| v - v % FRAME_SIZE);

        let max_ptime = s
            .get::<u32>("maxptime")
            .ok()
            .map(u64::from)
            .map(gst::ClockTime::from_mseconds)
            .map(|v| v - v % FRAME_SIZE);

        let clock_rate = match s.get::<i32>("clock-rate") {
            Ok(clock_rate) if clock_rate > 0 => clock_rate as u32,
            _ => {
                panic!("RTP caps {src_caps:?} without 'clock-rate'");
            }
        };

        self.parent_negotiate(src_caps);

        // Draining happened above if the clock rate has changed
        let mut state = self.state.borrow_mut();
        state.ptime = ptime;
        state.max_ptime = max_ptime;
        state.clock_rate = Some(clock_rate);
        drop(state);
    }

    fn drain(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.state.borrow_mut();
        self.drain_packets(&settings, &mut state, true)
    }

    fn flush(&self) {
        let mut state = self.state.borrow_mut();
        state.queued_buffers.clear();
        state.audio_discont.reset();
    }

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let s = caps.structure(0).unwrap();

        let channels = s.get::<i32>("channels").unwrap();

        let src_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "G723")
            .field("encoding-params", channels.to_string())
            .field("clock-rate", 8000i32)
            .build();

        self.obj().set_src_caps(&src_caps);

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

    fn handle_buffer(
        &self,
        buffer: &gst::Buffer,
        id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let buffer = buffer.clone().into_mapped_buffer_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "Can't map buffer readable");
            gst::FlowError::Error
        })?;
        let Some(pts) = buffer.buffer().pts() else {
            gst::error!(CAT, imp = self, "Input buffer must have pts()");
            return Err(gst::FlowError::Error);
        };

        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.state.borrow_mut();

        for (offset, frame) in g723_bytes_to_frames(buffer.as_slice()) {
            if frame.is_none() {
                gst::error!(CAT, imp = self, "Unknown G.723 frame type at idx {offset}");
                return Err(gst::FlowError::Error);
            }
        }
        let frames = g723_bytes_to_frames(buffer.as_slice())
            .map(|(offset, frame)| (offset, frame.unwrap()))
            .collect::<Vec<_>>();
        let num_samples = frames.len() * 240;

        let discont = state.audio_discont.process_input(
            &settings.audio_discont,
            buffer.buffer().flags().contains(gst::BufferFlags::DISCONT)
                || buffer.buffer().flags().contains(gst::BufferFlags::RESYNC),
            8000,
            pts,
            num_samples,
        );

        if discont {
            if state.audio_discont.base_pts().is_some() {
                gst::debug!(CAT, imp = self, "Draining because of discontinuity");
                self.drain_packets(&settings, &mut state, true)?;
            }

            state.audio_discont.resync(pts, num_samples);
        }

        state.queued_buffers.push_back(QueuedBuffer {
            id,
            buffer,
            frame_offset: 0,
            frames,
        });

        self.drain_packets(&settings, &mut state, false)
    }
}

impl RtpG723Pay {
    fn drain_packets(
        &self,
        settings: &Settings,
        state: &mut State,
        force: bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Always set when caps are set
        let Some(_clock_rate) = state.clock_rate else {
            return Ok(gst::FlowSuccess::Ok);
        };

        let min_pframes = if force {
            1
        } else {
            settings.min_ptime / FRAME_SIZE
        };
        let max_ptime = match (settings.max_ptime, state.max_ptime) {
            (Some(max_ptime), Some(caps_max_ptime)) => {
                Some(core::cmp::min(max_ptime, caps_max_ptime))
            }
            (None, Some(max_ptime)) => Some(max_ptime),
            (Some(max_ptime), None) => Some(max_ptime),
            _ => None,
        };
        let pframes_multiple = core::cmp::max(1, settings.ptime_multiple / FRAME_SIZE) as usize;

        let pframes = state.ptime.or(max_ptime).map(|ptime| ptime / FRAME_SIZE);

        let max_payload_size = self.obj().max_payload_size() as usize;

        if max_payload_size < 24 {
            gst::error!(
                CAT,
                "Max payload size must be greater than one frame of 24 bytes"
            );
            return Err(gst::FlowError::Error);
        }

        gst::trace!(
            CAT,
            imp = self,
            "min ptime {} (frames: {}), max ptime {} (frames: {}), ptime multiple {} (frames {})",
            settings.min_ptime,
            min_pframes,
            settings.max_ptime.display(),
            pframes.unwrap_or(0),
            settings.ptime_multiple,
            pframes_multiple,
        );

        while state.queued_buffers.front().is_some() {
            if !force
                && state.queued_buffers.iter().fold(0, |n_frames, queued| {
                    n_frames + (queued.frames.len() - queued.frame_offset) as u64
                }) < min_pframes
            {
                break;
            }

            let front = state.queued_buffers.front().unwrap();
            let start_id = front.id;
            let mut end_id = front.id;
            let mut packet_builder = rtp_types::RtpPacketBuilder::new()
                .marker_bit(state.audio_discont.next_output_offset().is_none());

            let mut remaining_packet_size = max_payload_size;
            let mut collected_frames = 0;
            let mut pending_previous_frames = 0;
            for buffer in state.queued_buffers.iter() {
                gst::trace!(
                    CAT,
                    imp = self,
                    "collecting (collected frames: {collected_frames}), (remaining packet size:{remaining_packet_size}), pending previous frames: {pending_previous_frames} may use buffer: {buffer:?}"
                );
                if pending_previous_frames > 0 {
                    // TODO: ptimes/multiple that cause straddling of two (or more) input buffers.
                    gst::warning!(CAT, imp = self, "Unhandled!");
                    return Err(gst::FlowError::Error);
                }

                let initial_frame_offset = buffer.frame_offset;
                let mut end_frame_offset = initial_frame_offset;
                for (idx, frames) in buffer.frames[buffer.frame_offset..]
                    .chunks(pframes_multiple)
                    .enumerate()
                {
                    let (latest_frame_byte_offset, frame) = frames[frames.len() - 1];
                    // number of bytes in this chunk.
                    let payload_size = latest_frame_byte_offset + frame.octets() - frames[0].0;
                    // number of remaining bytes that can be taken from this buffer.
                    let payload_size_from_start = latest_frame_byte_offset + frame.octets()
                        - buffer.frames[initial_frame_offset].0;
                    gst::trace!(
                        CAT,
                        imp = self,
                        "frame group (len: {}) start at byte offset {} of size {payload_size} size from start {payload_size_from_start}",
                        frames.len(),
                        frames[0].0
                    );
                    if payload_size_from_start > remaining_packet_size {
                        if collected_frames == 0 {
                            gst::error!(
                                CAT,
                                imp = self,
                                "Failed to collect any G.723 frames. Likely too low packet/frame size settings."
                            );
                            return Err(gst::FlowError::Error);
                        }
                        break;
                    }
                    pending_previous_frames = pframes_multiple - frames.len();
                    if pending_previous_frames != 0 {
                        break;
                    }
                    collected_frames += pframes_multiple;
                    end_frame_offset = buffer.frame_offset + idx;
                    if collected_frames as u64 >= pframes.unwrap_or(u64::MAX) {
                        break;
                    }
                }

                end_id = buffer.id;
                let offset = buffer.frames[initial_frame_offset].0;
                let (latest_frame_byte_offset, frame) = buffer.frames[end_frame_offset];
                let this_buffer_payload_size = latest_frame_byte_offset + frame.octets() - offset;
                if this_buffer_payload_size == 0 {
                    break;
                }
                packet_builder =
                    packet_builder.payload(&buffer.buffer[offset..][..this_buffer_payload_size]);
                remaining_packet_size -= this_buffer_payload_size;
                if collected_frames as u64 >= pframes.unwrap_or(u64::MAX) {
                    break;
                }
            }

            // Then create the packet.
            self.obj().queue_packet(
                PacketToBufferRelation::IdsWithOffset {
                    ids: start_id..=end_id,
                    timestamp_offset: {
                        if let Some(next_out_offset) = state.audio_discont.next_output_offset() {
                            TimestampOffset::Rtp(next_out_offset)
                        } else {
                            TimestampOffset::Pts(gst::ClockTime::ZERO)
                        }
                    },
                },
                packet_builder,
            )?;

            let num_samples = collected_frames * 240;
            while collected_frames > 0 {
                let buffer = state.queued_buffers.front_mut().unwrap();

                let frames_consumed =
                    core::cmp::min(buffer.frames.len() - buffer.frame_offset, collected_frames);
                buffer.frame_offset += frames_consumed;
                collected_frames -= frames_consumed;

                if buffer.frame_offset == buffer.frames.len() {
                    let _ = state.queued_buffers.pop_front();
                }
            }

            state.audio_discont.process_output(num_samples);
        }

        let (n_bytes, n_frames) =
            state
                .queued_buffers
                .iter()
                .fold((0, 0), |(n_bytes, n_frames), buffer| {
                    (
                        n_bytes + buffer.buffer.size(),
                        n_frames + buffer.frames.len(),
                    )
                });

        gst::trace!(
            CAT,
            imp = self,
            "Currently {n_bytes} bytes / {n_frames} frames queued",
        );

        Ok(gst::FlowSuccess::Ok)
    }
}
