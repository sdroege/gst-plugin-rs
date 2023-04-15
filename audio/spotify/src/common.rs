// Copyright (C) 2021 Guillaume Desmottes <guillaume@desmottes.be>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use anyhow::bail;

use gst::glib;
use gst::prelude::*;

use librespot::core::{
    cache::Cache, config::SessionConfig, session::Session, spotify_id::SpotifyId,
};
use librespot::discovery::Credentials;

#[derive(Default, Debug, Clone)]
pub struct Settings {
    username: String,
    password: String,
    cache_credentials: String,
    cache_files: String,
    cache_max_size: u64,
    pub track: String,
}

impl Settings {
    pub fn properties() -> Vec<glib::ParamSpec> {
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
    }

    pub fn set_property(&mut self, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "username" => {
                self.username = value.get().expect("type checked upstream");
            }
            "password" => {
                self.password = value.get().expect("type checked upstream");
            }
            "cache-credentials" => {
                self.cache_credentials = value.get().expect("type checked upstream");
            }
            "cache-files" => {
                self.cache_files = value.get().expect("type checked upstream");
            }
            "cache-max-size" => {
                self.cache_max_size = value.get().expect("type checked upstream");
            }
            "track" => {
                self.track = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    pub fn property(&self, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "username" => self.username.to_value(),
            "password" => self.password.to_value(),
            "cache-credentials" => self.cache_credentials.to_value(),
            "cache-files" => self.cache_files.to_value(),
            "cache-max-size" => self.cache_max_size.to_value(),
            "track" => self.track.to_value(),
            _ => unimplemented!(),
        }
    }

    pub async fn connect_session<T>(
        &self,
        src: T,
        cat: &gst::DebugCategory,
    ) -> anyhow::Result<Session>
    where
        T: glib::IsA<glib::Object>,
    {
        let credentials_cache = if self.cache_credentials.is_empty() {
            None
        } else {
            Some(&self.cache_credentials)
        };

        let files_cache = if self.cache_files.is_empty() {
            None
        } else {
            Some(&self.cache_files)
        };

        let max_size = if self.cache_max_size != 0 {
            Some(self.cache_max_size)
        } else {
            None
        };

        let cache = Cache::new(credentials_cache, None, files_cache, max_size)?;

        if let Some(cached_cred) = cache.credentials() {
            gst::debug!(cat, obj: &src, "reuse credentials from cache",);
            if let Ok((session, _credentials)) = Session::connect(
                SessionConfig::default(),
                cached_cred,
                Some(cache.clone()),
                true,
            )
            .await
            {
                return Ok(session);
            }
        }

        gst::debug!(
            cat,
            obj: &src,
            "credentials not in cache or cached credentials invalid",
        );

        if self.username.is_empty() {
            bail!("username is not set and credentials are not in cache");
        }
        if self.password.is_empty() {
            bail!("password is not set and credentials are not in cache");
        }

        let cred = Credentials::with_password(&self.username, &self.password);

        let (session, _credentials) =
            Session::connect(SessionConfig::default(), cred, Some(cache), true).await?;

        Ok(session)
    }

    pub fn track_id(&self) -> anyhow::Result<SpotifyId> {
        if self.track.is_empty() {
            bail!("track is not set");
        }
        let track = SpotifyId::from_uri(&self.track).map_err(|_| {
            anyhow::anyhow!("failed to create Spotify URI from track {}", self.track)
        })?;

        Ok(track)
    }
}
