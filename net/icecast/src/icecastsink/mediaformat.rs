// GStreamer Icecast Sink - media format utils
//
// Copyright (C) 2023-2024 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use super::imp::CAT;
use super::utils;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct AudioInfo {
    pub(super) rate: i32,
    pub(super) channels: i32,
}

// Streamheaders are needed when reconnecting
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) enum MediaFormat {
    #[default]
    None,
    Mpeg1Audio(AudioInfo),
    AacAudio(AudioInfo),
    FlacAudio(AudioInfo, Vec<gst::Buffer>),
    OggAudio(AudioInfo, &'static str, Vec<gst::Buffer>),
}

impl MediaFormat {
    pub(super) fn from_caps(caps: &gst::Caps) -> Result<Self, gst::LoggableError> {
        let s = caps.structure(0).unwrap();

        let media_format = match s.name().as_str() {
            "audio/mpeg" => {
                let rate = s.get::<i32>("rate").unwrap();
                let channels = s.get::<i32>("channels").unwrap();
                let mpegversion = s.get::<i32>("mpegversion").unwrap();

                let audio_info = AudioInfo { rate, channels };

                match mpegversion {
                    1 => MediaFormat::Mpeg1Audio(audio_info),
                    2 | 4 => MediaFormat::AacAudio(audio_info),
                    _ => unreachable!(),
                }
            }
            "audio/x-flac" => {
                let rate = s.get::<i32>("rate").unwrap();
                let channels = s.get::<i32>("channels").unwrap();
                let audio_info = AudioInfo { rate, channels };

                // Technically this is optional and we could pick the headers up in-band, but
                // for now require stream headers for simplicity, as they will be useful when
                // trying to implement reconnecting later.
                let streamheaders = utils::get_streamheaders_from_caps(s)?;

                MediaFormat::FlacAudio(audio_info, streamheaders)
            }
            "audio/ogg" => {
                let streamheaders = utils::get_streamheaders_from_caps(s)?;

                let (codec_name, audio_info) = utils::parse_ogg_audio_streamheaders(&streamheaders)
                    .map_err(|error_string| gst::loggable_error!(CAT, "{}", error_string))?;

                MediaFormat::OggAudio(audio_info, codec_name, streamheaders)
            }
            media_type => {
                unreachable!("Caps with unexpected media type {media_type}");
            }
        };

        Ok(media_format)
    }

    pub(super) fn content_type(&self) -> Option<String> {
        let content_type = match self {
            MediaFormat::None => return None,
            MediaFormat::Mpeg1Audio(..) => "audio/mpeg".to_string(),
            MediaFormat::AacAudio(..) => "audio/aac".to_string(),
            MediaFormat::FlacAudio(..) => "audio/flac".to_string(),
            // rsas 1.3 only seems to works with plain audio/ogg type, needs more investigation
            // and testing, also with other servers.
            // MediaFormat::OggAudio(_, codec_name, ..) => format!("audio/ogg; codecs={codec_name}"),
            MediaFormat::OggAudio(_, _codec_name, ..) => "audio/ogg".to_string(),
        };
        Some(content_type)
    }

    pub(super) fn ice_audio_info(&self) -> Option<String> {
        match self {
            MediaFormat::None => None,
            MediaFormat::Mpeg1Audio(audio_info)
            | MediaFormat::AacAudio(audio_info)
            | MediaFormat::FlacAudio(audio_info, ..)
            | MediaFormat::OggAudio(audio_info, ..) => Some(format!(
                "channels={};samplerate={}",
                audio_info.channels, audio_info.rate
            )),
        }
    }

    pub(super) fn stream_headers(&self) -> Vec<gst::Buffer> {
        match self {
            MediaFormat::None => vec![],
            MediaFormat::Mpeg1Audio(..) => vec![],
            MediaFormat::AacAudio(..) => vec![],
            MediaFormat::FlacAudio(_, stream_headers) => stream_headers.clone(),
            MediaFormat::OggAudio(_, _, stream_headers) => stream_headers.clone(),
        }
    }
}
