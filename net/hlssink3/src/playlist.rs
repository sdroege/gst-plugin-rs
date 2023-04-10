// Copyright (C) 2021 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use chrono::{DateTime, Utc};
use gst::glib::once_cell::sync::Lazy;
use m3u8_rs::{MediaPlaylist, MediaPlaylistType, MediaSegment};
use regex::Regex;
use std::io::Write;

const GST_M3U8_PLAYLIST_V3: usize = 3;
const GST_M3U8_PLAYLIST_V4: usize = 4;

static SEGMENT_IDX_PATTERN: Lazy<regex::Regex> = Lazy::new(|| Regex::new(r"(%0(\d+)d)").unwrap());

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
    pub fn add_segment(&mut self, uri: String, duration: f32, date_time: Option<DateTime<Utc>>) {
        self.start();
        // TODO: We are adding date-time to each segment, hence during write all the segments have
        // program-date-time header.
        self.inner.segments.push(MediaSegment {
            uri,
            duration,
            title: None,
            byte_range: None,
            discontinuity: false,
            key: None,
            map: None,
            program_date_time: date_time.map(|d| d.into()),
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

/// A formatter for segment locations.
///
/// The formatting is based on a string that must contain the placeholder `%0Xd` where `X` is a
/// the number of zero prefixes you want to have in the segment name. The placeholder is only
/// replaced once in the string, other placements are not going to be processed.
///
/// # Examples
///
/// In this example we want to have segment files with the following names:
/// ```text
/// part001.ts
/// part002.ts
/// part003.ts
/// part004.ts
/// ```
/// Then we can use the segment pattern value as `"part%03d.ts"`:
///
/// ```rust,ignore
/// let formatter = SegmentFormatter::new("part%03d.ts").unwrap();
/// assert_eq!(formatter.segment(1), "part001.ts");
/// assert_eq!(formatter.segment(2), "part002.ts");
/// assert_eq!(formatter.segment(3), "part003.ts");
/// assert_eq!(formatter.segment(4), "part004.ts");
/// ```
pub struct SegmentFormatter {
    prefix: String,
    suffix: String,
    padding_len: u32,
}

impl SegmentFormatter {
    /// Processes the segment name containing a placeholder. It can be used
    /// repeatedly to format segment names.
    ///
    /// If an invalid placeholder is provided, then `None` is returned.
    pub fn new<S: AsRef<str>>(segment_pattern: S) -> Option<Self> {
        let segment_pattern = segment_pattern.as_ref();
        let caps = SEGMENT_IDX_PATTERN.captures(segment_pattern)?;
        let number_placement_match = caps.get(1)?;
        let zero_pad_match = caps.get(2)?;
        let padding_len = zero_pad_match
            .as_str()
            .parse::<u32>()
            .expect("valid number matched by regex");
        let prefix = segment_pattern[..number_placement_match.start()].to_string();
        let suffix = segment_pattern[number_placement_match.end()..].to_string();
        Some(Self {
            prefix,
            suffix,
            padding_len,
        })
    }

    /// Returns the segment location formatted for the provided id.
    #[inline]
    pub fn segment(&self, id: u32) -> String {
        let padded_number = left_pad_zeroes(self.padding_len, id);
        format!("{}{}{}", self.prefix, padded_number, self.suffix)
    }
}

/// Transforms a number to a zero padded string representation.
///
/// The zero padding is added to the left of the number which is converted to a string. For the
/// case that the length number converted to string is larger than the requested padding, the
/// number representation is returned and no padding is added. The length of the returned string is
/// the maximum value between the desired padding and the length of the number.
///
/// # Examples
///
/// ```rust,ignore
/// let padded_number = left_pad_zeroes(4, 10);
/// assert_eq!(padded_number, "0010");
/// ```
#[inline]
pub(crate) fn left_pad_zeroes(padding: u32, number: u32) -> String {
    let numerical_repr = number.to_string();
    let mut padded = String::with_capacity(padding.max(numerical_repr.len() as u32) as usize);
    let pad_zeroes = padding as i32 - numerical_repr.len() as i32;
    if pad_zeroes > 0 {
        for _ in 0..pad_zeroes {
            padded.push('0');
        }
    }
    padded.push_str(&numerical_repr);
    padded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_is_correctly_formatted() {
        let formatter = SegmentFormatter::new("segment%05d.ts").unwrap();

        assert_eq!("segment00001.ts", formatter.segment(1));
        assert_eq!("segment00016.ts", formatter.segment(16));
        assert_eq!("segment01827.ts", formatter.segment(1827));
        assert_eq!("segment98765.ts", formatter.segment(98765));

        let formatter = SegmentFormatter::new("part-%03d.ts").unwrap();
        assert_eq!("part-010.ts", formatter.segment(10));
        assert_eq!("part-9999.ts", formatter.segment(9999));
    }

    #[test]
    fn padding_numbers() {
        assert_eq!("001", left_pad_zeroes(3, 1));
        assert_eq!("010", left_pad_zeroes(3, 10));
        assert_eq!("100", left_pad_zeroes(3, 100));
        assert_eq!("1000", left_pad_zeroes(3, 1000));
        assert_eq!("987654321", left_pad_zeroes(3, 987654321));
    }
}
