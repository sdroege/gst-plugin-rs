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
    cmp,
    collections::VecDeque,
    sync::{LazyLock, Mutex},
};

use crate::{
    audio_discont::{AudioDiscont, AudioDiscontConfiguration},
    basepay::{PacketToBufferRelation, RtpBasePay2Ext, RtpBasePay2ImplExt, TimestampOffset},
};

/**
 * SECTION:element-rtpg7292
 * @see_also: rtpg729depay2
 *
 * Payloads G.729 encoded audio into RTP packets as per [RFC 3551][rfc-3551].
 *
 * [rfc-3551]: https://www.rfc-editor.org/rfc/rfc3551.html#section-4.5.6
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 audiotestsrc wave=ticks ! audio/x-raw,rate=8000,channels=1 ! avenc_g729 ! rtpg729pay2 ! udpsink host=127.0.0.1 port=5004
 * ]| This will generate an G.729 audio test signal and payload it as RTP and send it out
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
    offset: usize,
}

#[derive(Debug, Default)]
struct State {
    /// Desired "packet time", i.e. packet duration, from the caps, if set.
    ptime: Option<gst::ClockTime>,
    max_ptime: Option<gst::ClockTime>,

    /// Currently queued buffers.
    queued_buffers: VecDeque<QueuedBuffer>,
    queued_bytes: usize,

    audio_discont: AudioDiscont,
}

#[derive(Default)]
pub struct RtpG729Pay {
    state: AtomicRefCell<State>,
    settings: Mutex<Settings>,
}

const FRAME_SIZE: gst::ClockTime = gst::ClockTime::from_mseconds(10);
const CLOCK_RATE: i32 = 8000;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtpg729pay2",
        gst::DebugColorFlags::empty(),
        Some("RTP G.729 Audio Payloader"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for RtpG729Pay {
    const NAME: &'static str = "GstRtpG729Pay2";
    type Type = super::RtpG729Pay;
    type ParentType = crate::basepay::RtpBasePay2;
}

impl ObjectImpl for RtpG729Pay {
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
                    (v != -1).then_some(gst::ClockTime::from_nseconds(v as u64).min(FRAME_SIZE));
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

impl GstObjectImpl for RtpG729Pay {}

impl ElementImpl for RtpG729Pay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTP G.729 Payloader",
                "Codec/Payloader/Network/RTP",
                "Payload G.729 Audio into RTP packets (RFC 3551)",
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
                &gst::Caps::builder("audio/G729")
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
                            .field("payload", 18i32)
                            .field("clock-rate", CLOCK_RATE)
                            .field("encoding-name", "G729")
                            .build(),
                    )
                    .structure(
                        gst::Structure::builder("application/x-rtp")
                            .field("media", "audio")
                            .field("encoding-name", "G729")
                            .field("encoding-params", "1")
                            .field("clock-rate", CLOCK_RATE)
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

impl crate::basepay::RtpBasePay2Impl for RtpG729Pay {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = State::default();
        Ok(())
    }

    fn set_sink_caps(&self, caps: &gst::Caps) -> bool {
        let s = caps.structure(0).unwrap();

        let channels = s.get::<i32>("channels").unwrap();

        let src_caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", "G729")
            .field("encoding-params", channels.to_string())
            .field("channels", channels)
            .field("clock-rate", CLOCK_RATE)
            .build();

        self.obj().set_src_caps(&src_caps);

        true
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

        s.get::<i32>("clock-rate")
            .expect("RTP caps {src_caps:?} without 'clock-rate'");

        self.parent_negotiate(src_caps);

        // Draining happened above if the clock rate has changed
        let mut state = self.state.borrow_mut();
        state.ptime = ptime;
        state.max_ptime = max_ptime;
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
        state.queued_bytes = 0;
        state.audio_discont.reset();
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
            gst::error!(CAT, imp = self, "Input buffer needs timestamps.");
            return Err(gst::FlowError::Error);
        };

        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.state.borrow_mut();

        let frames = buffer.len() / 10;
        let frame_rem = buffer.len() % 10;
        let has_cn = frame_rem == 2;
        if ![0, 2].contains(&frame_rem) {
            gst::element_error!(
                self.obj(),
                gst::StreamError::WrongType,
                ("Invalid input buffer size"),
                [
                    "Invalid buffer size, should be multiple of 10 with an optional comfort frame size of 2 but it is {}",
                    buffer.len()
                ]
            );
            return Err(gst::FlowError::Error);
        }
        let frames_and_comfort = if has_cn { frames + 1 } else { frames };

        let discont = state.audio_discont.process_input(
            &settings.audio_discont,
            buffer.buffer().flags().contains(gst::BufferFlags::DISCONT)
                || buffer.buffer().flags().contains(gst::BufferFlags::RESYNC),
            8000,
            pts,
            frames_and_comfort * 80,
        );

        if discont {
            if state.audio_discont.base_pts().is_some() {
                gst::debug!(CAT, imp = self, "Draining because of discontinuity");
                self.drain_packets(&settings, &mut state, true)?;
            }

            state.audio_discont.resync(pts, frames_and_comfort);
        }

        state.queued_bytes += buffer.buffer().size();
        state.queued_buffers.push_back(QueuedBuffer {
            id,
            buffer,
            offset: 0,
        });

        self.drain_packets(&settings, &mut state, false)
    }
}

impl RtpG729Pay {
    /// Returns the minimum, maximum and chunk/multiple packet sizes
    fn calculate_packet_sizes(&self, settings: &Settings, state: &State) -> (usize, usize, usize) {
        let min_pframes = settings
            .min_ptime
            .nseconds()
            .mul_div_ceil(1, 10 * gst::ClockTime::MSECOND.nseconds())
            .unwrap() as u32;
        let max_ptime = match (settings.max_ptime, state.max_ptime) {
            (Some(max_ptime), Some(caps_max_ptime)) => Some(cmp::min(max_ptime, caps_max_ptime)),
            (None, Some(max_ptime)) => Some(max_ptime),
            (Some(max_ptime), None) => Some(max_ptime),
            _ => None,
        };
        let max_pframes = max_ptime.map(|max_ptime| {
            max_ptime
                .nseconds()
                .mul_div_ceil(1, 10 * gst::ClockTime::MSECOND.nseconds())
                .unwrap() as u32
        });
        let pframes_multiple = cmp::max(
            1,
            settings
                .ptime_multiple
                .nseconds()
                .mul_div_ceil(1, 10 * gst::ClockTime::MSECOND.nseconds())
                .unwrap() as u32,
        );

        gst::trace!(
            CAT,
            imp = self,
            "min ptime {} (frames: {}), max ptime {} (frames: {}), ptime multiple {} (frames {})",
            settings.min_ptime,
            min_pframes,
            settings.max_ptime.display(),
            max_pframes.unwrap_or(0),
            settings.ptime_multiple,
            pframes_multiple,
        );

        let psize_multiple = (pframes_multiple * 10) as usize;

        let mut max_packet_size = self.obj().max_payload_size() as usize;
        max_packet_size -= max_packet_size % psize_multiple;
        if let Some(max_pframes) = max_pframes {
            max_packet_size = cmp::min((max_pframes * 10) as usize, max_packet_size)
        }
        let mut min_packet_size = cmp::min(
            cmp::max((min_pframes * 10) as usize, psize_multiple),
            max_packet_size,
        );

        if let Some(ptime) = state.ptime {
            let pframes = ptime
                .nseconds()
                .mul_div_ceil(1, 10 * gst::ClockTime::MSECOND.nseconds())
                .unwrap() as u32;

            let psize = (pframes * 10) as usize;
            min_packet_size = cmp::max(min_packet_size, psize);
            max_packet_size = cmp::min(max_packet_size, psize);
        }

        (min_packet_size, max_packet_size, psize_multiple)
    }

    fn drain_packets(
        &self,
        settings: &Settings,
        state: &mut State,
        force: bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (min_packet_size, max_packet_size, psize_multiple) =
            self.calculate_packet_sizes(settings, state);
        if max_packet_size < psize_multiple {
            gst::error!(
                CAT,
                imp = self,
                "Impossible configuration, maximum packet size of {max_packet_size} is less than the packet size multiple {psize_multiple}."
            );
            return Err(gst::FlowError::Error);
        }

        gst::trace!(
            CAT,
            imp = self,
            "Currently {} bytes queued, min packet size {min_packet_size}, max packet size {max_packet_size}, force {force}",
            state.queued_bytes,
        );

        while state.queued_bytes >= min_packet_size
            || state.queued_bytes % 10 == 2
            || (force && state.queued_bytes > 0)
        {
            // Determine the total payload size (voice + optional trailing 2-byte CN).
            // Matches the C payloader: when everything remaining fits in the maximum
            // packet size, send it all (including a trailing CN frame); otherwise send
            // as many full 10-byte voice frames as fit, leaving the CN for later.
            let packet_size = if state.queued_bytes <= max_packet_size {
                state.queued_bytes
            } else {
                let mut packet_size = cmp::min(max_packet_size, (state.queued_bytes / 10) * 10);
                packet_size -= packet_size % psize_multiple;
                packet_size
            };
            assert_ne!(packet_size, 0);

            // A payload whose size is 2 mod 10 carries a trailing CN frame.
            let has_cn = packet_size % 10 == 2;
            // Each 10-byte voice frame and the 2-byte CN frame each occupy one 10 ms
            // slot, so each of them advances the RTP timestamp by 80 samples.
            let num_frames =
                (packet_size - if has_cn { 2 } else { 0 }) / 10 + if has_cn { 1 } else { 0 };
            gst::trace!(
                CAT,
                imp = self,
                "Creating packet of size {packet_size} ({num_frames} frames), marker {} comfort-noise {has_cn}",
                state.audio_discont.next_output_offset().is_none(),
            );

            // Set marker bit on the first packet after a discontinuity
            let mut packet_builder = rtp_types::RtpPacketBuilder::new()
                .marker_bit(state.audio_discont.next_output_offset().is_none());

            // queued_bytes > 0 implies the queue is not empty.
            let front = state.queued_buffers.front().unwrap();
            let start_id = front.id;
            let mut end_id = front.id;

            // Fill the payload with exactly `packet_size` bytes from the front of the queue.
            let mut remaining = packet_size;
            for buffer in state.queued_buffers.iter() {
                let buf_remaining = buffer.buffer.size() - buffer.offset;
                let take = cmp::min(buf_remaining, remaining);

                end_id = buffer.id;
                packet_builder = packet_builder.payload(&buffer.buffer[buffer.offset..][..take]);

                remaining -= take;
                if remaining == 0 {
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

            // And finally dequeue exactly the bytes we emitted.
            let mut remaining = packet_size;
            while remaining > 0 {
                let buffer = state.queued_buffers.front_mut().unwrap();
                let buf_remaining = buffer.buffer.size() - buffer.offset;

                if buf_remaining > remaining {
                    buffer.offset += remaining;
                    remaining = 0;
                } else {
                    remaining -= buf_remaining;
                    let _ = state.queued_buffers.pop_front();
                }
            }
            state.queued_bytes -= packet_size;
            state.audio_discont.process_output(num_frames * 80);
        }

        gst::trace!(
            CAT,
            imp = self,
            "Currently {} bytes / {} frames queued",
            state.queued_bytes,
            state.queued_bytes / 10
        );

        Ok(gst::FlowSuccess::Ok)
    }
}
