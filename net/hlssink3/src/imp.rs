// Copyright (C) 2021 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::playlist::Playlist;
use crate::HlsSink3PlaylistType;
use chrono::{DateTime, Duration, Utc};
use gio::prelude::*;
use gst::glib;
use gst::glib::once_cell::sync::Lazy;
use gst::prelude::*;
use gst::subclass::prelude::*;
use m3u8_rs::MediaPlaylistType;
use std::fs;
use std::io::Write;
use std::path;
use std::sync::Mutex;

const DEFAULT_LOCATION: &str = "segment%05d.ts";
const DEFAULT_PLAYLIST_LOCATION: &str = "playlist.m3u8";
const DEFAULT_MAX_NUM_SEGMENT_FILES: u32 = 10;
const DEFAULT_TARGET_DURATION: u32 = 15;
const DEFAULT_PLAYLIST_LENGTH: u32 = 5;
const DEFAULT_PLAYLIST_TYPE: HlsSink3PlaylistType = HlsSink3PlaylistType::Unspecified;
const DEFAULT_I_FRAMES_ONLY_PLAYLIST: bool = false;
const DEFAULT_PROGRAM_DATE_TIME_TAG: bool = false;
const DEFAULT_CLOCK_TRACKING_FOR_PDT: bool = true;
const DEFAULT_SEND_KEYFRAME_REQUESTS: bool = true;

const SIGNAL_GET_PLAYLIST_STREAM: &str = "get-playlist-stream";
const SIGNAL_GET_FRAGMENT_STREAM: &str = "get-fragment-stream";
const SIGNAL_DELETE_FRAGMENT: &str = "delete-fragment";

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new("hlssink3", gst::DebugColorFlags::empty(), Some("HLS sink"))
});

impl From<HlsSink3PlaylistType> for Option<MediaPlaylistType> {
    fn from(pl_type: HlsSink3PlaylistType) -> Self {
        use HlsSink3PlaylistType::*;
        match pl_type {
            Unspecified => None,
            Event => Some(MediaPlaylistType::Event),
            Vod => Some(MediaPlaylistType::Vod),
        }
    }
}

impl From<Option<&MediaPlaylistType>> for HlsSink3PlaylistType {
    fn from(inner_pl_type: Option<&MediaPlaylistType>) -> Self {
        use HlsSink3PlaylistType::*;
        match inner_pl_type {
            None | Some(MediaPlaylistType::Other(_)) => Unspecified,
            Some(MediaPlaylistType::Event) => Event,
            Some(MediaPlaylistType::Vod) => Vod,
        }
    }
}

struct Settings {
    location: String,
    playlist_location: String,
    playlist_root: Option<String>,
    playlist_length: u32,
    playlist_type: Option<MediaPlaylistType>,
    max_num_segment_files: usize,
    target_duration: u32,
    i_frames_only: bool,
    enable_program_date_time: bool,
    pdt_follows_pipeline_clock: bool,
    send_keyframe_requests: bool,

    splitmuxsink: gst::Element,
    giostreamsink: gst::Element,
    video_sink: bool,
    audio_sink: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let splitmuxsink = gst::ElementFactory::make("splitmuxsink")
            .name("split_mux_sink")
            .build()
            .expect("Could not make element splitmuxsink");
        let giostreamsink = gst::ElementFactory::make("giostreamsink")
            .name("giostream_sink")
            .build()
            .expect("Could not make element giostreamsink");

        // giostreamsink doesn't let go of its stream until the element is finalized, which might
        // be too late for the calling application. Let's try to force it to close while tearing
        // down the pipeline.
        if giostreamsink.has_property("close-on-stop", Some(bool::static_type())) {
            giostreamsink.set_property("close-on-stop", true);
        } else {
            gst::warning!(
                CAT,
                "hlssink3 may sometimes fail to write out the final playlist update. This can be fixed by using giostreamsink from GStreamer 1.24 or later."
            )
        }

        Self {
            location: String::from(DEFAULT_LOCATION),
            playlist_location: String::from(DEFAULT_PLAYLIST_LOCATION),
            playlist_root: None,
            playlist_length: DEFAULT_PLAYLIST_LENGTH,
            playlist_type: None,
            max_num_segment_files: DEFAULT_MAX_NUM_SEGMENT_FILES as usize,
            target_duration: DEFAULT_TARGET_DURATION,
            send_keyframe_requests: DEFAULT_SEND_KEYFRAME_REQUESTS,
            i_frames_only: DEFAULT_I_FRAMES_ONLY_PLAYLIST,
            enable_program_date_time: DEFAULT_PROGRAM_DATE_TIME_TAG,
            pdt_follows_pipeline_clock: DEFAULT_CLOCK_TRACKING_FOR_PDT,

            splitmuxsink,
            giostreamsink,
            video_sink: false,
            audio_sink: false,
        }
    }
}

struct PlaylistContext {
    pdt_base_utc: Option<DateTime<Utc>>,
    pdt_base_running_time: Option<gst::ClockTime>,
    playlist: Playlist,
    fragment_opened_at: Option<gst::ClockTime>,
    fragment_running_time: Option<gst::ClockTime>,
    current_segment_location: Option<String>,
    old_segment_locations: Vec<String>,
}

#[derive(Default)]
struct State {
    context: Option<PlaylistContext>,
}

#[derive(Default)]
pub struct HlsSink3 {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl HlsSink3 {
    fn start(&self) {
        gst::info!(CAT, imp: self, "Starting");

        let (target_duration, playlist_type, i_frames_only) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.target_duration as f32,
                settings.playlist_type.clone(),
                settings.i_frames_only,
            )
        };

        let mut state = self.state.lock().unwrap();
        state.context = Some(PlaylistContext {
            pdt_base_utc: None,
            pdt_base_running_time: None,
            playlist: Playlist::new(target_duration, playlist_type, i_frames_only),
            fragment_opened_at: None,
            fragment_running_time: None,
            current_segment_location: None,
            old_segment_locations: Vec::new(),
        });
    }

    fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(mut context) = state.context.take() {
            if context.playlist.is_rendering() {
                context.playlist.stop();
                let _ = self.write_playlist(&mut context);
            }
        }
    }

    fn on_format_location(&self, fragment_id: u32) -> Result<String, String> {
        gst::info!(
            CAT,
            imp: self,
            "Starting the formatting of the fragment-id: {}",
            fragment_id
        );

        let mut state = self.state.lock().unwrap();
        let context = match state.context.as_mut() {
            Some(context) => context,
            None => {
                gst::error!(
                    CAT,
                    imp: self,
                    "Playlist is not configured",
                );

                return Err(String::from("Playlist is not configured"));
            }
        };

        let settings = self.settings.lock().unwrap();
        let segment_file_location = match sprintf::sprintf!(&settings.location, fragment_id) {
            Ok(file_name) => file_name,
            Err(err) => {
                gst::error!(
                    CAT,
                    imp: self,
                    "Couldn't build file name, err: {:?}", err,
                );
                return Err(String::from("Invalid init segment file pattern"));
            }
        };

        gst::trace!(
            CAT,
            imp: self,
            "Segment location formatted: {}",
            segment_file_location
        );

        context.current_segment_location = Some(segment_file_location.clone());

        let fragment_stream = self
            .obj()
            .emit_by_name::<Option<gio::OutputStream>>(
                SIGNAL_GET_FRAGMENT_STREAM,
                &[&segment_file_location],
            )
            .ok_or_else(|| String::from("Error while getting fragment stream"))?;

        settings
            .giostreamsink
            .set_property("stream", &fragment_stream);

        gst::info!(
            CAT,
            imp: self,
            "New segment location: {:?}",
            context.current_segment_location.as_ref()
        );
        Ok(segment_file_location)
    }

    fn new_file_stream<P>(&self, location: &P) -> Result<gio::OutputStream, String>
    where
        P: AsRef<path::Path>,
    {
        let file = fs::File::create(location).map_err(move |err| {
            let error_msg = gst::error_msg!(
                gst::ResourceError::OpenWrite,
                [
                    "Could not open file {} for writing: {}",
                    location.as_ref().to_str().unwrap(),
                    err.to_string(),
                ]
            );
            self.post_error_message(error_msg);
            err.to_string()
        })?;
        Ok(gio::WriteOutputStream::new(file).upcast())
    }

    fn delete_fragment<P>(&self, location: &P)
    where
        P: AsRef<path::Path>,
    {
        let _ = fs::remove_file(location).map_err(|err| {
            gst::warning!(
                CAT,
                imp: self,
                "Could not delete segment file: {}",
                err.to_string()
            );
        });
    }

    fn on_fragment_closed(&self, closed_at: gst::ClockTime, date_time: Option<DateTime<Utc>>) {
        let mut state = self.state.lock().unwrap();
        let context = match state.context.as_mut() {
            Some(context) => context,
            None => {
                gst::error!(CAT, imp: self, "Playlist is not configured");
                return;
            }
        };

        let location = match context.current_segment_location.take() {
            Some(location) => location,
            None => {
                gst::error!(CAT, imp: self, "Unknown segment location");
                return;
            }
        };

        let opened_at = match context.fragment_opened_at.take() {
            Some(opened_at) => opened_at,
            None => {
                gst::error!(CAT, imp: self, "Unknown segment duration");
                return;
            }
        };

        let duration = ((closed_at - opened_at).mseconds() as f32) / 1_000f32;
        let file_name = path::Path::new(&location)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();

        let settings = self.settings.lock().unwrap();
        let segment_file_name = if let Some(playlist_root) = &settings.playlist_root {
            format!("{playlist_root}/{file_name}")
        } else {
            file_name.to_string()
        };
        drop(settings);

        context
            .playlist
            .add_segment(segment_file_name, duration, date_time);
        context.old_segment_locations.push(location);

        let _ = self.write_playlist(context);
    }

    fn write_playlist(
        &self,
        context: &mut PlaylistContext,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::info!(CAT, imp: self, "Preparing to write new playlist, COUNT {}", context.playlist.len());

        let (playlist_location, max_num_segments, max_playlist_length) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.playlist_location.clone(),
                settings.max_num_segment_files,
                settings.playlist_length as usize,
            )
        };

        context.playlist.update_playlist_state(max_playlist_length);

        // Acquires the playlist file handle so we can update it with new content. By default, this
        // is expected to be the same file every time.
        let mut playlist_stream = self
            .obj()
            .emit_by_name::<Option<gio::OutputStream>>(
                SIGNAL_GET_PLAYLIST_STREAM,
                &[&playlist_location],
            )
            .ok_or_else(|| {
                gst::error!(
                    CAT,
                    imp: self,
                    "Could not get stream to write playlist content",
                );
                gst::FlowError::Error
            })?
            .into_write();

        context
            .playlist
            .write_to(&mut playlist_stream)
            .map_err(|err| {
                gst::error!(
                    CAT,
                    imp: self,
                    "Could not write new playlist: {}",
                    err.to_string()
                );
                gst::FlowError::Error
            })?;
        playlist_stream.flush().map_err(|err| {
            gst::error!(
                CAT,
                imp: self,
                "Could not flush playlist: {}",
                err.to_string()
            );
            gst::FlowError::Error
        })?;

        if context.playlist.is_type_undefined() && max_num_segments > 0 {
            // Cleanup old segments from filesystem
            if context.old_segment_locations.len() > max_num_segments {
                for _ in 0..context.old_segment_locations.len() - max_num_segments {
                    let old_segment_location = context.old_segment_locations.remove(0);
                    if !self
                        .obj()
                        .emit_by_name::<bool>(SIGNAL_DELETE_FRAGMENT, &[&old_segment_location])
                    {
                        gst::error!(CAT, imp: self, "Could not delete fragment");
                    }
                }
            }
        }

        gst::debug!(CAT, imp: self, "Wrote new playlist file!");
        Ok(gst::FlowSuccess::Ok)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for HlsSink3 {
    const NAME: &'static str = "GstHlsSink3";
    type Type = super::HlsSink3;
    type ParentType = gst::Bin;
}

impl BinImpl for HlsSink3 {
    #[allow(clippy::single_match)]
    fn handle_message(&self, msg: gst::Message) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Element(msg) => {
                let event_is_from_splitmuxsink = {
                    let settings = self.settings.lock().unwrap();

                    msg.src() == Some(settings.splitmuxsink.upcast_ref())
                };
                if !event_is_from_splitmuxsink {
                    return;
                }

                let s = msg.structure().unwrap();
                match s.name().as_str() {
                    "splitmuxsink-fragment-opened" => {
                        if let Ok(new_fragment_opened_at) = s.get::<gst::ClockTime>("running-time")
                        {
                            let mut state = self.state.lock().unwrap();
                            if let Some(context) = state.context.as_mut() {
                                context.fragment_opened_at = Some(new_fragment_opened_at);
                            }
                        }
                    }
                    "splitmuxsink-fragment-closed" => {
                        let settings = self.settings.lock().unwrap();
                        let mut state = self.state.lock().unwrap();
                        let context = match state.context.as_mut() {
                            Some(context) => context,
                            None => {
                                gst::error!(
                                    CAT,
                                    imp: self,
                                    "Playlist is not configured",
                                );

                                return;
                            }
                        };

                        let fragment_pts = context
                            .fragment_running_time
                            .expect("fragment running time must be set by format-location-full");

                        if context.pdt_base_running_time.is_none() {
                            context.pdt_base_running_time = context.fragment_running_time;
                        }
                        // Calculate the mapping from running time to UTC
                        // calculate pdt_base_utc for each segment for !pdt_follows_pipeline_clock
                        // when pdt_follows_pipeline_clock is set, we calculate the base time every time
                        // this avoids the drift between pdt tag and external clock (if gst clock has skew w.r.t external clock)
                        if context.pdt_base_utc.is_none() || !settings.pdt_follows_pipeline_clock {
                            let now_utc = Utc::now();
                            let now_gst = settings.giostreamsink.clock().unwrap().time().unwrap();
                            let pts_clock_time =
                                fragment_pts + settings.giostreamsink.base_time().unwrap();

                            let diff = now_gst.checked_sub(pts_clock_time).expect("time between fragments running time and current running time overflow");
                            let pts_utc = now_utc
                                .checked_sub_signed(Duration::nanoseconds(diff.nseconds() as i64))
                                .expect("offsetting the utc with gstreamer clock-diff overflow");

                            context.pdt_base_utc = Some(pts_utc);
                        }

                        let fragment_date_time = if settings.enable_program_date_time
                            && context.pdt_base_running_time.is_some()
                        {
                            // Add the diff of running time to UTC time
                            // date_time = first_segment_utc + (current_seg_running_time - first_seg_running_time)
                            context
                                .pdt_base_utc
                                .unwrap()
                                .checked_add_signed(Duration::nanoseconds(
                                    context
                                        .fragment_running_time
                                        .opt_checked_sub(context.pdt_base_running_time)
                                        .unwrap()
                                        .unwrap()
                                        .nseconds() as i64,
                                ))
                        } else {
                            None
                        };
                        drop(state);
                        drop(settings);

                        if let Ok(fragment_closed_at) = s.get::<gst::ClockTime>("running-time") {
                            self.on_fragment_closed(fragment_closed_at, fragment_date_time);
                        }
                    }
                    _ => {}
                }
            }
            _ => self.parent_handle_message(msg),
        }
    }
}

impl ObjectImpl for HlsSink3 {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("location")
                    .nick("File Location")
                    .blurb("Location of the file to write")
                    .default_value(Some(DEFAULT_LOCATION))
                    .build(),
                glib::ParamSpecString::builder("playlist-location")
                    .nick("Playlist Location")
                    .blurb("Location of the playlist to write.")
                    .default_value(Some(DEFAULT_PLAYLIST_LOCATION))
                    .build(),
                glib::ParamSpecString::builder("playlist-root")
                    .nick("Playlist Root")
                    .blurb("Base path for the segments in the playlist file.")
                    .build(),
                glib::ParamSpecUInt::builder("max-files")
                    .nick("Max files")
                    .blurb("Maximum number of files to keep on disk. Once the maximum is reached, old files start to be deleted to make room for new ones.")
                    .build(),
                glib::ParamSpecUInt::builder("target-duration")
                    .nick("Target duration")
                    .blurb("The target duration in seconds of a segment/file. (0 - disabled, useful for management of segment duration by the streaming server)")
                    .default_value(DEFAULT_TARGET_DURATION)
                    .build(),
                glib::ParamSpecUInt::builder("playlist-length")
                    .nick("Playlist length")
                    .blurb("Length of HLS playlist. To allow players to conform to section 6.3.3 of the HLS specification, this should be at least 3. If set to 0, the playlist will be infinite.")
                    .default_value(DEFAULT_PLAYLIST_LENGTH)
                    .build(),
                glib::ParamSpecEnum::builder_with_default("playlist-type", DEFAULT_PLAYLIST_TYPE)
                    .nick("Playlist Type")
                    .blurb("The type of the playlist to use. When VOD type is set, the playlist will be live until the pipeline ends execution.")
                    .build(),
                glib::ParamSpecBoolean::builder("i-frames-only")
                    .nick("I-Frames only playlist")
                    .blurb("Each video segments is single iframe, So put EXT-X-I-FRAMES-ONLY tag in the playlist")
                    .default_value(DEFAULT_I_FRAMES_ONLY_PLAYLIST)
                    .build(),
                glib::ParamSpecBoolean::builder("enable-program-date-time")
                    .nick("add EXT-X-PROGRAM-DATE-TIME tag")
                    .blurb("put EXT-X-PROGRAM-DATE-TIME tag in the playlist")
                    .default_value(DEFAULT_PROGRAM_DATE_TIME_TAG)
                    .build(),
                glib::ParamSpecBoolean::builder("pdt-follows-pipeline-clock")
                    .nick("Whether Program-Date-Time should follow the pipeline clock")
                    .blurb("As there might be drift between the wallclock and pipeline clock, this controls whether the Program-Date-Time markers should follow the pipeline clock rate (true), or be skewed to match the wallclock rate (false).")
                    .default_value(DEFAULT_CLOCK_TRACKING_FOR_PDT)
                    .build(),
                glib::ParamSpecBoolean::builder("send-keyframe-requests")
                    .nick("Send Keyframe Requests")
                    .blurb("Send keyframe requests to ensure correct fragmentation. If this is disabled then the input must have keyframes in regular intervals.")
                    .default_value(DEFAULT_SEND_KEYFRAME_REQUESTS)
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
                settings
                    .splitmuxsink
                    .set_property("location", &settings.location);
            }
            "playlist-location" => {
                settings.playlist_location = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| String::from(DEFAULT_PLAYLIST_LOCATION));
            }
            "playlist-root" => {
                settings.playlist_root = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
            }
            "max-files" => {
                let max_files: u32 = value.get().expect("type checked upstream");
                settings.max_num_segment_files = max_files as usize;
            }
            "target-duration" => {
                settings.target_duration = value.get().expect("type checked upstream");
                settings
                    .splitmuxsink
                    .set_property("max-size-time", (settings.target_duration as u64).seconds());
            }
            "playlist-length" => {
                settings.playlist_length = value.get().expect("type checked upstream");
            }
            "playlist-type" => {
                settings.playlist_type = value
                    .get::<HlsSink3PlaylistType>()
                    .expect("type checked upstream")
                    .into();
            }
            "i-frames-only" => {
                settings.i_frames_only = value.get().expect("type checked upstream");
                if settings.i_frames_only && settings.audio_sink {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::WrongType,
                        ("Invalid configuration"),
                        ["Audio not allowed for i-frames-only-stream"]
                    );
                }
            }
            "enable-program-date-time" => {
                settings.enable_program_date_time = value.get().expect("type checked upstream");
            }
            "pdt-follows-pipeline-clock" => {
                settings.pdt_follows_pipeline_clock = value.get().expect("type checked upstream");
            }
            "send-keyframe-requests" => {
                settings.send_keyframe_requests = value.get().expect("type checked upstream");
                settings
                    .splitmuxsink
                    .set_property("send-keyframe-requests", settings.send_keyframe_requests);
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "location" => settings.location.to_value(),
            "playlist-location" => settings.playlist_location.to_value(),
            "playlist-root" => settings.playlist_root.to_value(),
            "max-files" => {
                let max_files = settings.max_num_segment_files as u32;
                max_files.to_value()
            }
            "target-duration" => settings.target_duration.to_value(),
            "playlist-length" => settings.playlist_length.to_value(),
            "playlist-type" => {
                let playlist_type: HlsSink3PlaylistType = settings.playlist_type.as_ref().into();
                playlist_type.to_value()
            }
            "i-frames-only" => settings.i_frames_only.to_value(),
            "enable-program-date-time" => settings.enable_program_date_time.to_value(),
            "pdt-follows-pipeline-clock" => settings.pdt_follows_pipeline_clock.to_value(),
            "send-keyframe-requests" => settings.send_keyframe_requests.to_value(),
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![
                glib::subclass::Signal::builder(SIGNAL_GET_PLAYLIST_STREAM)
                    .param_types([String::static_type()])
                    .return_type::<Option<gio::OutputStream>>()
                    .class_handler(|_, args| {
                        let element = args[0]
                            .get::<super::HlsSink3>()
                            .expect("playlist-stream signal arg");
                        let playlist_location =
                            args[1].get::<String>().expect("playlist-stream signal arg");
                        let hlssink3 = element.imp();

                        Some(hlssink3.new_file_stream(&playlist_location).ok().to_value())
                    })
                    .accumulator(|_hint, ret, value| {
                        // First signal handler wins
                        *ret = value.clone();
                        false
                    })
                    .build(),
                glib::subclass::Signal::builder(SIGNAL_GET_FRAGMENT_STREAM)
                    .param_types([String::static_type()])
                    .return_type::<Option<gio::OutputStream>>()
                    .class_handler(|_, args| {
                        let element = args[0]
                            .get::<super::HlsSink3>()
                            .expect("fragment-stream signal arg");
                        let fragment_location =
                            args[1].get::<String>().expect("fragment-stream signal arg");
                        let hlssink3 = element.imp();

                        Some(hlssink3.new_file_stream(&fragment_location).ok().to_value())
                    })
                    .accumulator(|_hint, ret, value| {
                        // First signal handler wins
                        *ret = value.clone();
                        false
                    })
                    .build(),
                glib::subclass::Signal::builder(SIGNAL_DELETE_FRAGMENT)
                    .param_types([String::static_type()])
                    .return_type::<bool>()
                    .class_handler(|_, args| {
                        let element = args[0].get::<super::HlsSink3>().expect("signal arg");
                        let fragment_location = args[1].get::<String>().expect("signal arg");
                        let hlssink3 = element.imp();

                        hlssink3.delete_fragment(&fragment_location);
                        Some(true.to_value())
                    })
                    .accumulator(|_hint, ret, value| {
                        // First signal handler wins
                        *ret = value.clone();
                        false
                    })
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.set_element_flags(gst::ElementFlags::SINK);
        obj.set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);

        let settings = self.settings.lock().unwrap();

        let mux = gst::ElementFactory::make("mpegtsmux")
            .name("mpeg-ts_mux")
            .build()
            .expect("Could not make element mpegtsmux");

        let location: Option<String> = None;
        settings.splitmuxsink.set_properties(&[
            ("location", &location),
            (
                "max-size-time",
                &(settings.target_duration as u64).seconds(),
            ),
            ("send-keyframe-requests", &settings.send_keyframe_requests),
            ("muxer", &mux),
            ("sink", &settings.giostreamsink),
            ("reset-muxer", &false),
        ]);

        obj.add(&settings.splitmuxsink).unwrap();
        settings
            .splitmuxsink
            .connect("format-location-full", false, {
                let imp_weak = self.downgrade();
                move |args| {
                    let imp = match imp_weak.upgrade() {
                        Some(imp) => imp,
                        None => return Some(None::<String>.to_value()),
                    };
                    let fragment_id = args[1].get::<u32>().unwrap();
                    gst::info!(CAT, imp: imp, "Got fragment-id: {}", fragment_id);

                    let mut state = imp.state.lock().unwrap();
                    let context = match state.context.as_mut() {
                        Some(context) => context,
                        None => {
                            gst::error!(
                                CAT,
                                imp: imp,
                                "on format location called with Stopped state"
                            );
                            return Some("unknown_segment".to_value());
                        }
                    };

                    let sample = args[2].get::<gst::Sample>().unwrap();
                    let buffer = sample.buffer();
                    if let Some(buffer) = buffer {
                        let segment = sample
                            .segment()
                            .expect("segment not available")
                            .downcast_ref::<gst::ClockTime>()
                            .expect("no time segment");
                        context.fragment_running_time =
                            segment.to_running_time(buffer.pts().unwrap());
                    } else {
                        gst::warning!(
                            CAT,
                            imp: imp,
                            "buffer null for fragment-id: {}",
                            fragment_id
                        );
                    }
                    drop(state);

                    match imp.on_format_location(fragment_id) {
                        Ok(segment_location) => Some(segment_location.to_value()),
                        Err(err) => {
                            gst::error!(CAT, imp: imp, "on format-location handler: {}", err);
                            Some("unknown_segment".to_value())
                        }
                    }
                }
            });
    }
}

impl GstObjectImpl for HlsSink3 {}

impl ElementImpl for HlsSink3 {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "HTTP Live Streaming sink",
                "Sink/Muxer",
                "HTTP Live Streaming sink",
                "Alessandro Decina <alessandro.d@gmail.com>, \
                Sebastian Dröge <sebastian@centricular.com>, \
                Rafael Caricio <rafael@caricio.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let video_pad_template = gst::PadTemplate::new(
                "video",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
            )
            .unwrap();

            let caps = gst::Caps::new_any();
            let audio_pad_template = gst::PadTemplate::new(
                "audio",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps,
            )
            .unwrap();

            vec![video_pad_template, audio_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::ReadyToPaused {
            self.start();
        }

        let ret = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::PlayingToPaused => {
                let mut state = self.state.lock().unwrap();
                if let Some(context) = state.context.as_mut() {
                    // reset mapping from rt to utc. during pause
                    // rt is stopped but utc keep moving so need to
                    // calculate the mapping again
                    context.pdt_base_running_time = None;
                    context.pdt_base_utc = None
                }
            }
            gst::StateChange::PausedToReady => {
                self.stop();
            }
            _ => (),
        }

        Ok(ret)
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        _name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let mut settings = self.settings.lock().unwrap();
        match templ.name_template() {
            "audio" => {
                if settings.audio_sink {
                    gst::debug!(
                        CAT,
                        imp: self,
                        "requested_new_pad: audio pad is already set"
                    );
                    return None;
                }
                if settings.i_frames_only {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::WrongType,
                        ("Invalid configuration"),
                        ["Audio not allowed for i-frames-only-stream"]
                    );
                    return None;
                }

                let peer_pad = settings.splitmuxsink.request_pad_simple("audio_0").unwrap();
                let sink_pad = gst::GhostPad::from_template_with_target(templ, &peer_pad).unwrap();
                self.obj().add_pad(&sink_pad).unwrap();
                sink_pad.set_active(true).unwrap();
                settings.audio_sink = true;

                Some(sink_pad.upcast())
            }
            "video" => {
                if settings.video_sink {
                    gst::debug!(
                        CAT,
                        imp: self,
                        "requested_new_pad: video pad is already set"
                    );
                    return None;
                }
                let peer_pad = settings.splitmuxsink.request_pad_simple("video").unwrap();

                let sink_pad = gst::GhostPad::from_template_with_target(templ, &peer_pad).unwrap();
                self.obj().add_pad(&sink_pad).unwrap();
                sink_pad.set_active(true).unwrap();
                settings.video_sink = true;

                Some(sink_pad.upcast())
            }
            other_name => {
                gst::debug!(
                    CAT,
                    imp: self,
                    "requested_new_pad: name \"{}\" is not audio or video",
                    other_name
                );
                None
            }
        }
    }

    fn release_pad(&self, pad: &gst::Pad) {
        let mut settings = self.settings.lock().unwrap();

        if !settings.audio_sink && !settings.video_sink {
            return;
        }

        let ghost_pad = pad.downcast_ref::<gst::GhostPad>().unwrap();
        if let Some(peer) = ghost_pad.target() {
            settings.splitmuxsink.release_request_pad(&peer);
        }

        pad.set_active(false).unwrap();
        self.obj().remove_pad(pad).unwrap();

        if "audio" == ghost_pad.name() {
            settings.audio_sink = false;
        } else {
            settings.video_sink = false;
        }
    }
}
