// Copyright (C) 2021 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use m3u8_rs::{MediaPlaylist, MediaPlaylistType, MediaSegment};
use std::io::Write;

const GST_M3U8_PLAYLIST_V3: usize = 3;
const GST_M3U8_PLAYLIST_V4: usize = 4;

/// An HLS playlist.
///
/// Controls the changes that needs to happen in the playlist as new segments are added. This
/// includes maintaining the number of segments present in the playlist and setting the final
/// state of the playlist.
#[derive(Debug, Clone)]
pub struct Playlist {
    inner: MediaPlaylist,
    playlist_index: u64,
    status: PlaylistRenderState,
    turn_vod: bool,
}

impl Playlist {
    pub fn new(
        target_duration: f32,
        playlist_type: Option<MediaPlaylistType>,
        i_frames_only: bool,
    ) -> Self {
        let mut turn_vod = false;
        let playlist_type = if playlist_type == Some(MediaPlaylistType::Vod) {
            turn_vod = true;
            Some(MediaPlaylistType::Event)
        } else {
            playlist_type
        };
        let m3u8_version = if i_frames_only {
            GST_M3U8_PLAYLIST_V4
        } else {
            GST_M3U8_PLAYLIST_V3
        };
        Self {
            inner: MediaPlaylist {
                version: Some(m3u8_version),
                target_duration,
                media_sequence: 0,
                segments: vec![],
                discontinuity_sequence: 0,
                end_list: false,
                playlist_type,
                i_frames_only,
                start: None,
                independent_segments: false,
                unknown_tags: vec![],
            },
            playlist_index: 0,
            status: PlaylistRenderState::Init,
            turn_vod,
        }
    }

    /// Adds a new segment to the playlist.
    pub fn add_segment(&mut self, uri: String, duration: f32) {
        self.start();
        self.inner.segments.push(MediaSegment {
            uri,
            duration,
            title: None,
            byte_range: None,
            discontinuity: false,
            key: None,
            map: None,
            program_date_time: None,
            daterange: None,
            unknown_tags: vec![],
        });
    }

    /// Updates the playlist based on current state.
    ///
    /// The playlist will be updated based on it's type. The playlist status is set to started.
    /// When a playlist type is defined, the number of segments is updated to match the max
    /// playlist length value. The playlist index and current media sequence is also kept up
    /// to date.
    pub fn update_playlist_state(&mut self, max_playlist_length: usize) {
        if !self.is_type_undefined() {
            return;
        }

        // Remove oldest segments if playlist is at maximum expected capacity
        if self.inner.segments.len() > max_playlist_length {
            for _ in 0..self.inner.segments.len() - max_playlist_length {
                let _ = self.inner.segments.remove(0);
            }
        }

        self.playlist_index += 1;
        self.inner.media_sequence = self.playlist_index - self.inner.segments.len() as u64;
    }

    /// Sets the playlist to started state.
    fn start(&mut self) {
        self.status = PlaylistRenderState::Started;
        self.inner.end_list = false;
    }

    /// Sets the playlist to stopped state.
    pub fn stop(&mut self) {
        match &self.inner.playlist_type {
            None => self.inner.end_list = false,
            Some(defined) => match defined {
                MediaPlaylistType::Other(_) => unreachable!(),
                MediaPlaylistType::Event => {
                    if self.turn_vod {
                        self.inner.playlist_type = Some(MediaPlaylistType::Vod);
                    }
                    self.inner.end_list = true
                }
                MediaPlaylistType::Vod => self.inner.end_list = false,
            },
        }
    }

    /// Returns true if the playlist type is not specified.
    pub fn is_type_undefined(&self) -> bool {
        self.inner.playlist_type.is_none()
    }

    /// Returns true if the playlist internal status started.
    pub fn is_rendering(&self) -> bool {
        self.status == PlaylistRenderState::Started
    }

    /// Returns the number of segments in the playlist.
    pub fn len(&self) -> usize {
        self.inner.segments.len()
    }

    /// Writes the playlist in textual format to the provided `Write` reference.
    pub fn write_to<T: Write>(&self, w: &mut T) -> std::io::Result<()> {
        self.inner.write_to(w)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PlaylistRenderState {
    Init,
    Started,
}
