// Copyright (C) 2021 Guillaume Desmottes <guillaume@desmottes.be>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::sync::{mpsc, Arc, Mutex};

use anyhow::bail;
use once_cell::sync::Lazy;
use tokio::{runtime, task::JoinHandle};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::{base_src::CreateSuccess, prelude::*};

use librespot::core::{
    cache::Cache, config::SessionConfig, session::Session, spotify_id::SpotifyId,
};
use librespot::discovery::Credentials;
use librespot::playback::{
    audio_backend::{Sink, SinkResult},
    config::PlayerConfig,
    convert::Converter,
    decoder::AudioPacket,
    mixer::NoOpVolume,
    player::{Player, PlayerEvent},
};

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "spotifyaudiosrc",
        gst::DebugColorFlags::empty(),
        Some("Spotify audio source"),
    )
});

static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

/// Messages from the librespot thread
enum Message {
    Buffer(gst::Buffer),
    Eos,
    Unavailable,
}

struct State {
    player: Player,

    /// receiver sending buffer to streaming thread
    receiver: mpsc::Receiver<Message>,
    /// thread receiving player events from librespot
    player_channel_handle: JoinHandle<()>,
}

#[derive(Default)]
struct Settings {
    username: String,
    password: String,
    cache_credentials: String,
    cache_files: String,
    cache_max_size: u64,
    track: String,
}

#[derive(Default)]
pub struct SpotifyAudioSrc {
    state: Arc<Mutex<Option<State>>>,
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for SpotifyAudioSrc {
    const NAME: &'static str = "GstSpotifyAudioSrc";
    type Type = super::SpotifyAudioSrc;
    type ParentType = gst_base::BaseSrc;
    type Interfaces = (gst::URIHandler,);
}

impl ObjectImpl for SpotifyAudioSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpecString::builder("username")
                    .nick("Username")
                    .blurb("Spotify device username from https://www.spotify.com/us/account/set-device-password/")
                    .default_value(Some(""))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("password")
                    .nick("Password")
                    .blurb("Spotify device password from https://www.spotify.com/us/account/set-device-password/")
                    .default_value(Some(""))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("cache-credentials")
                    .nick("Credentials cache")
                    .blurb("Directory where to cache Spotify credentials")
                    .default_value(Some(""))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("cache-files")
                    .nick("Files cache")
                    .blurb("Directory where to cache downloaded files from Spotify")
                    .default_value(Some(""))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("cache-max-size")
                    .nick("Cache max size")
                    .blurb("The max allowed size of the cache, in bytes, or 0 to disable the cache limit")
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("track")
                    .nick("Spotify URI")
                    .blurb("Spotify track URI, in the form 'spotify:track:$SPOTIFY_ID'")
                    .default_value(Some(""))
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "username" => {
                let mut settings = self.settings.lock().unwrap();
                settings.username = value.get().expect("type checked upstream");
            }
            "password" => {
                let mut settings = self.settings.lock().unwrap();
                settings.password = value.get().expect("type checked upstream");
            }
            "cache-credentials" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cache_credentials = value.get().expect("type checked upstream");
            }
            "cache-files" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cache_files = value.get().expect("type checked upstream");
            }
            "cache-max-size" => {
                let mut settings = self.settings.lock().unwrap();
                settings.cache_max_size = value.get().expect("type checked upstream");
            }
            "track" => {
                let mut settings = self.settings.lock().unwrap();
                settings.track = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "username" => {
                let settings = self.settings.lock().unwrap();
                settings.username.to_value()
            }
            "password" => {
                let settings = self.settings.lock().unwrap();
                settings.password.to_value()
            }
            "cache-credentials" => {
                let settings = self.settings.lock().unwrap();
                settings.cache_credentials.to_value()
            }
            "cache-files" => {
                let settings = self.settings.lock().unwrap();
                settings.cache_files.to_value()
            }
            "cache-max-size" => {
                let settings = self.settings.lock().unwrap();
                settings.cache_max_size.to_value()
            }
            "track" => {
                let settings = self.settings.lock().unwrap();
                settings.track.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for SpotifyAudioSrc {}

impl ElementImpl for SpotifyAudioSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Spotify source",
                "Source/Audio",
                "Spotify source",
                "Guillaume Desmottes <guillaume@desmottes.be>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("application/ogg").build();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseSrcImpl for SpotifyAudioSrc {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        {
            let state = self.state.lock().unwrap();
            if state.is_some() {
                // already started
                return Ok(());
            }
        }

        if let Err(err) = RUNTIME.block_on(async move { self.setup().await }) {
            let details = format!("{:?}", err);
            gst::error!(CAT, imp: self, "failed to start: {}", details);
            gst::element_imp_error!(self, gst::ResourceError::Settings, [&details]);
            return Err(gst::error_msg!(gst::ResourceError::Settings, [&details]));
        }

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        if let Some(state) = self.state.lock().unwrap().take() {
            gst::debug!(CAT, imp: self, "stopping");
            state.player.stop();
            state.player_channel_handle.abort();
            // FIXME: not sure why this is needed to unblock BufferSink::write(), dropping State should drop the receiver
            drop(state.receiver);
        }

        Ok(())
    }

    fn create(
        &self,
        _offset: u64,
        _buffer: Option<&mut gst::BufferRef>,
        _length: u32,
    ) -> Result<CreateSuccess, gst::FlowError> {
        let state = self.state.lock().unwrap();
        let state = state.as_ref().unwrap();

        match state.receiver.recv().unwrap() {
            Message::Buffer(buffer) => {
                gst::log!(CAT, imp: self, "got buffer of size {}", buffer.size());
                Ok(CreateSuccess::NewBuffer(buffer))
            }
            Message::Eos => {
                gst::debug!(CAT, imp: self, "eos");
                Err(gst::FlowError::Eos)
            }
            Message::Unavailable => {
                gst::error!(CAT, imp: self, "track is not available");
                gst::element_imp_error!(
                    self,
                    gst::ResourceError::NotFound,
                    ["track is not available"]
                );
                Err(gst::FlowError::Error)
            }
        }
    }
}

impl SpotifyAudioSrc {
    async fn setup(&self) -> anyhow::Result<()> {
        let (credentials, cache, track) = {
            let settings = self.settings.lock().unwrap();

            let credentials_cache = if settings.cache_credentials.is_empty() {
                None
            } else {
                Some(&settings.cache_credentials)
            };

            let files_cache = if settings.cache_files.is_empty() {
                None
            } else {
                Some(&settings.cache_files)
            };

            let max_size = if settings.cache_max_size != 0 {
                Some(settings.cache_max_size)
            } else {
                None
            };

            let cache = Cache::new(credentials_cache, None, files_cache, max_size)?;

            let credentials = match cache.credentials() {
                Some(cached_cred) => {
                    gst::debug!(CAT, imp: self, "reuse credentials from cache",);
                    cached_cred
                }
                None => {
                    gst::debug!(CAT, imp: self, "credentials not in cache",);

                    if settings.username.is_empty() {
                        bail!("username is not set and credentials are not in cache");
                    }
                    if settings.password.is_empty() {
                        bail!("password is not set and credentials are not in cache");
                    }

                    let cred = Credentials::with_password(&settings.username, &settings.password);
                    cache.save_credentials(&cred);
                    cred
                }
            };

            if settings.track.is_empty() {
                bail!("track is not set")
            }

            (credentials, cache, settings.track.clone())
        };

        let state = self.state.clone();

        let (session, _credentials) =
            Session::connect(SessionConfig::default(), credentials, Some(cache), false).await?;

        let player_config = PlayerConfig {
            passthrough: true,
            ..Default::default()
        };

        // use a sync channel to prevent buffering the whole track inside the channel
        let (sender, receiver) = mpsc::sync_channel(2);
        let sender_clone = sender.clone();

        let (mut player, mut player_event_channel) =
            Player::new(player_config, session, Box::new(NoOpVolume), || {
                Box::new(BufferSink { sender })
            });

        let track = match SpotifyId::from_uri(&track) {
            Ok(track) => track,
            Err(_) => bail!("Failed to create Spotify URI from track"),
        };

        player.load(track, true, 0);

        let player_channel_handle = RUNTIME.spawn(async move {
            let sender = sender_clone;

            while let Some(event) = player_event_channel.recv().await {
                match event {
                    PlayerEvent::EndOfTrack { .. } => {
                        let _ = sender.send(Message::Eos);
                    }
                    PlayerEvent::Unavailable { .. } => {
                        let _ = sender.send(Message::Unavailable);
                    }
                    _ => {}
                }
            }
        });

        let mut state = state.lock().unwrap();
        state.replace(State {
            player,
            receiver,
            player_channel_handle,
        });

        Ok(())
    }
}

struct BufferSink {
    sender: mpsc::SyncSender<Message>,
}

impl Sink for BufferSink {
    fn write(&mut self, packet: AudioPacket, _converter: &mut Converter) -> SinkResult<()> {
        let oggdata = match packet {
            AudioPacket::OggData(data) => data,
            AudioPacket::Samples(_) => unimplemented!(),
        };
        let buffer = gst::Buffer::from_slice(oggdata);

        // ignore if sending fails as that means the source element is being shutdown
        let _ = self.sender.send(Message::Buffer(buffer));

        Ok(())
    }
}

impl URIHandlerImpl for SpotifyAudioSrc {
    const URI_TYPE: gst::URIType = gst::URIType::Src;

    fn protocols() -> &'static [&'static str] {
        &["spotify"]
    }

    fn uri(&self) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        if settings.track.is_empty() {
            None
        } else {
            Some(settings.track.clone())
        }
    }

    fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
        gst::debug!(CAT, imp: self, "set URI: {}", uri);

        let url = url::Url::parse(uri)
            .map_err(|e| glib::Error::new(gst::URIError::BadUri, &format!("{:?}", e)))?;

        // allow to configure auth and cache settings from the URI
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "username" | "password" | "cache-credentials" | "cache-files" => {
                    self.instance().set_property(&key, value.as_ref());
                }
                _ => {
                    gst::warning!(CAT, imp: self, "unsupported query: {}={}", key, value);
                }
            }
        }

        self.instance()
            .set_property("track", format!("{}:{}", url.scheme(), url.path()));

        Ok(())
    }
}
