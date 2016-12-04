//  Copyright (C) 2016 Sebastian Dröge <sebastian@centricular.com>
//
//  This library is free software; you can redistribute it and/or
//  modify it under the terms of the GNU Library General Public
//  License as published by the Free Software Foundation; either
//  version 2 of the License, or (at your option) any later version.
//
//  This library is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
//  Library General Public License for more details.
//
//  You should have received a copy of the GNU Library General Public
//  License along with this library; if not, write to the
//  Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
//  Boston, MA 02110-1301, USA.

use std::cmp;

use nom;
use nom::IResult;

use flavors::parser as flavors;

use error::*;
use rsdemuxer::*;
use buffer::*;
use adapter::*;

const AUDIO_STREAM_ID: u32 = 0;
const VIDEO_STREAM_ID: u32 = 1;

#[derive(Debug)]
enum State {
    Stopped,
    NeedHeader,
    Skipping {
        audio: bool,
        video: bool,
        skip_left: u32,
    },
    Streaming,
}

#[derive(Debug)]
struct StreamingState {
    audio: Option<AudioFormat>,
    expect_audio: bool,
    video: Option<VideoFormat>,
    expect_video: bool,
    got_all_streams: bool,
    last_position: Option<u64>,

    metadata: Option<Metadata>,
}

impl StreamingState {
    fn new(audio: bool, video: bool) -> StreamingState {
        StreamingState {
            audio: None,
            expect_audio: audio,
            video: None,
            expect_video: video,
            got_all_streams: false,
            last_position: None,
            metadata: None,
        }
    }
}

#[derive(Debug, Eq, Clone)]
struct AudioFormat {
    format: flavors::SoundFormat,
    rate: u16,
    width: u8,
    channels: u8,
    bitrate: Option<u32>,
}

// Ignores bitrate
impl PartialEq for AudioFormat {
    fn eq(&self, other: &Self) -> bool {
        self.format.eq(&other.format) && self.rate.eq(&other.rate) && self.width.eq(&other.width) &&
        self.channels.eq(&other.channels)
    }
}

impl AudioFormat {
    fn new(data_header: &flavors::AudioDataHeader, metadata: &Option<Metadata>) -> AudioFormat {
        let numeric_rate = match (data_header.sound_format, data_header.sound_rate) {
            (flavors::SoundFormat::NELLYMOSER_16KHZ_MONO, _) => 16000,
            (flavors::SoundFormat::NELLYMOSER_8KHZ_MONO, _) => 8000,
            (flavors::SoundFormat::MP3_8KHZ, _) => 8000,
            (_, flavors::SoundRate::_5_5KHZ) => 5500,
            (_, flavors::SoundRate::_11KHZ) => 11025,
            (_, flavors::SoundRate::_22KHZ) => 22050,
            (_, flavors::SoundRate::_44KHZ) => 44100,
        };

        let numeric_width = match data_header.sound_size {
            flavors::SoundSize::Snd8bit => 8,
            flavors::SoundSize::Snd16bit => 16,
        };

        let numeric_channels = match data_header.sound_type {
            flavors::SoundType::SndMono => 1,
            flavors::SoundType::SndStereo => 2,
        };

        AudioFormat {
            format: data_header.sound_format,
            rate: numeric_rate,
            width: numeric_width,
            channels: numeric_channels,
            bitrate: metadata.as_ref().and_then(|m| m.audio_bitrate),
        }
    }

    fn update_with_metadata(&mut self, metadata: &Metadata) -> bool {
        let mut changed = false;

        if self.bitrate != metadata.audio_bitrate {
            self.bitrate = metadata.audio_bitrate;
            changed = true;
        }

        changed
    }

    fn to_string(&self) -> Option<String> {
        match self.format {
            flavors::SoundFormat::MP3 => {
                let mut format = String::from("audio/mpeg, mpegversion=(int) 1, layer=(int) 3");
                if self.rate != 0 {
                    format.push_str(&format!(", rate=(int) {}", self.rate));
                }
                if self.channels != 0 {
                    format.push_str(&format!(", channels=(int) {}", self.channels));
                }

                Some(format)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Eq, Clone)]
struct VideoFormat {
    format: flavors::CodecId,
    width: Option<u32>,
    height: Option<u32>,
    pixel_aspect_ratio: Option<(u32, u32)>,
    framerate: Option<(u32, u32)>,
    bitrate: Option<u32>,
}

impl VideoFormat {
    fn new(data_header: &flavors::VideoDataHeader, metadata: &Option<Metadata>) -> VideoFormat {
        VideoFormat {
            format: data_header.codec_id,
            width: metadata.as_ref().and_then(|m| m.video_width),
            height: metadata.as_ref().and_then(|m| m.video_height),
            pixel_aspect_ratio: metadata.as_ref().and_then(|m| m.video_pixel_aspect_ratio),
            framerate: metadata.as_ref().and_then(|m| m.video_framerate),
            bitrate: metadata.as_ref().and_then(|m| m.video_bitrate),
        }
    }

    fn update_with_metadata(&mut self, metadata: &Metadata) -> bool {
        let mut changed = false;

        if self.width != metadata.video_width {
            self.width = metadata.video_width;
            changed = true;
        }

        if self.height != metadata.video_height {
            self.height = metadata.video_height;
            changed = true;
        }

        if self.pixel_aspect_ratio != metadata.video_pixel_aspect_ratio {
            self.pixel_aspect_ratio = metadata.video_pixel_aspect_ratio;
            changed = true;
        }

        if self.framerate != metadata.video_framerate {
            self.framerate = metadata.video_framerate;
            changed = true;
        }

        if self.bitrate != metadata.video_bitrate {
            self.bitrate = metadata.video_bitrate;
            changed = true;
        }

        changed
    }

    fn to_string(&self) -> Option<String> {
        match self.format {
            flavors::CodecId::VP6 => {
                let mut format = String::from("video/x-vp6-flash");
                if let (Some(width), Some(height)) = (self.width, self.height) {
                    format.push_str(&format!(", width=(int) {}, height=(int) {}", width, height));
                }
                if let Some(par) = self.pixel_aspect_ratio {
                    if par.0 != 0 && par.1 != 0 {
                        format.push_str(&format!(", pixel-aspect-ratio=(fraction) {}/{}",
                                                 par.0,
                                                 par.1));
                    }
                }
                if let Some(fps) = self.framerate {
                    if fps.1 != 0 {
                        format.push_str(&format!(", framerate=(fraction) {}/{}", fps.0, fps.1));
                    }
                }

                Some(format)
            }
            _ => None,
        }
    }
}

// Ignores bitrate
impl PartialEq for VideoFormat {
    fn eq(&self, other: &Self) -> bool {
        self.format.eq(&other.format) && self.width.eq(&other.width) &&
        self.height.eq(&other.height) &&
        self.pixel_aspect_ratio.eq(&other.pixel_aspect_ratio) &&
        self.framerate.eq(&other.framerate)
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct Metadata {
    duration: Option<u64>,

    creation_date: Option<String>,
    creator: Option<String>,
    title: Option<String>,
    metadata_creator: Option<String>, /* TODO: seek_table: _,
                                       * filepositions / times metadata arrays */

    audio_bitrate: Option<u32>,

    video_width: Option<u32>,
    video_height: Option<u32>,
    video_pixel_aspect_ratio: Option<(u32, u32)>,
    video_framerate: Option<(u32, u32)>,
    video_bitrate: Option<u32>,
}

impl Metadata {
    fn new(script_data: &flavors::ScriptData) -> Metadata {
        assert_eq!(script_data.name, "onMetaData");

        let mut metadata = Metadata {
            duration: None,
            creation_date: None,
            creator: None,
            title: None,
            metadata_creator: None,
            audio_bitrate: None,
            video_width: None,
            video_height: None,
            video_pixel_aspect_ratio: None,
            video_framerate: None,
            video_bitrate: None,
        };

        let args = match script_data.arguments {
            flavors::ScriptDataValue::Object(ref objects) => objects,
            flavors::ScriptDataValue::ECMAArray(ref objects) => objects,
            flavors::ScriptDataValue::StrictArray(ref objects) => objects,
            _ => return metadata,
        };

        let mut par_n = None;
        let mut par_d = None;

        for arg in args {
            match (arg.name, &arg.data) {
                ("duration", &flavors::ScriptDataValue::Number(duration)) => {
                    metadata.duration = Some((duration * 1000.0 * 1000.0 * 1000.0) as u64);
                }
                ("creationdate", &flavors::ScriptDataValue::String(date)) => {
                    metadata.creation_date = Some(String::from(date));
                }
                ("creator", &flavors::ScriptDataValue::String(creator)) => {
                    metadata.creator = Some(String::from(creator));
                }
                ("title", &flavors::ScriptDataValue::String(title)) => {
                    metadata.title = Some(String::from(title));
                }
                ("metadatacreator", &flavors::ScriptDataValue::String(creator)) => {
                    metadata.metadata_creator = Some(String::from(creator));
                }
                ("audiodatarate", &flavors::ScriptDataValue::Number(datarate)) => {
                    metadata.audio_bitrate = Some((datarate * 1024.0) as u32);
                }
                ("width", &flavors::ScriptDataValue::Number(width)) => {
                    metadata.video_width = Some(width as u32);
                }
                ("height", &flavors::ScriptDataValue::Number(height)) => {
                    metadata.video_height = Some(height as u32);
                }
                ("framerate", &flavors::ScriptDataValue::Number(framerate)) => {
                    // FIXME: Convert properly to a fraction
                    metadata.video_framerate = Some((framerate as u32, 1));
                }
                ("AspectRatioX", &flavors::ScriptDataValue::Number(par_x)) => {
                    par_n = Some(par_x as u32);
                }
                ("AspectRatioY", &flavors::ScriptDataValue::Number(par_y)) => {
                    par_d = Some(par_y as u32);
                }
                ("videodatarate", &flavors::ScriptDataValue::Number(datarate)) => {
                    metadata.video_bitrate = Some((datarate * 1024.0) as u32);
                }
                _ => {}
            }
        }

        if let (Some(par_n), Some(par_d)) = (par_n, par_d) {
            metadata.video_pixel_aspect_ratio = Some((par_n, par_d));
        }

        metadata
    }
}

#[derive(Debug)]
pub struct FlvDemux {
    state: State,
    adapter: Adapter,
    // Only in >= State::Streaming
    streaming_state: Option<StreamingState>,
}

impl FlvDemux {
    pub fn new() -> FlvDemux {
        FlvDemux {
            state: State::Stopped,
            adapter: Adapter::new(),
            streaming_state: None,
        }
    }

    pub fn new_boxed() -> Box<Demuxer> {
        Box::new(FlvDemux::new())
    }

    fn handle_script_tag(&mut self,
                         tag_header: &flavors::TagHeader)
                         -> Result<HandleBufferResult, FlowError> {
        if self.adapter.get_available() < (15 + tag_header.data_size) as usize {
            return Ok(HandleBufferResult::NeedMoreData);
        }

        self.adapter.flush(15).unwrap();

        let buffer = self.adapter.get_buffer(tag_header.data_size as usize).unwrap();
        let map = buffer.map_read().unwrap();
        let data = map.as_slice();

        match flavors::script_data(data) {
            IResult::Done(_, ref script_data) if script_data.name == "onMetaData" => {
                let streaming_state = self.streaming_state.as_mut().unwrap();
                let metadata = Metadata::new(script_data);

                let audio_changed = streaming_state.audio
                    .as_mut()
                    .map(|a| a.update_with_metadata(&metadata))
                    .unwrap_or(false);
                let video_changed = streaming_state.video
                    .as_mut()
                    .map(|v| v.update_with_metadata(&metadata))
                    .unwrap_or(false);
                streaming_state.metadata = Some(metadata);

                if audio_changed || video_changed {
                    let mut streams = Vec::new();

                    if audio_changed {
                        if let Some(format) = streaming_state.audio
                            .as_ref()
                            .and_then(|a| a.to_string()) {
                            streams.push(Stream::new(AUDIO_STREAM_ID,
                                                         format,
                                                         String::from("audio")));
                        }
                    }
                    if video_changed {
                        if let Some(format) = streaming_state.video
                            .as_ref()
                            .and_then(|v| v.to_string()) {
                            streams.push(Stream::new(VIDEO_STREAM_ID,
                                                         format,
                                                         String::from("video")));
                        }
                    }

                    return Ok(HandleBufferResult::StreamsChanged(streams));
                }
            }
            IResult::Done(_, _) |
            IResult::Error(_) |
            IResult::Incomplete(_) => {
                // ignore
            }
        }

        Ok(HandleBufferResult::Again)
    }

    fn update_audio_stream(&mut self,
                           data_header: &flavors::AudioDataHeader)
                           -> Result<HandleBufferResult, FlowError> {
        let streaming_state = self.streaming_state.as_mut().unwrap();

        let new_audio_format = AudioFormat::new(data_header, &streaming_state.metadata);

        if streaming_state.audio.as_ref() != Some(&new_audio_format) {
            let new_stream = streaming_state.audio == None;

            let format = new_audio_format.to_string();
            if let Some(format) = format {
                streaming_state.audio = Some(new_audio_format);
                let stream = Stream::new(AUDIO_STREAM_ID, format, String::from("audio"));
                if new_stream {
                    return Ok(HandleBufferResult::StreamAdded(stream));
                } else {
                    return Ok(HandleBufferResult::StreamChanged(stream));
                }
            }
        }

        if !streaming_state.got_all_streams &&
           (streaming_state.expect_video && streaming_state.video != None ||
            !streaming_state.expect_video) {
            streaming_state.got_all_streams = true;
            return Ok(HandleBufferResult::HaveAllStreams);
        }

        Ok(HandleBufferResult::Again)
    }

    fn handle_audio_tag(&mut self,
                        tag_header: &flavors::TagHeader,
                        data_header: &flavors::AudioDataHeader)
                        -> Result<HandleBufferResult, FlowError> {
        let res = self.update_audio_stream(data_header);
        match res {
            Ok(HandleBufferResult::Again) => (),
            _ => return res,
        }

        if self.adapter.get_available() < (15 + tag_header.data_size) as usize {
            return Ok(HandleBufferResult::NeedMoreData);
        }

        self.adapter.flush(16).unwrap();
        if tag_header.data_size == 0 {
            return Ok(HandleBufferResult::Again);
        }

        let mut buffer = self.adapter.get_buffer((tag_header.data_size - 1) as usize).unwrap();
        buffer.set_pts(Some((tag_header.timestamp as u64) * 1000 * 1000)).unwrap();

        Ok(HandleBufferResult::BufferForStream(AUDIO_STREAM_ID, buffer))
    }

    fn update_video_stream(&mut self,
                           data_header: &flavors::VideoDataHeader)
                           -> Result<HandleBufferResult, FlowError> {
        let streaming_state = self.streaming_state.as_mut().unwrap();

        let new_video_format = VideoFormat::new(data_header, &streaming_state.metadata);

        if streaming_state.video.as_ref() != Some(&new_video_format) {
            let new_stream = streaming_state.video == None;

            let format = new_video_format.to_string();
            if let Some(format) = format {
                streaming_state.video = Some(new_video_format);
                let stream = Stream::new(VIDEO_STREAM_ID, format, String::from("video"));
                if new_stream {
                    return Ok(HandleBufferResult::StreamAdded(stream));
                } else {
                    return Ok(HandleBufferResult::StreamChanged(stream));
                }
            }
        }

        if !streaming_state.got_all_streams &&
           (streaming_state.expect_audio && streaming_state.audio != None ||
            !streaming_state.expect_audio) {
            streaming_state.got_all_streams = true;
            return Ok(HandleBufferResult::HaveAllStreams);
        }

        Ok(HandleBufferResult::Again)
    }

    fn handle_video_tag(&mut self,
                        tag_header: &flavors::TagHeader,
                        data_header: &flavors::VideoDataHeader)
                        -> Result<HandleBufferResult, FlowError> {
        let res = self.update_video_stream(data_header);
        match res {
            Ok(HandleBufferResult::Again) => (),
            _ => return res,
        }

        if self.adapter.get_available() < (15 + tag_header.data_size) as usize {
            return Ok(HandleBufferResult::NeedMoreData);
        }

        let streaming_state = self.streaming_state.as_ref().unwrap();
        let video = streaming_state.video.as_ref().unwrap();
        let is_keyframe = data_header.frame_type == flavors::FrameType::Key;

        self.adapter.flush(16).unwrap();

        let offset = if video.format == flavors::CodecId::VP6 ||
                        video.format == flavors::CodecId::VP6A {
            1
        } else {
            0
        };

        if tag_header.data_size == 0 {
            return Ok(HandleBufferResult::Again);
        }

        if tag_header.data_size < offset {
            self.adapter.flush((tag_header.data_size - 1) as usize).unwrap();
            return Ok(HandleBufferResult::Again);
        }

        if offset > 0 {
            self.adapter.flush(offset as usize).unwrap();
        }

        let mut buffer = self.adapter
            .get_buffer((tag_header.data_size - 1 - offset) as usize)
            .unwrap();
        if !is_keyframe {
            buffer.set_flags(BUFFER_FLAG_DELTA_UNIT).unwrap();
        }
        buffer.set_dts(Some((tag_header.timestamp as u64) * 1000 * 1000))
            .unwrap();

        Ok(HandleBufferResult::BufferForStream(VIDEO_STREAM_ID, buffer))
    }

    fn update_state(&mut self) -> Result<HandleBufferResult, FlowError> {
        match self.state {
            State::Stopped => unreachable!(),
            State::NeedHeader => {
                while self.adapter.get_available() >= 9 {
                    let mut data = [0u8; 9];
                    self.adapter.peek_into(&mut data).unwrap();

                    match flavors::header(&data) {
                        IResult::Error(_) |
                        IResult::Incomplete(_) => {
                            // fall through
                        }
                        IResult::Done(_, ref header) => {
                            let skip = if header.offset < 9 {
                                0
                            } else {
                                header.offset - 9
                            };

                            self.adapter.flush(9).unwrap();

                            self.state = State::Skipping {
                                audio: header.audio,
                                video: header.video,
                                skip_left: skip,
                            };

                            return Ok(HandleBufferResult::Again);
                        }
                    }

                    self.adapter.flush(1).unwrap();
                }

                Ok(HandleBufferResult::NeedMoreData)
            }
            State::Skipping { audio, video, skip_left: 0 } => {
                self.state = State::Streaming;
                self.streaming_state = Some(StreamingState::new(audio, video));

                Ok(HandleBufferResult::Again)
            }
            State::Skipping { ref mut skip_left, .. } => {
                let skip = cmp::min(self.adapter.get_available(), *skip_left as usize);
                self.adapter.flush(skip).unwrap();
                *skip_left -= skip as u32;

                Ok(HandleBufferResult::Again)
            }
            State::Streaming => {
                if self.adapter.get_available() < 16 {
                    return Ok(HandleBufferResult::NeedMoreData);
                }

                let mut data = [0u8; 16];
                self.adapter.peek_into(&mut data).unwrap();

                match nom::be_u32(&data[0..4]) {
                    IResult::Error(_) |
                    IResult::Incomplete(_) => {
                        unimplemented!();
                    }
                    IResult::Done(_, _previous_size) => {
                        // Nothing to do here, we just consume it for now
                    }
                }

                let tag_header = match flavors::tag_header(&data[4..]) {
                    IResult::Error(_) |
                    IResult::Incomplete(_) => {
                        unimplemented!();
                    }
                    IResult::Done(_, tag_header) => tag_header,
                };

                let res = match tag_header.tag_type {
                    flavors::TagType::Script => self.handle_script_tag(&tag_header),
                    flavors::TagType::Audio => {
                        let data_header = match flavors::audio_data_header(&data[15..]) {
                            IResult::Error(_) |
                            IResult::Incomplete(_) => {
                                unimplemented!();
                            }
                            IResult::Done(_, data_header) => data_header,
                        };

                        self.handle_audio_tag(&tag_header, &data_header)
                    }
                    flavors::TagType::Video => {
                        let data_header = match flavors::video_data_header(&data[15..]) {
                            IResult::Error(_) |
                            IResult::Incomplete(_) => {
                                unimplemented!();
                            }
                            IResult::Done(_, data_header) => data_header,
                        };

                        self.handle_video_tag(&tag_header, &data_header)
                    }
                };

                if let Ok(HandleBufferResult::BufferForStream(_, ref buffer)) = res {
                    let streaming_state = self.streaming_state.as_mut().unwrap();

                    if let Some(pts) = buffer.get_pts() {
                        streaming_state.last_position = streaming_state.last_position
                            .map(|last| cmp::max(last, pts))
                            .or_else(|| Some(pts));
                    } else if let Some(dts) = buffer.get_dts() {
                        streaming_state.last_position = streaming_state.last_position
                            .map(|last| cmp::max(last, dts))
                            .or_else(|| Some(dts));
                    }
                }

                res

            }
        }
    }
}

impl Demuxer for FlvDemux {
    fn start(&mut self,
             _upstream_size: Option<u64>,
             _random_access: bool)
             -> Result<(), ErrorMessage> {
        self.state = State::NeedHeader;

        Ok(())
    }

    fn stop(&mut self) -> Result<(), ErrorMessage> {
        self.state = State::Stopped;
        self.adapter.clear();
        self.streaming_state = None;

        Ok(())
    }

    fn seek(&mut self, start: u64, stop: Option<u64>) -> Result<SeekResult, ErrorMessage> {
        unimplemented!();
    }

    fn handle_buffer(&mut self, buffer: Option<Buffer>) -> Result<HandleBufferResult, FlowError> {
        if let Some(buffer) = buffer {
            self.adapter.push(buffer);
        }

        self.update_state()
    }

    fn end_of_stream(&mut self) -> Result<(), ErrorMessage> {
        // nothing to do here, all data we have left is incomplete
        Ok(())
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn get_position(&self) -> Option<u64> {
        if let Some(StreamingState { last_position, .. }) = self.streaming_state {
            return last_position;
        }

        None
    }

    fn get_duration(&self) -> Option<u64> {
        if let Some(StreamingState { metadata: Some(Metadata { duration, .. }), .. }) =
            self.streaming_state {
            return duration;
        }

        None
    }
}
