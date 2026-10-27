// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-webvttsink
 *
 * Collects fragmented WebVTT buffers and generates HLS WebVTT playlist
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 filesrc location=mystream.mp4 ! parsebin ! h264ccextractor ! queue ! ccconverter ! cea608tojson ! jsontovtt ! hlswebvttsink target-duration=5
 * ]| This pipeline extracts CEA-708 closed captions from an H.264 stream
 * and converts them into fragmented WebVTT via cea608tojson and jsontovtt.
 * hlswebvttsink then collects fragmented WebVTT buffers and writes an HLS playlist.
 *
 * Since: plugins-rs-0.15.0
 */
use crate::HlsBaseSink;
use crate::hlsbasesink::HlsBaseSinkImpl;
use crate::hlssink3::HlsSink3PlaylistType;
use crate::playlist::Playlist;
use gio::prelude::*;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use m3u8_rs::{MediaPlaylist, MediaPlaylistType, MediaSegment};
use std::io::Write;
use std::sync::LazyLock;
use std::sync::Mutex;

const DEFAULT_LOCATION: &str = "segment%05d.vtt";
const DEFAULT_TARGET_DURATION: u32 = 15;
const DEFAULT_PLAYLIST_TYPE: HlsSink3PlaylistType = HlsSink3PlaylistType::Unspecified;
const DEFAULT_SYNC: bool = true;
const DEFAULT_MPEGTS_TIME_OFFSET: u64 = 60 * 60 * 90000;
const DEFAULT_ENABLE_TIMESTAMP_MAP: bool = true;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "hlswebvttsink",
        gst::DebugColorFlags::empty(),
        Some("HLS WebVTT sink"),
    )
});

macro_rules! base_imp {
    ($i:expr) => {
        $i.obj().upcast_ref::<HlsBaseSink>().imp()
    };
}

struct HlsWebvttSinkSettings {
    location: String,
    target_duration: u32,
    playlist_type: Option<MediaPlaylistType>,
    sync: bool,
    enable_timestamp_map: bool,
    mpegts_time_offset: u64,

    appsink: gst_app::AppSink,
}

impl Default for HlsWebvttSinkSettings {
    fn default() -> Self {
        let appsink = gst_app::AppSink::builder()
            .sync(DEFAULT_SYNC)
            .name("sink")
            .caps(&gst::Caps::builder("application/x-subtitle-vtt-fragmented").build())
            .build();

        Self {
            location: String::from(DEFAULT_LOCATION),
            target_duration: DEFAULT_TARGET_DURATION,
            playlist_type: None,
            sync: DEFAULT_SYNC,
            enable_timestamp_map: DEFAULT_ENABLE_TIMESTAMP_MAP,
            mpegts_time_offset: DEFAULT_MPEGTS_TIME_OFFSET,
            appsink,
        }
    }
}

struct FragmentData {
    running_time_start: gst::ClockTime,
    duration: gst::ClockTime,
    buffers: Vec<gst::Buffer>,
}

#[derive(Default)]
struct HlsWebvttSinkState {
    segment_idx: u32,
    offset: u64,
    running_time_start: Option<gst::ClockTime>,
    running_time_end: Option<gst::ClockTime>,
    running_time_in_mpegts: u64,
    mpegts_timestamp_map: Option<String>,
    buffers: Vec<gst::Buffer>,
}

#[derive(Default)]
pub struct HlsWebvttSink {
    settings: Mutex<HlsWebvttSinkSettings>,
    state: Mutex<HlsWebvttSinkState>,
}

#[glib::object_subclass]
impl ObjectSubclass for HlsWebvttSink {
    const NAME: &'static str = "GstHlsWebvttSink";
    type Type = super::HlsWebvttSink;
    type ParentType = HlsBaseSink;
}

impl ObjectImpl for HlsWebvttSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("location")
                    .nick("Location")
                    .blurb("Location of the fragment file to write")
                    .default_value(Some(DEFAULT_LOCATION))
                    .build(),
                glib::ParamSpecUInt::builder("target-duration")
                    .nick("Target duration")
                    .blurb("The target duration in seconds of a segment/file. (0 - disabled, useful for management of segment duration by the streaming server)")
                    .default_value(DEFAULT_TARGET_DURATION)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("playlist-type", DEFAULT_PLAYLIST_TYPE)
                    .nick("Playlist Type")
                    .blurb("The type of the playlist to use. When VOD type is set, the playlist will be live until the pipeline ends execution.")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("sync")
                    .nick("Sync")
                    .blurb("Sync on the clock")
                    .default_value(DEFAULT_SYNC)
                    .build(),
                glib::ParamSpecBoolean::builder("enable-timestamp-map")
                    .nick("Enable Timestamp Map")
                    .blurb("Write X-TIMESTAMP-MAP tag to WebVTT segments")
                    .default_value(DEFAULT_ENABLE_TIMESTAMP_MAP)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("mpegts-time-offset")
                    .nick("MPEG TS Time Offset")
                    .blurb("Time offset, in MPEG-TS time (90 kHz clock), corresponding to running-time zero. \
                            The default value is 324000000 (1 hour, i.e. 60 * 60 * 90000), which matches the \
                            offset used by the mpegtsmux element. \
                            If set to UINT64_MAX, the element will use a fixed EXT-X-TIMESTAMP-MAP tag \
                            (i.e. MPEGTS:0, LOCAL:00:00:00.000) without handling MPEG-TS PTS wrap-around. \
                            This can be useful when generating WebVTT playlists for fMP4-based HLS, \
                            where MPEG-TS PES timestamp wrap-around does not apply")
                    .default_value(DEFAULT_MPEGTS_TIME_OFFSET)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "location" => {
                settings.location = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| DEFAULT_LOCATION.into());
            }
            "target-duration" => {
                settings.target_duration = value.get().expect("type checked upstream");
            }
            "playlist-type" => {
                settings.playlist_type = value
                    .get::<HlsSink3PlaylistType>()
                    .expect("type checked upstream")
                    .into();
            }
            "sync" => {
                settings.sync = value.get().expect("type checked upstream");
                settings.appsink.set_property("sync", settings.sync);
            }
            "enable-timestamp-map" => {
                settings.enable_timestamp_map = value.get().expect("type checked upstream");
            }
            "mpegts-time-offset" => {
                settings.mpegts_time_offset = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "location" => settings.location.to_value(),
            "target-duration" => settings.target_duration.to_value(),
            "playlist-type" => {
                let playlist_type: HlsSink3PlaylistType = settings.playlist_type.as_ref().into();
                playlist_type.to_value()
            }
            "sync" => settings.sync.to_value(),
            "enable-timestamp-map" => settings.enable_timestamp_map.to_value(),
            "mpegts-time-offset" => settings.mpegts_time_offset.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        let settings = self.settings.lock().unwrap();

        obj.add(&settings.appsink).unwrap();

        let sinkpad = settings.appsink.static_pad("sink").unwrap();
        let gpad = gst::GhostPad::builder_with_target(&sinkpad)
            .unwrap()
            .build();

        obj.add_pad(&gpad).unwrap();

        let self_weak = self.downgrade();
        let self_eos_weak = self.downgrade();
        settings.appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let Some(imp) = self_weak.upgrade() else {
                        return Err(gst::FlowError::Eos);
                    };

                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    imp.on_new_sample(sample)
                })
                .eos(move |_sink| {
                    let Some(imp) = self_eos_weak.upgrade() else {
                        return;
                    };

                    imp.close();
                })
                .build(),
        );
    }
}

impl GstObjectImpl for HlsWebvttSink {}

impl ElementImpl for HlsWebvttSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "HTTP Live Streaming WebVTT Sink",
                "Sink/Muxer",
                "HTTP Live Streaming WebVTT Sink",
                "Seungha Yang <seungha@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("application/x-subtitle-vtt-fragmented").build();
            let pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::ReadyToPaused => {
                let (target_duration, playlist_type, segment_template) = {
                    let settings = self.settings.lock().unwrap();
                    (
                        settings.target_duration,
                        settings.playlist_type.clone(),
                        settings.location.clone(),
                    )
                };

                let playlist = self.start(target_duration, playlist_type);
                base_imp!(self).open_playlist(playlist, segment_template.clone());
            }
            gst::StateChange::PausedToReady => {
                self.close();
            }
            _ => (),
        }

        self.parent_change_state(transition)
    }
}

impl BinImpl for HlsWebvttSink {}

impl HlsBaseSinkImpl for HlsWebvttSink {}

impl HlsWebvttSink {
    fn start(&self, target_duration: u32, playlist_type: Option<MediaPlaylistType>) -> Playlist {
        gst::info!(CAT, imp = self, "Starting");

        let mut state = self.state.lock().unwrap();
        *state = HlsWebvttSinkState::default();

        let (turn_vod, playlist_type) = if playlist_type == Some(MediaPlaylistType::Vod) {
            (true, Some(MediaPlaylistType::Event))
        } else {
            (false, playlist_type)
        };

        let playlist = MediaPlaylist {
            version: Some(3),
            target_duration: target_duration as u64,
            playlist_type: playlist_type.clone(),
            ..Default::default()
        };

        Playlist::new(playlist, turn_vod, false)
    }

    fn on_new_fragment(
        &self,
        state: &mut HlsWebvttSinkState,
    ) -> Result<(gio::OutputStreamWrite<gio::OutputStream>, String), String> {
        let (stream, location) = base_imp!(self)
            .get_fragment_stream(state.segment_idx)
            .ok_or_else(|| String::from("Error while getting fragment stream"))?;

        state.segment_idx += 1;

        Ok((stream.into_write(), location))
    }

    fn add_segment(
        &self,
        duration: gst::ClockTime,
        running_time: Option<gst::ClockTime>,
        location: String,
        byte_range: Option<m3u8_rs::ByteRange>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let uri = base_imp!(self).get_segment_uri(&location, None);

        base_imp!(self).add_segment(
            &location,
            running_time,
            duration,
            None,
            MediaSegment {
                uri,
                duration: duration.mseconds() as f32 / 1_000f32,
                byte_range,
                ..Default::default()
            },
        )
    }

    fn clip(
        &self,
        segment: &gst::FormattedSegment<gst::ClockTime>,
        mut buffer: gst::Buffer,
    ) -> Option<gst::Buffer> {
        if buffer.pts().is_none() {
            gst::warning!(CAT, imp = self, "Need timestamped buffer");
            return None;
        };

        let Some((clipped_start, clipped_stop)) =
            segment.clip(buffer.pts(), buffer.pts().opt_add(buffer.duration()))
        else {
            gst::debug!(
                CAT,
                imp = self,
                "Dropping out-of-segment buffer {:?}",
                buffer
            );

            return None;
        };

        let buf_mut = buffer.make_mut();
        buf_mut.set_pts(clipped_start);
        if buf_mut.duration().is_some() {
            buf_mut.set_duration(clipped_stop.opt_sub(clipped_start));
        }

        Some(buffer)
    }

    fn on_new_sample(&self, sample: gst::Sample) -> Result<gst::FlowSuccess, gst::FlowError> {
        let buffer = sample.buffer_owned().unwrap();

        gst::log!(CAT, imp = self, "Handle buffer {:?}", buffer);

        let segment = sample
            .segment()
            .unwrap()
            .downcast_ref::<gst::ClockTime>()
            .unwrap();

        let Some(mut buffer) = self.clip(segment, buffer) else {
            return Ok(gst::FlowSuccess::Ok);
        };

        let running_time = segment.to_running_time(buffer.pts()).unwrap();

        let mut state = self.state.lock().unwrap();

        let mut fku = None;
        let mut drained = None;
        if buffer.flags().contains(gst::BufferFlags::HEADER)
            || !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT)
        {
            gst::debug!(CAT, imp = self, "Found header buffer");

            buffer = self.insert_timestamp_map(&mut state, buffer, running_time)?;

            if let Some(running_time_start) = state.running_time_start {
                drained = self.drain(
                    &mut state,
                    running_time_start,
                    running_time - running_time_start,
                );
            }

            let target_dur = self.settings.lock().unwrap().target_duration as u64;
            if target_dur > 0 {
                fku = Some(
                    gst_video::UpstreamForceKeyUnitEvent::builder()
                        .running_time(running_time + gst::ClockTime::from_seconds(target_dur))
                        .all_headers(true)
                        .build(),
                );
            }

            state.running_time_start = Some(running_time);
        } else if state.buffers.is_empty() {
            gst::debug!(CAT, imp = self, "Dropping {:?} before header", buffer);

            let ev = gst_video::UpstreamForceKeyUnitEvent::builder()
                .running_time(running_time)
                .all_headers(true)
                .build();
            let pad = self.obj().static_pad("sink").unwrap();
            drop(state);
            let _ = pad.push_event(ev);

            return Ok(gst::FlowSuccess::Ok);
        }

        let running_time = if let Some(dur) = buffer.duration() {
            running_time + dur
        } else {
            running_time
        };

        state.running_time_end = Some(running_time);
        state.buffers.push(buffer);
        drop(state);

        if let Some(fku) = fku.take() {
            gst::debug!(CAT, imp = self, "Sending force-keyunit event {:?}", fku);
            let pad = self.obj().static_pad("sink").unwrap();
            let _ = pad.push_event(fku);
        }

        if let Some(drained) = drained.take() {
            self.write_segment(drained)
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn write_segment(&self, data: FragmentData) -> Result<gst::FlowSuccess, gst::FlowError> {
        let is_single_media_file = base_imp!(self).is_single_media_file();

        let mut state = self.state.lock().unwrap();
        let (mut stream, location) = self.on_new_fragment(&mut state).map_err(|err| {
            gst::error!(
                CAT,
                imp = self,
                "Couldn't get output stream for segment, {err}",
            );
            gst::FlowError::Error
        })?;

        gst::trace!(
            CAT,
            imp = self,
            "Writing buffer for segment: {location} with running_time: {:?}",
            data.running_time_start
        );

        for buffer in data.buffers.iter() {
            let map = buffer.map_readable().unwrap();
            stream.write_all(&map).map_err(|_| {
                gst::error!(CAT, imp = self, "Couldn't write segment to output stream",);
                gst::FlowError::Error
            })?;
        }

        stream.flush().map_err(|_| {
            gst::error!(CAT, imp = self, "Couldn't flush output stream",);
            gst::FlowError::Error
        })?;

        let byte_range = if !is_single_media_file {
            None
        } else {
            let mut length = 0;
            for buf in data.buffers {
                length += buf.size() as u64;
            }

            let offset = Some(state.offset);
            state.offset += length;

            Some(m3u8_rs::ByteRange { length, offset })
        };

        self.add_segment(
            data.duration,
            Some(data.running_time_start),
            location,
            byte_range,
        )
    }

    fn to_mpegtime(time: gst::ClockTime, offset: u64) -> u64 {
        let mpegtime = time
            .mul_div_round(90_000, gst::ClockTime::SECOND.nseconds())
            .unwrap()
            .nseconds()
            .saturating_add(offset);

        // Takes 33bits to cover rollover
        mpegtime & 0x1ffffffff
    }

    fn split_time(time: gst::ClockTime) -> (u64, u8, u8, u16) {
        let time = time.nseconds();

        let mut s = time / 1_000_000_000;
        let mut m = s / 60;
        let h = m / 60;
        s %= 60;
        m %= 60;
        let ns = time % 1_000_000_000;

        (h, m as u8, s as u8, (ns / 1_000_000) as u16)
    }

    fn insert_timestamp_map(
        &self,
        state: &mut HlsWebvttSinkState,
        buffer: gst::Buffer,
        running_time: gst::ClockTime,
    ) -> Result<gst::Buffer, gst::FlowError> {
        let settings = self.settings.lock().unwrap();

        if !settings.enable_timestamp_map {
            return Ok(buffer);
        }

        // mpegts_time_offset == u64::MAX means using a fixed EXT-X-TIMESTAMP-MAP
        // with MPEGTS:0 and LOCAL:00:00:00.000, so no MPEG-TS rollover tracking
        // is needed. Otherwise, track MPEG-TS timestamp rollover here.
        if settings.mpegts_time_offset != u64::MAX {
            let running_time_in_mpegts =
                Self::to_mpegtime(running_time, settings.mpegts_time_offset);
            if running_time_in_mpegts < state.running_time_in_mpegts {
                state.mpegts_timestamp_map = None;
            }

            state.running_time_in_mpegts = running_time_in_mpegts;
        }

        let mpegts_timestamp_map = state.mpegts_timestamp_map.get_or_insert_with(|| {
            if settings.mpegts_time_offset == u64::MAX {
                "X-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000".to_string()
            } else {
                let (h, m, s, ms) = Self::split_time(buffer.pts().unwrap());
                format!(
                    "X-TIMESTAMP-MAP=MPEGTS:{},LOCAL:{h:02}:{m:02}:{s:02}.{ms:03}",
                    state.running_time_in_mpegts
                )
            }
        });

        let map = buffer.map_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "Couldn't map buffer");
            gst::FlowError::Error
        })?;
        let data = map.as_slice();

        // Find location to write timestamp tag
        const WEBVTT_HDR: &[u8] = b"WEBVTT";
        const WEBVTT_BOM_HDR: &[u8] = b"\xEF\xBB\xBFWEBVTT";

        if !data.starts_with(WEBVTT_HDR) && !data.starts_with(WEBVTT_BOM_HDR) {
            gst::error!(CAT, imp = self, "Invalid WebVTT header");
            return Err(gst::FlowError::Error);
        }

        let mut s = String::from_utf8_lossy(data).into_owned();
        drop(map);

        // Find first line terminator position (CRLF / LF / CR)
        if let Some(next_line_pos) = s
            .find("\r\n")
            .map(|p| p + 2)
            .or_else(|| s.find('\n').map(|p| p + 1))
            .or_else(|| s.find('\r').map(|p| p + 1))
        {
            gst::debug!(
                CAT,
                imp = self,
                "Found line terminator position at {next_line_pos}"
            );
            s.insert_str(next_line_pos, mpegts_timestamp_map);
            s.insert(next_line_pos + mpegts_timestamp_map.len(), '\n')
        } else {
            gst::warning!(CAT, imp = self, "Couldn't find WebVTT line terminator");
            s.push('\n');
            s.push_str(mpegts_timestamp_map);
            s.push('\n');
        }

        let mut out = gst::Buffer::from_mut_slice(s.into_bytes());
        {
            let out_mut = out.get_mut().unwrap();
            out_mut.set_pts(buffer.pts());
            out_mut.set_duration(buffer.duration());
            out_mut.set_flags(buffer.flags());
        }

        Ok(out)
    }

    fn drain(
        &self,
        state: &mut HlsWebvttSinkState,
        running_time_start: gst::ClockTime,
        duration: gst::ClockTime,
    ) -> Option<FragmentData> {
        if state.buffers.is_empty() {
            return None;
        }

        Some(FragmentData {
            running_time_start,
            duration,
            buffers: std::mem::take(&mut state.buffers),
        })
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();

        if state.buffers.is_empty()
            || state.running_time_start.is_none()
            || state.running_time_end.is_none()
        {
            state.buffers.clear();
            return;
        }

        let running_time_start = state.running_time_start.unwrap();
        let dur = state.running_time_end.unwrap() - running_time_start;
        let mut drained = self.drain(&mut state, running_time_start, dur);
        drop(state);

        if let Some(drained) = drained.take() {
            let _ = self.write_segment(drained);
        }
    }
}
