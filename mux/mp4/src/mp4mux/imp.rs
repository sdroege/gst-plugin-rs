// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use std::sync::Mutex;

use once_cell::sync::Lazy;

use super::boxes;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "mp4mux",
        gst::DebugColorFlags::empty(),
        Some("MP4Mux Element"),
    )
});

const DEFAULT_INTERLEAVE_BYTES: Option<u64> = None;
const DEFAULT_INTERLEAVE_TIME: Option<gst::ClockTime> = Some(gst::ClockTime::from_mseconds(500));

#[derive(Debug, Clone)]
struct Settings {
    interleave_bytes: Option<u64>,
    interleave_time: Option<gst::ClockTime>,
    movie_timescale: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            interleave_bytes: DEFAULT_INTERLEAVE_BYTES,
            interleave_time: DEFAULT_INTERLEAVE_TIME,
            movie_timescale: 0,
        }
    }
}

struct PendingBuffer {
    buffer: gst::Buffer,
    timestamp: gst::Signed<gst::ClockTime>,
    pts: gst::ClockTime,
    composition_time_offset: Option<i64>,
    duration: Option<gst::ClockTime>,
}

struct Stream {
    /// Sink pad for this stream.
    sinkpad: super::MP4MuxPad,

    /// Currently configured caps for this stream.
    caps: gst::Caps,
    /// Whether this stream is intra-only and has frame reordering.
    delta_frames: super::DeltaFrames,

    /// Already written out chunks with their samples for this stream
    chunks: Vec<super::Chunk>,

    /// Queued time in the latest chunk.
    queued_chunk_time: gst::ClockTime,
    /// Queue bytes in the latest chunk.
    queued_chunk_bytes: u64,

    /// Currently pending buffer, DTS or PTS running time and duration
    ///
    /// If the duration is set then the next buffer is already queued up and the duration was
    /// calculated based on that.
    pending_buffer: Option<PendingBuffer>,

    /// Start DTS.
    start_dts: Option<gst::Signed<gst::ClockTime>>,

    /// Earliest PTS.
    earliest_pts: Option<gst::ClockTime>,
    /// Current end PTS.
    end_pts: Option<gst::ClockTime>,
}

#[derive(Default)]
struct State {
    /// List of streams when the muxer was started.
    streams: Vec<Stream>,

    /// Index of stream that is currently selected to fill a chunk.
    current_stream_idx: Option<usize>,

    /// Current writing offset since the beginning of the stream.
    current_offset: u64,

    /// Offset of the `mdat` box from the beginning of the stream.
    mdat_offset: Option<u64>,

    /// Size of the `mdat` as written so far.
    mdat_size: u64,
}

#[derive(Default)]
pub(crate) struct MP4Mux {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl MP4Mux {
    /// Queue a buffer and calculate its duration.
    ///
    /// Returns `Ok(())` if a buffer with duration is known or if the stream is EOS and a buffer is
    /// queued, i.e. if this stream is ready to be processed.
    ///
    /// Returns `Err(Eos)` if nothing is queued and the stream is EOS.
    ///
    /// Returns `Err(AGGREGATOR_FLOW_NEED_DATA)` if more data is needed.
    ///
    /// Returns `Err(Error)` on errors.
    fn queue_buffer(&self, stream: &mut Stream) -> Result<(), gst::FlowError> {
        // Loop up to two times here to first retrieve the current buffer and then potentially
        // already calculate its duration based on the next queued buffer.
        loop {
            match stream.pending_buffer {
                Some(PendingBuffer {
                    duration: Some(_), ..
                }) => return Ok(()),
                Some(PendingBuffer {
                    timestamp,
                    pts,
                    ref buffer,
                    ref mut duration,
                    ..
                }) => {
                    // Already have a pending buffer but no duration, so try to get that now
                    let buffer = match stream.sinkpad.peek_buffer() {
                        Some(buffer) => buffer,
                        None => {
                            if stream.sinkpad.is_eos() {
                                let dur = buffer.duration().unwrap_or(gst::ClockTime::ZERO);
                                gst::trace!(
                                    CAT,
                                    obj: stream.sinkpad,
                                    "Stream is EOS, using {dur} as duration for queued buffer",
                                );

                                let pts = pts + dur;
                                if stream.end_pts.map_or(true, |end_pts| end_pts < pts) {
                                    gst::trace!(CAT, obj: stream.sinkpad, "Stream end PTS {pts}");
                                    stream.end_pts = Some(pts);
                                }

                                *duration = Some(dur);

                                return Ok(());
                            } else {
                                gst::trace!(CAT, obj: stream.sinkpad, "Stream has no buffer queued");
                                return Err(gst_base::AGGREGATOR_FLOW_NEED_DATA);
                            }
                        }
                    };

                    if stream.delta_frames.requires_dts() && buffer.dts().is_none() {
                        gst::error!(CAT, obj: stream.sinkpad, "Require DTS for video streams");
                        return Err(gst::FlowError::Error);
                    }

                    if stream.delta_frames.intra_only()
                        && buffer.flags().contains(gst::BufferFlags::DELTA_UNIT)
                    {
                        gst::error!(CAT, obj: stream.sinkpad, "Intra-only stream with delta units");
                        return Err(gst::FlowError::Error);
                    }

                    let pts_position = buffer.pts().ok_or_else(|| {
                        gst::error!(CAT, obj: stream.sinkpad, "Require timestamped buffers");
                        gst::FlowError::Error
                    })?;

                    let next_timestamp_position = if stream.delta_frames.requires_dts() {
                        // Was checked above
                        buffer.dts().unwrap()
                    } else {
                        pts_position
                    };

                    let segment = match stream.sinkpad.segment().downcast::<gst::ClockTime>().ok() {
                        Some(segment) => segment,
                        None => {
                            gst::error!(CAT, obj: stream.sinkpad, "Got buffer before segment");
                            return Err(gst::FlowError::Error);
                        }
                    };

                    // If the stream has no valid running time, assume it's before everything else.
                    let next_timestamp = match segment.to_running_time_full(next_timestamp_position)
                    {
                        None => {
                            gst::error!(CAT, obj: stream.sinkpad, "Stream has no valid running time");
                            return Err(gst::FlowError::Error);
                        }
                        Some(running_time) => running_time,
                    };

                    gst::trace!(
                        CAT,
                        obj: stream.sinkpad,
                        "Stream has buffer with timestamp {next_timestamp} queued",
                    );

                    let dur = next_timestamp
                        .saturating_sub(timestamp)
                        .positive()
                        .unwrap_or_else(|| {
                            gst::warning!(
                                CAT,
                                obj: stream.sinkpad,
                                "Stream timestamps going backwards {next_timestamp} < {timestamp}",
                            );
                            gst::ClockTime::ZERO
                        });

                    gst::trace!(
                        CAT,
                        obj: stream.sinkpad,
                        "Using {dur} as duration for queued buffer",
                    );

                    let pts = pts + dur;
                    if stream.end_pts.map_or(true, |end_pts| end_pts < pts) {
                        gst::trace!(CAT, obj: stream.sinkpad, "Stream end PTS {pts}");
                        stream.end_pts = Some(pts);
                    }

                    *duration = Some(dur);

                    return Ok(());
                }
                None => {
                    // Have no buffer queued at all yet

                    let buffer = match stream.sinkpad.pop_buffer() {
                        Some(buffer) => buffer,
                        None => {
                            if stream.sinkpad.is_eos() {
                                gst::trace!(
                                    CAT,
                                    obj: stream.sinkpad,
                                    "Stream is EOS",
                                );

                                return Err(gst::FlowError::Eos);
                            } else {
                                gst::trace!(CAT, obj: stream.sinkpad, "Stream has no buffer queued");
                                return Err(gst_base::AGGREGATOR_FLOW_NEED_DATA);
                            }
                        }
                    };

                    if stream.delta_frames.requires_dts() && buffer.dts().is_none() {
                        gst::error!(CAT, obj: stream.sinkpad, "Require DTS for video streams");
                        return Err(gst::FlowError::Error);
                    }

                    if stream.delta_frames.intra_only()
                        && buffer.flags().contains(gst::BufferFlags::DELTA_UNIT)
                    {
                        gst::error!(CAT, obj: stream.sinkpad, "Intra-only stream with delta units");
                        return Err(gst::FlowError::Error);
                    }

                    let pts_position = buffer.pts().ok_or_else(|| {
                        gst::error!(CAT, obj: stream.sinkpad, "Require timestamped buffers");
                        gst::FlowError::Error
                    })?;
                    let dts_position = buffer.dts();

                    let segment = match stream
                        .sinkpad
                        .segment()
                        .clone()
                        .downcast::<gst::ClockTime>()
                        .ok()
                    {
                        Some(segment) => segment,
                        None => {
                            gst::error!(CAT, obj: stream.sinkpad, "Got buffer before segment");
                            return Err(gst::FlowError::Error);
                        }
                    };

                    let pts = match segment.to_running_time_full(pts_position) {
                        None => {
                            gst::error!(CAT, obj: stream.sinkpad, "Stream has no valid PTS running time");
                            return Err(gst::FlowError::Error);
                        }
                        Some(running_time) => running_time,
                    }.positive().unwrap_or_else(|| {
                        gst::error!(CAT, obj: stream.sinkpad, "Stream has negative PTS running time");
                        gst::ClockTime::ZERO
                    });

                    let dts = match dts_position {
                        None => None,
                        Some(dts_position) => match segment.to_running_time_full(dts_position) {
                            None => {
                                gst::error!(CAT, obj: stream.sinkpad, "Stream has no valid DTS running time");
                                return Err(gst::FlowError::Error);
                            }
                            Some(running_time) => Some(running_time),
                        },
                    };

                    let timestamp = if stream.delta_frames.requires_dts() {
                        // Was checked above
                        let dts = dts.unwrap();

                        if stream.start_dts.is_none() {
                            gst::debug!(CAT, obj: stream.sinkpad, "Stream start DTS {dts}");
                            stream.start_dts = Some(dts);
                        }

                        dts
                    } else {
                        gst::Signed::Positive(pts)
                    };

                    if stream
                        .earliest_pts
                        .map_or(true, |earliest_pts| earliest_pts > pts)
                    {
                        gst::debug!(CAT, obj: stream.sinkpad, "Stream earliest PTS {pts}");
                        stream.earliest_pts = Some(pts);
                    }

                    let composition_time_offset = if stream.delta_frames.requires_dts() {
                        let pts = gst::Signed::Positive(pts);
                        let dts = dts.unwrap(); // set above

                        if pts > dts {
                            Some(i64::try_from((pts - dts).nseconds().positive().unwrap()).map_err(|_| {
                                gst::error!(CAT, obj: stream.sinkpad, "Too big PTS/DTS difference");
                                gst::FlowError::Error
                            })?)
                        } else {
                            let diff = i64::try_from((dts - pts).nseconds().positive().unwrap()).map_err(|_| {
                                gst::error!(CAT, obj: stream.sinkpad, "Too big PTS/DTS difference");
                                gst::FlowError::Error
                            })?;
                            Some(-diff)
                        }
                    } else {
                        None
                    };

                    gst::trace!(
                        CAT,
                        obj: stream.sinkpad,
                        "Stream has buffer of size {} with timestamp {timestamp} pending",
                        buffer.size(),
                    );

                    stream.pending_buffer = Some(PendingBuffer {
                        buffer,
                        timestamp,
                        pts,
                        composition_time_offset,
                        duration: None,
                    });
                }
            }
        }
    }

    fn find_earliest_stream(
        &self,
        settings: &Settings,
        state: &mut State,
    ) -> Result<Option<usize>, gst::FlowError> {
        if let Some(current_stream_idx) = state.current_stream_idx {
            // If a stream was previously selected, check if another buffer from
            // this stream can be consumed or if that would exceed the interleave.

            let single_stream = state.streams.len() == 1;
            let stream = &mut state.streams[current_stream_idx];

            match self.queue_buffer(stream) {
                Ok(_) => {
                    assert!(matches!(
                        stream.pending_buffer,
                        Some(PendingBuffer {
                            duration: Some(_),
                            ..
                        })
                    ));

                    if single_stream
                        || (settings.interleave_bytes.map_or(true, |interleave_bytes| {
                            interleave_bytes >= stream.queued_chunk_bytes
                        }) && settings.interleave_time.map_or(true, |interleave_time| {
                            interleave_time >= stream.queued_chunk_time
                        }))
                    {
                        gst::trace!(CAT,
                            obj: stream.sinkpad,
                            "Continuing current chunk: single stream {}, or {} >= {} and {} >= {}",
                            single_stream,
                            gst::format::Bytes::from_u64(stream.queued_chunk_bytes),
                            settings.interleave_bytes.map(gst::format::Bytes::from_u64).display(),
                            stream.queued_chunk_time, settings.interleave_time.display(),
                        );
                        return Ok(Some(current_stream_idx));
                    }

                    state.current_stream_idx = None;
                    gst::debug!(CAT,
                        obj: stream.sinkpad,
                        "Switching to next chunk: {} < {} and {} < {}",
                        gst::format::Bytes::from_u64(stream.queued_chunk_bytes),
                        settings.interleave_bytes.map(gst::format::Bytes::from_u64).display(),
                        stream.queued_chunk_time, settings.interleave_time.display(),
                    );
                }
                Err(gst::FlowError::Eos) => {
                    gst::debug!(CAT, obj: stream.sinkpad, "Stream is EOS, switching to next stream");
                    state.current_stream_idx = None;
                }
                Err(err) => {
                    return Err(err);
                }
            }
        }

        // Otherwise find the next earliest stream here
        let mut earliest_stream = None;
        let mut all_have_data_or_eos = true;
        let mut all_eos = true;

        for (idx, stream) in state.streams.iter_mut().enumerate() {
            // First queue a buffer on each stream and try to get the duration

            match self.queue_buffer(stream) {
                Ok(_) => {
                    assert!(matches!(
                        stream.pending_buffer,
                        Some(PendingBuffer {
                            duration: Some(_),
                            ..
                        })
                    ));

                    let timestamp = stream.pending_buffer.as_ref().unwrap().timestamp;

                    gst::trace!(CAT,
                        obj: stream.sinkpad,
                        "Stream at timestamp {timestamp}",
                    );

                    all_eos = false;

                    if earliest_stream
                        .as_ref()
                        .map_or(true, |(_idx, _stream, earliest_timestamp)| {
                            *earliest_timestamp > timestamp
                        })
                    {
                        earliest_stream = Some((idx, stream, timestamp));
                    }
                }
                Err(gst::FlowError::Eos) => {
                    all_eos &= true;
                    continue;
                }
                Err(gst_base::AGGREGATOR_FLOW_NEED_DATA) => {
                    all_have_data_or_eos = false;
                    continue;
                }
                Err(err) => {
                    return Err(err);
                }
            }
        }

        if !all_have_data_or_eos {
            gst::trace!(CAT, imp: self, "Not all streams have a buffer or are EOS");
            Err(gst_base::AGGREGATOR_FLOW_NEED_DATA)
        } else if all_eos {
            gst::info!(CAT, imp: self, "All streams are EOS");
            Err(gst::FlowError::Eos)
        } else if let Some((idx, stream, earliest_timestamp)) = earliest_stream {
            gst::debug!(
                CAT,
                obj: stream.sinkpad,
                "Stream is earliest stream with timestamp {earliest_timestamp}",
            );

            gst::debug!(
                CAT,
                obj: stream.sinkpad,
                "Starting new chunk at offset {}",
                state.current_offset,
            );

            stream.chunks.push(super::Chunk {
                offset: state.current_offset,
                samples: Vec::new(),
            });
            stream.queued_chunk_time = gst::ClockTime::ZERO;
            stream.queued_chunk_bytes = 0;

            state.current_stream_idx = Some(idx);
            Ok(Some(idx))
        } else {
            unreachable!()
        }
    }

    fn drain_buffers(
        &self,
        settings: &Settings,
        state: &mut State,
        buffers: &mut gst::BufferListRef,
    ) -> Result<(), gst::FlowError> {
        // Now we can start handling buffers
        while let Some(idx) = self.find_earliest_stream(settings, state)? {
            let stream = &mut state.streams[idx];

            let buffer = stream.pending_buffer.take().unwrap();
            let duration = buffer.duration.unwrap();
            let composition_time_offset = buffer.composition_time_offset;
            let mut buffer = buffer.buffer;

            stream.queued_chunk_time += duration;
            stream.queued_chunk_bytes += buffer.size() as u64;

            stream
                .chunks
                .last_mut()
                .unwrap()
                .samples
                .push(super::Sample {
                    sync_point: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
                    duration,
                    composition_time_offset,
                    size: buffer.size() as u32,
                });

            {
                let buffer = buffer.make_mut();
                buffer.set_dts(None);
                buffer.set_pts(None);
                buffer.set_duration(duration);
                buffer.unset_flags(gst::BufferFlags::all());
            }

            state.current_offset += buffer.size() as u64;
            state.mdat_size += buffer.size() as u64;
            buffers.add(buffer);
        }

        Ok(())
    }

    fn create_streams(&self, state: &mut State) -> Result<(), gst::FlowError> {
        gst::info!(CAT, imp: self, "Creating streams");

        for pad in self
            .obj()
            .sink_pads()
            .into_iter()
            .map(|pad| pad.downcast::<super::MP4MuxPad>().unwrap())
        {
            let caps = match pad.current_caps() {
                Some(caps) => caps,
                None => {
                    gst::warning!(CAT, obj: pad, "Skipping pad without caps");
                    continue;
                }
            };

            gst::info!(CAT, obj: pad, "Configuring caps {:?}", caps);

            let s = caps.structure(0).unwrap();

            let mut delta_frames = super::DeltaFrames::IntraOnly;
            match s.name() {
                "video/x-h264" | "video/x-h265" => {
                    if !s.has_field_with_type("codec_data", gst::Buffer::static_type()) {
                        gst::error!(CAT, obj: pad, "Received caps without codec_data");
                        return Err(gst::FlowError::NotNegotiated);
                    }
                    delta_frames = super::DeltaFrames::Bidirectional;
                }
                "video/x-vp9" => {
                    if !s.has_field_with_type("colorimetry", str::static_type()) {
                        gst::error!(CAT, obj: pad, "Received caps without colorimetry");
                        return Err(gst::FlowError::NotNegotiated);
                    }
                    delta_frames = super::DeltaFrames::PredictiveOnly;
                }
                "image/jpeg" => (),
                "audio/mpeg" => {
                    if !s.has_field_with_type("codec_data", gst::Buffer::static_type()) {
                        gst::error!(CAT, obj: pad, "Received caps without codec_data");
                        return Err(gst::FlowError::NotNegotiated);
                    }
                }
                "audio/x-opus" => {
                    if let Some(header) = s
                        .get::<gst::ArrayRef>("streamheader")
                        .ok()
                        .and_then(|a| a.get(0).and_then(|v| v.get::<gst::Buffer>().ok()))
                    {
                        if gst_pbutils::codec_utils_opus_parse_header(&header, None).is_err() {
                            gst::error!(CAT, obj: pad, "Received invalid Opus header");
                            return Err(gst::FlowError::NotNegotiated);
                        }
                    } else if gst_pbutils::codec_utils_opus_parse_caps(&caps, None).is_err() {
                        gst::error!(CAT, obj: pad, "Received invalid Opus caps");
                        return Err(gst::FlowError::NotNegotiated);
                    }
                }
                "audio/x-alaw" | "audio/x-mulaw" => (),
                "audio/x-adpcm" => (),
                "application/x-onvif-metadata" => (),
                _ => unreachable!(),
            }

            state.streams.push(Stream {
                sinkpad: pad,
                caps,
                delta_frames,
                chunks: Vec::new(),
                pending_buffer: None,
                queued_chunk_time: gst::ClockTime::ZERO,
                queued_chunk_bytes: 0,
                start_dts: None,
                earliest_pts: None,
                end_pts: None,
            });
        }

        if state.streams.is_empty() {
            gst::error!(CAT, imp: self, "No streams available");
            return Err(gst::FlowError::Error);
        }

        // Sort video streams first and then audio streams and then metadata streams, and each group by pad name.
        state.streams.sort_by(|a, b| {
            let order_of_caps = |caps: &gst::CapsRef| {
                let s = caps.structure(0).unwrap();

                if s.name().starts_with("video/") {
                    0
                } else if s.name().starts_with("audio/") {
                    1
                } else if s.name().starts_with("application/x-onvif-metadata") {
                    2
                } else {
                    unimplemented!();
                }
            };

            let st_a = order_of_caps(&a.caps);
            let st_b = order_of_caps(&b.caps);

            if st_a == st_b {
                return a.sinkpad.name().cmp(&b.sinkpad.name());
            }

            st_a.cmp(&st_b)
        });

        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for MP4Mux {
    const NAME: &'static str = "GstRsMP4Mux";
    type Type = super::MP4Mux;
    type ParentType = gst_base::Aggregator;
    type Class = Class;
}

impl ObjectImpl for MP4Mux {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt64::builder("interleave-bytes")
                    .nick("Interleave Bytes")
                    .blurb("Interleave between streams in bytes")
                    .default_value(DEFAULT_INTERLEAVE_BYTES.unwrap_or(0))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("interleave-time")
                    .nick("Interleave Time")
                    .blurb("Interleave between streams in nanoseconds")
                    .default_value(
                        DEFAULT_INTERLEAVE_TIME
                            .map(gst::ClockTime::nseconds)
                            .unwrap_or(u64::MAX),
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("movie-timescale")
                    .nick("Movie Timescale")
                    .blurb("Timescale to use for the movie (units per second, 0 is automatic)")
                    .mutable_ready()
                    .build(),
            ]
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "interleave-bytes" => {
                let mut settings = self.settings.lock().unwrap();
                settings.interleave_bytes = match value.get().expect("type checked upstream") {
                    0 => None,
                    v => Some(v),
                };
            }

            "interleave-time" => {
                let mut settings = self.settings.lock().unwrap();
                settings.interleave_time = match value.get().expect("type checked upstream") {
                    Some(gst::ClockTime::ZERO) | None => None,
                    v => v,
                };
            }

            "movie-timescale" => {
                let mut settings = self.settings.lock().unwrap();
                settings.movie_timescale = value.get().expect("type checked upstream");
            }

            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "interleave-bytes" => {
                let settings = self.settings.lock().unwrap();
                settings.interleave_bytes.unwrap_or(0).to_value()
            }

            "interleave-time" => {
                let settings = self.settings.lock().unwrap();
                settings.interleave_time.to_value()
            }

            "movie-timescale" => {
                let settings = self.settings.lock().unwrap();
                settings.movie_timescale.to_value()
            }

            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for MP4Mux {}

impl ElementImpl for MP4Mux {
    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let state = self.state.lock().unwrap();
        if !state.streams.is_empty() {
            gst::error!(
                CAT,
                imp: self,
                "Can't request new pads after start was generated"
            );
            return None;
        }

        self.parent_request_new_pad(templ, name, caps)
    }
}

impl AggregatorImpl for MP4Mux {
    fn next_time(&self) -> Option<gst::ClockTime> {
        None
    }

    fn sink_query(
        &self,
        aggregator_pad: &gst_base::AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryViewMut;

        gst::trace!(CAT, obj: aggregator_pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryViewMut::Caps(q) => {
                let allowed_caps = aggregator_pad
                    .current_caps()
                    .unwrap_or_else(|| aggregator_pad.pad_template_caps());

                if let Some(filter_caps) = q.filter() {
                    let res = filter_caps
                        .intersect_with_mode(&allowed_caps, gst::CapsIntersectMode::First);
                    q.set_result(&res);
                } else {
                    q.set_result(&allowed_caps);
                }

                true
            }
            _ => self.parent_sink_query(aggregator_pad, query),
        }
    }

    fn sink_event_pre_queue(
        &self,
        aggregator_pad: &gst_base::AggregatorPad,
        mut event: gst::Event,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        use gst::EventView;

        gst::trace!(CAT, obj: aggregator_pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Segment(ev) => {
                if ev.segment().format() != gst::Format::Time {
                    gst::warning!(
                        CAT,
                        obj: aggregator_pad,
                        "Received non-TIME segment, replacing with default TIME segment"
                    );
                    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
                    event = gst::event::Segment::builder(&segment)
                        .seqnum(event.seqnum())
                        .build();
                }
                self.parent_sink_event_pre_queue(aggregator_pad, event)
            }
            _ => self.parent_sink_event_pre_queue(aggregator_pad, event),
        }
    }

    fn sink_event(&self, aggregator_pad: &gst_base::AggregatorPad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::trace!(CAT, obj: aggregator_pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Tag(_ev) => {
                // TODO: Maybe store for putting into the header at the end?

                self.parent_sink_event(aggregator_pad, event)
            }
            _ => self.parent_sink_event(aggregator_pad, event),
        }
    }

    fn src_query(&self, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        gst::trace!(CAT, imp: self, "Handling query {:?}", query);

        match query.view_mut() {
            QueryViewMut::Seeking(q) => {
                // We can't really handle seeking, it would break everything
                q.set(false, gst::ClockTime::ZERO, gst::ClockTime::NONE);
                true
            }
            _ => self.parent_src_query(query),
        }
    }

    fn src_event(&self, event: gst::Event) -> bool {
        use gst::EventView;

        gst::trace!(CAT, imp: self, "Handling event {:?}", event);

        match event.view() {
            EventView::Seek(_ev) => false,
            _ => self.parent_src_event(event),
        }
    }

    fn flush(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        for stream in &mut state.streams {
            stream.pending_buffer = None;
        }
        drop(state);

        self.parent_flush()
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::trace!(CAT, imp: self, "Stopping");

        let _ = self.parent_stop();

        *self.state.lock().unwrap() = State::default();

        Ok(())
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::trace!(CAT, imp: self, "Starting");

        self.parent_start()?;

        // Always output a BYTES segment
        let segment = gst::FormattedSegment::<gst::format::Bytes>::new();
        self.obj().update_segment(&segment);

        *self.state.lock().unwrap() = State::default();

        Ok(())
    }

    fn negotiate(&self) -> bool {
        true
    }

    fn aggregate(&self, _timeout: bool) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.state.lock().unwrap();

        let mut buffers = gst::BufferList::new();
        let mut caps = None;

        // If no streams were created yet, collect all streams now and write the mdat.
        if state.streams.is_empty() {
            // First check if downstream is seekable. If not we can't rewrite the mdat box header!
            drop(state);

            let mut q = gst::query::Seeking::new(gst::Format::Bytes);
            if self.obj().src_pad().peer_query(&mut q) {
                if !q.result().0 {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Mux,
                        ["Downstream is not seekable"]
                    );
                    return Err(gst::FlowError::Error);
                }
            } else {
                // Can't query downstream, have to assume downstream is seekable
                gst::warning!(CAT, imp: self, "Can't query downstream for seekability");
            }

            state = self.state.lock().unwrap();
            self.create_streams(&mut state)?;

            // Create caps now to be sent before any buffers
            caps = Some(
                gst::Caps::builder("video/quicktime")
                    .field("variant", "iso")
                    .build(),
            );

            gst::info!(
                CAT,
                imp: self,
                "Creating ftyp box at offset {}",
                state.current_offset
            );

            // ... and then create the ftyp box plus mdat box header so we can start outputting
            // actual data
            let buffers = buffers.get_mut().unwrap();

            let ftyp = boxes::create_ftyp(self.obj().class().as_ref().variant).map_err(|err| {
                gst::error!(CAT, imp: self, "Failed to create ftyp box: {err}");
                gst::FlowError::Error
            })?;
            state.current_offset += ftyp.size() as u64;
            buffers.add(ftyp);

            gst::info!(
                CAT,
                imp: self,
                "Creating mdat box header at offset {}",
                state.current_offset
            );
            state.mdat_offset = Some(state.current_offset);
            let mdat = boxes::create_mdat_header(None).map_err(|err| {
                gst::error!(CAT, imp: self, "Failed to create mdat box header: {err}");
                gst::FlowError::Error
            })?;
            state.current_offset += mdat.size() as u64;
            state.mdat_size = 0;
            buffers.add(mdat);
        }

        let res = match self.drain_buffers(&settings, &mut state, buffers.get_mut().unwrap()) {
            Ok(_) => Ok(gst::FlowSuccess::Ok),
            Err(err @ gst::FlowError::Eos) | Err(err @ gst_base::AGGREGATOR_FLOW_NEED_DATA) => {
                Err(err)
            }
            Err(err) => return Err(err),
        };

        if res == Err(gst::FlowError::Eos) {
            // Create moov box now and append it to the buffers

            gst::info!(
                CAT,
                imp: self,
                "Creating moov box now, mdat ends at offset {} with size {}",
                state.current_offset,
                state.mdat_size
            );

            let mut streams = Vec::with_capacity(state.streams.len());
            for stream in state.streams.drain(..) {
                let pad_settings = stream.sinkpad.imp().settings.lock().unwrap().clone();
                let (earliest_pts, end_pts) = match Option::zip(stream.earliest_pts, stream.end_pts)
                {
                    Some(res) => res,
                    None => continue, // empty stream
                };

                streams.push(super::Stream {
                    caps: stream.caps.clone(),
                    delta_frames: stream.delta_frames,
                    trak_timescale: pad_settings.trak_timescale,
                    start_dts: stream.start_dts,
                    earliest_pts,
                    end_pts,
                    chunks: stream.chunks,
                });
            }

            let moov = boxes::create_moov(super::Header {
                variant: self.obj().class().as_ref().variant,
                movie_timescale: settings.movie_timescale,
                streams,
            })
            .map_err(|err| {
                gst::error!(CAT, imp: self, "Failed to create moov box: {err}");
                gst::FlowError::Error
            })?;
            state.current_offset += moov.size() as u64;
            buffers.get_mut().unwrap().add(moov);
        }

        drop(state);

        if let Some(ref caps) = caps {
            self.obj().set_src_caps(caps);
        }

        if !buffers.is_empty() {
            if let Err(err) = self.obj().finish_buffer_list(buffers) {
                gst::error!(CAT, imp: self, "Failed pushing buffer: {:?}", err);
                return Err(err);
            }
        }

        if res == Err(gst::FlowError::Eos) {
            let mut state = self.state.lock().unwrap();

            if let Some(mdat_offset) = state.mdat_offset {
                gst::info!(
                    CAT,
                    imp: self,
                    "Rewriting mdat box header at offset {mdat_offset} with size {} now",
                    state.mdat_size,
                );
                let mut segment = gst::FormattedSegment::<gst::format::Bytes>::new();
                segment.set_start(gst::format::Bytes::from_u64(mdat_offset));
                state.current_offset = mdat_offset;
                let mdat = boxes::create_mdat_header(Some(state.mdat_size)).map_err(|err| {
                    gst::error!(CAT, imp: self, "Failed to create mdat box header: {err}");
                    gst::FlowError::Error
                })?;
                drop(state);

                self.obj().update_segment(&segment);
                if let Err(err) = self.obj().finish_buffer(mdat) {
                    gst::error!(
                        CAT,
                        imp: self,
                        "Failed pushing updated mdat box header buffer downstream: {:?}",
                        err,
                    );
                }
            }
        }

        res
    }
}

#[repr(C)]
pub(crate) struct Class {
    parent: gst_base::ffi::GstAggregatorClass,
    variant: super::Variant,
}

unsafe impl ClassStruct for Class {
    type Type = MP4Mux;
}

impl std::ops::Deref for Class {
    type Target = glib::Class<gst_base::Aggregator>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*(&self.parent as *const _ as *const _) }
    }
}

unsafe impl<T: MP4MuxImpl> IsSubclassable<T> for super::MP4Mux {
    fn class_init(class: &mut glib::Class<Self>) {
        Self::parent_class_init::<T>(class);

        let class = class.as_mut();
        class.variant = T::VARIANT;
    }
}

pub(crate) trait MP4MuxImpl: AggregatorImpl {
    const VARIANT: super::Variant;
}

#[derive(Default)]
pub(crate) struct ISOMP4Mux;

#[glib::object_subclass]
impl ObjectSubclass for ISOMP4Mux {
    const NAME: &'static str = "GstISOMP4Mux";
    type Type = super::ISOMP4Mux;
    type ParentType = super::MP4Mux;
}

impl ObjectImpl for ISOMP4Mux {}

impl GstObjectImpl for ISOMP4Mux {}

impl ElementImpl for ISOMP4Mux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "ISOMP4Mux",
                "Codec/Muxer",
                "ISO MP4 muxer",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/quicktime")
                    .field("variant", "iso")
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::with_gtype(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &[
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", gst::List::new(["avc", "avc3"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", gst::List::new(["hvc1", "hev1"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-vp9")
                        .field("profile", gst::List::new(["0", "1", "2", "3"]))
                        .field("chroma-format", gst::List::new(["4:2:0", "4:2:2", "4:4:4"]))
                        .field("bit-depth-luma", gst::List::new([8u32, 10u32, 12u32]))
                        .field("bit-depth-chroma", gst::List::new([8u32, 10u32, 12u32]))
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "raw")
                        .field("channels", gst::IntRange::new(1, u16::MAX as i32))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                    gst::Structure::builder("audio/x-opus")
                        .field("channel-mapping-family", gst::IntRange::new(0i32, 255))
                        .field("channels", gst::IntRange::new(1i32, 8))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                ]
                .into_iter()
                .collect::<gst::Caps>(),
                super::MP4MuxPad::static_type(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl AggregatorImpl for ISOMP4Mux {}

impl MP4MuxImpl for ISOMP4Mux {
    const VARIANT: super::Variant = super::Variant::ISO;
}

#[derive(Default, Clone)]
struct PadSettings {
    trak_timescale: u32,
}

#[derive(Default)]
pub(crate) struct MP4MuxPad {
    settings: Mutex<PadSettings>,
}

#[glib::object_subclass]
impl ObjectSubclass for MP4MuxPad {
    const NAME: &'static str = "GstRsMP4MuxPad";
    type Type = super::MP4MuxPad;
    type ParentType = gst_base::AggregatorPad;
}

impl ObjectImpl for MP4MuxPad {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpecUInt::builder("trak-timescale")
                .nick("Track Timescale")
                .blurb("Timescale to use for the track (units per second, 0 is automatic)")
                .mutable_ready()
                .build()]
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "trak-timescale" => {
                let mut settings = self.settings.lock().unwrap();
                settings.trak_timescale = value.get().expect("type checked upstream");
            }

            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "trak-timescale" => {
                let settings = self.settings.lock().unwrap();
                settings.trak_timescale.to_value()
            }

            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for MP4MuxPad {}

impl PadImpl for MP4MuxPad {}

impl AggregatorPadImpl for MP4MuxPad {
    fn flush(&self, aggregator: &gst_base::Aggregator) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mux = aggregator.downcast_ref::<super::MP4Mux>().unwrap();
        let mut mux_state = mux.imp().state.lock().unwrap();

        for stream in &mut mux_state.streams {
            if stream.sinkpad == *self.obj() {
                stream.pending_buffer = None;
                break;
            }
        }

        drop(mux_state);

        self.parent_flush(aggregator)
    }
}
