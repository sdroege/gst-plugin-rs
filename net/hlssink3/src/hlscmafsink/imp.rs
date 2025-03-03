// Copyright (C) 2023 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::hlsbasesink::HlsBaseSinkImpl;
use crate::hlssink3::HlsSink3PlaylistType;
use crate::playlist::Playlist;
use crate::HlsBaseSink;
use gio::prelude::*;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use m3u8_rs::{MediaPlaylist, MediaPlaylistType, MediaSegment};
use once_cell::sync::Lazy;
use std::io::Write;
use std::sync::Mutex;

const DEFAULT_INIT_LOCATION: &str = "init%05d.mp4";
const DEFAULT_CMAF_LOCATION: &str = "segment%05d.m4s";
const DEFAULT_TARGET_DURATION: u32 = 15;
const DEFAULT_PLAYLIST_TYPE: HlsSink3PlaylistType = HlsSink3PlaylistType::Unspecified;
const DEFAULT_SYNC: bool = true;
const DEFAULT_LATENCY: gst::ClockTime =
    gst::ClockTime::from_mseconds((DEFAULT_TARGET_DURATION * 500) as u64);
const SIGNAL_GET_INIT_STREAM: &str = "get-init-stream";

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "hlscmafsink",
        gst::DebugColorFlags::empty(),
        Some("HLS CMAF sink"),
    )
});

macro_rules! base_imp {
    ($i:expr) => {
        $i.obj().upcast_ref::<HlsBaseSink>().imp()
    };
}

struct HlsCmafSinkSettings {
    init_location: String,
    location: String,
    target_duration: u32,
    playlist_type: Option<MediaPlaylistType>,
    sync: bool,
    latency: gst::ClockTime,

    cmafmux: gst::Element,
    appsink: gst_app::AppSink,
}

impl Default for HlsCmafSinkSettings {
    fn default() -> Self {
        let cmafmux = gst::ElementFactory::make("cmafmux")
            .name("muxer")
            .property(
                "fragment-duration",
                gst::ClockTime::from_seconds(DEFAULT_TARGET_DURATION as u64),
            )
            .property("latency", DEFAULT_LATENCY)
            .build()
            .expect("Could not make element cmafmux");
        let appsink = gst_app::AppSink::builder()
            .buffer_list(true)
            .sync(DEFAULT_SYNC)
            .name("sink")
            .build();

        Self {
            init_location: String::from(DEFAULT_INIT_LOCATION),
            location: String::from(DEFAULT_CMAF_LOCATION),
            target_duration: DEFAULT_TARGET_DURATION,
            playlist_type: None,
            sync: DEFAULT_SYNC,
            latency: DEFAULT_LATENCY,
            cmafmux,
            appsink,
        }
    }
}

#[derive(Default)]
struct HlsCmafSinkState {
    init_idx: u32,
    segment_idx: u32,
    init_segment: Option<m3u8_rs::Map>,
    new_header: bool,
}

#[derive(Default)]
pub struct HlsCmafSink {
    settings: Mutex<HlsCmafSinkSettings>,
    state: Mutex<HlsCmafSinkState>,
}

#[glib::object_subclass]
impl ObjectSubclass for HlsCmafSink {
    const NAME: &'static str = "GstHlsCmafSink";
    type Type = super::HlsCmafSink;
    type ParentType = HlsBaseSink;
}

impl ObjectImpl for HlsCmafSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("init-location")
                    .nick("Init Location")
                    .blurb("Location of the init fragment file to write")
                    .default_value(Some(DEFAULT_INIT_LOCATION))
                    .build(),
                glib::ParamSpecString::builder("location")
                    .nick("Location")
                    .blurb("Location of the fragment file to write")
                    .default_value(Some(DEFAULT_CMAF_LOCATION))
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
                glib::ParamSpecUInt64::builder("latency")
                    .nick("Latency")
                    .blurb(
                        "Additional latency to allow upstream to take longer to \
                         produce buffers for the current position (in nanoseconds)",
                    )
                    .maximum(i64::MAX as u64)
                    .default_value(DEFAULT_LATENCY.nseconds())
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "init-location" => {
                settings.init_location = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| DEFAULT_INIT_LOCATION.into());
            }
            "location" => {
                settings.location = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| DEFAULT_CMAF_LOCATION.into());
            }
            "target-duration" => {
                settings.target_duration = value.get().expect("type checked upstream");
                settings.cmafmux.set_property(
                    "fragment-duration",
                    gst::ClockTime::from_seconds(settings.target_duration as u64),
                );
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
            "latency" => {
                settings.latency = value.get().expect("type checked upstream");
                settings.cmafmux.set_property("latency", settings.latency);
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "init-location" => settings.init_location.to_value(),
            "location" => settings.location.to_value(),
            "target-duration" => settings.target_duration.to_value(),
            "playlist-type" => {
                let playlist_type: HlsSink3PlaylistType = settings.playlist_type.as_ref().into();
                playlist_type.to_value()
            }
            "sync" => settings.sync.to_value(),
            "latency" => settings.latency.to_value(),
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![glib::subclass::Signal::builder(SIGNAL_GET_INIT_STREAM)
                .param_types([String::static_type()])
                .return_type::<Option<gio::OutputStream>>()
                .class_handler(|_, args| {
                    let elem = args[0].get::<HlsBaseSink>().expect("signal arg");
                    let init_location = args[1].get::<String>().expect("signal arg");
                    let imp = elem.imp();

                    Some(imp.new_file_stream(&init_location).ok().to_value())
                })
                .accumulator(|_hint, ret, value| {
                    // First signal handler wins
                    *ret = value.clone();
                    false
                })
                .build()]
        });

        SIGNALS.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        let settings = self.settings.lock().unwrap();

        obj.add_many([&settings.cmafmux, settings.appsink.upcast_ref()])
            .unwrap();
        settings.cmafmux.link(&settings.appsink).unwrap();

        let sinkpad = settings.cmafmux.static_pad("sink").unwrap();
        let gpad = gst::GhostPad::with_target(&sinkpad).unwrap();

        obj.add_pad(&gpad).unwrap();

        let self_weak = self.downgrade();
        settings.appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let Some(imp) = self_weak.upgrade() else {
                        return Err(gst::FlowError::Eos);
                    };

                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    imp.on_new_sample(sample)
                })
                .build(),
        );
    }
}

impl GstObjectImpl for HlsCmafSink {}

impl ElementImpl for HlsCmafSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "HTTP Live Streaming CMAF Sink",
                "Sink/Muxer",
                "HTTP Live Streaming CMAF Sink",
                "Seungha Yang <seungha@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
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
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "raw")
                        .field("channels", gst::IntRange::new(1, u16::MAX as i32))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                ]
                .into_iter()
                .collect::<gst::Caps>(),
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
        if transition == gst::StateChange::ReadyToPaused {
            let (target_duration, playlist_type, segment_template) = {
                let settings = self.settings.lock().unwrap();
                (
                    settings.target_duration,
                    settings.playlist_type.clone(),
                    settings.location.clone(),
                )
            };

            let playlist = self.start(target_duration, playlist_type);
            base_imp!(self).open_playlist(playlist, segment_template);
        }

        self.parent_change_state(transition)
    }
}

impl BinImpl for HlsCmafSink {}

impl HlsBaseSinkImpl for HlsCmafSink {}

impl HlsCmafSink {
    fn start(&self, target_duration: u32, playlist_type: Option<MediaPlaylistType>) -> Playlist {
        gst::info!(CAT, imp = self, "Starting");

        let mut state = self.state.lock().unwrap();
        *state = HlsCmafSinkState::default();

        let (turn_vod, playlist_type) = if playlist_type == Some(MediaPlaylistType::Vod) {
            (true, Some(MediaPlaylistType::Event))
        } else {
            (false, playlist_type)
        };

        let playlist = MediaPlaylist {
            version: Some(6),
            target_duration: target_duration as u64,
            playlist_type,
            independent_segments: true,
            ..Default::default()
        };

        Playlist::new(playlist, turn_vod, true)
    }

    fn on_init_segment(&self) -> Result<gio::OutputStreamWrite<gio::OutputStream>, String> {
        let settings = self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        let location = match sprintf::sprintf!(&settings.init_location, state.init_idx) {
            Ok(location) => location,
            Err(err) => {
                gst::error!(CAT, imp = self, "Couldn't build file name, err: {:?}", err,);
                return Err(String::from("Invalid init segment file pattern"));
            }
        };

        let stream = self
            .obj()
            .emit_by_name::<Option<gio::OutputStream>>(SIGNAL_GET_INIT_STREAM, &[&location])
            .ok_or_else(|| String::from("Error while getting fragment stream"))?
            .into_write();

        let uri = base_imp!(self).get_segment_uri(&location);

        state.init_segment = Some(m3u8_rs::Map {
            uri,
            ..Default::default()
        });
        state.new_header = true;
        state.init_idx += 1;

        Ok(stream)
    }

    fn on_new_fragment(
        &self,
    ) -> Result<(gio::OutputStreamWrite<gio::OutputStream>, String), String> {
        let mut state = self.state.lock().unwrap();
        let (stream, location) = base_imp!(self)
            .get_fragment_stream(state.segment_idx)
            .ok_or_else(|| String::from("Error while getting fragment stream"))?;

        state.segment_idx += 1;

        Ok((stream.into_write(), location))
    }

    fn add_segment(
        &self,
        duration: f32,
        running_time: Option<gst::ClockTime>,
        location: String,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let uri = base_imp!(self).get_segment_uri(&location);
        let mut state = self.state.lock().unwrap();

        let map = if state.new_header {
            state.new_header = false;
            state.init_segment.clone()
        } else {
            None
        };

        base_imp!(self).add_segment(
            &location,
            running_time,
            MediaSegment {
                uri,
                duration,
                map,
                ..Default::default()
            },
        )
    }

    fn on_new_sample(&self, sample: gst::Sample) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut buffer_list = sample.buffer_list_owned().unwrap();
        let mut first = buffer_list.get(0).unwrap();

        if first
            .flags()
            .contains(gst::BufferFlags::DISCONT | gst::BufferFlags::HEADER)
        {
            let mut stream = self.on_init_segment().map_err(|err| {
                gst::error!(
                    CAT,
                    imp = self,
                    "Couldn't get output stream for init segment, {err}",
                );
                gst::FlowError::Error
            })?;

            let map = first.map_readable().unwrap();
            stream.write(&map).map_err(|_| {
                gst::error!(
                    CAT,
                    imp = self,
                    "Couldn't write init segment to output stream",
                );
                gst::FlowError::Error
            })?;

            stream.flush().map_err(|_| {
                gst::error!(CAT, imp = self, "Couldn't flush output stream",);
                gst::FlowError::Error
            })?;

            drop(map);

            buffer_list.make_mut().remove(0..1);
            if buffer_list.is_empty() {
                return Ok(gst::FlowSuccess::Ok);
            }

            first = buffer_list.get(0).unwrap();
        }

        let segment = sample
            .segment()
            .unwrap()
            .downcast_ref::<gst::ClockTime>()
            .unwrap();
        let running_time = segment.to_running_time(first.pts().unwrap());
        let dur = first.duration().unwrap();

        let (mut stream, location) = self.on_new_fragment().map_err(|err| {
            gst::error!(
                CAT,
                imp = self,
                "Couldn't get output stream for segment, {err}",
            );
            gst::FlowError::Error
        })?;

        for buffer in &*buffer_list {
            let map = buffer.map_readable().unwrap();

            stream.write(&map).map_err(|_| {
                gst::error!(CAT, imp = self, "Couldn't write segment to output stream",);
                gst::FlowError::Error
            })?;
        }

        stream.flush().map_err(|_| {
            gst::error!(CAT, imp = self, "Couldn't flush output stream",);
            gst::FlowError::Error
        })?;

        self.add_segment(dur.mseconds() as f32 / 1_000f32, running_time, location)
    }
}
