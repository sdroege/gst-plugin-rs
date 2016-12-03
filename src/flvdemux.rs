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
    Skipping { skip_left: u32 },
    Streaming,
}

#[derive(Debug)]
struct StreamingState {
    // TODO: Store in custom structs that just contain what we need
    audio: Option<flavors::AudioDataHeader>,
    video: Option<flavors::VideoDataHeader>,
    got_all_streams: bool,
    last_position: Option<u64>, // TODO: parse and store various audio/video metadata from ScriptDataObject
}

#[derive(Debug)]
pub struct FlvDemux {
    state: State,
    adapter: Adapter,
    // Only in >= State::Skipping
    header: Option<flavors::Header>,
    // Only in >= State::Streaming
    streaming_state: Option<StreamingState>,
}

impl FlvDemux {
    pub fn new() -> FlvDemux {
        FlvDemux {
            state: State::Stopped,
            adapter: Adapter::new(),
            header: None,
            streaming_state: None,
        }
    }

    pub fn new_boxed() -> Box<Demuxer> {
        Box::new(FlvDemux::new())
    }

    fn handle_script_tag(&mut self,
                         tag_header: &flavors::TagHeader)
                         -> Result<HandleBufferResult, FlowError> {
        self.adapter.flush((15 + tag_header.data_size) as usize).unwrap();

        Ok(HandleBufferResult::Again)
    }

    fn update_audio_stream(&mut self,
                           data_header: flavors::AudioDataHeader)
                           -> Result<HandleBufferResult, FlowError> {
        let header = self.header.as_ref().unwrap();
        let streaming_state = self.streaming_state.as_mut().unwrap();

        let stream_changed = match streaming_state.audio {
            None => true,
            Some(flavors::AudioDataHeader { sound_format, sound_rate, sound_size, sound_type })
                if sound_format != data_header.sound_format ||
                   sound_rate != data_header.sound_rate ||
                   sound_size != data_header.sound_size ||
                   sound_type != data_header.sound_type => true,
            _ => false,
        };

        if stream_changed {
            match data_header {
                flavors::AudioDataHeader { sound_format: flavors::SoundFormat::MP3, .. } => {
                    let format = String::from("audio/mpeg, mpegversion=1, layer=3");
                    let new_stream = streaming_state.audio == None;

                    streaming_state.audio = Some(data_header);
                    let stream = Stream::new(AUDIO_STREAM_ID, format, String::from("audio"));
                    if new_stream {
                        return Ok(HandleBufferResult::StreamAdded(stream));
                    } else {
                        return Ok(HandleBufferResult::StreamChanged(stream));
                    }
                }
                _ => {
                    unimplemented!();
                }
            }
        }
        streaming_state.audio = Some(data_header);

        if !streaming_state.got_all_streams &&
           (header.video && streaming_state.video != None || !header.video) {
            streaming_state.got_all_streams = true;
            return Ok(HandleBufferResult::HaveAllStreams);
        }

        Ok(HandleBufferResult::Again)
    }

    fn handle_audio_tag(&mut self,
                        tag_header: &flavors::TagHeader,
                        data_header: flavors::AudioDataHeader)
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
                           data_header: flavors::VideoDataHeader)
                           -> Result<HandleBufferResult, FlowError> {
        let header = self.header.as_ref().unwrap();
        let streaming_state = self.streaming_state.as_mut().unwrap();

        let stream_changed = match streaming_state.video {
            None => true,
            Some(flavors::VideoDataHeader { codec_id, .. }) if codec_id != data_header.codec_id => {
                true
            }
            _ => false,
        };

        if stream_changed {
            match data_header {
                flavors::VideoDataHeader { codec_id: flavors::CodecId::VP6, .. } => {
                    let format = String::from("video/x-vp6-flash");
                    let new_stream = streaming_state.video == None;
                    streaming_state.video = Some(data_header);

                    let stream = Stream::new(VIDEO_STREAM_ID, format, String::from("video"));
                    if new_stream {
                        return Ok(HandleBufferResult::StreamAdded(stream));
                    } else {
                        return Ok(HandleBufferResult::StreamChanged(stream));
                    }
                }
                _ => {
                    unimplemented!();
                }
            }
        }

        streaming_state.video = Some(data_header);

        if !streaming_state.got_all_streams &&
           (header.audio && streaming_state.audio != None || !header.audio) {
            streaming_state.got_all_streams = true;
            return Ok(HandleBufferResult::HaveAllStreams);
        }

        Ok(HandleBufferResult::Again)
    }

    fn handle_video_tag(&mut self,
                        tag_header: &flavors::TagHeader,
                        data_header: flavors::VideoDataHeader)
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
        let is_keyframe = video.frame_type == flavors::FrameType::Key;

        self.adapter.flush(16).unwrap();

        let offset = if video.codec_id == flavors::CodecId::VP6 ||
                        video.codec_id == flavors::CodecId::VP6A {
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
                        IResult::Done(_, mut header) => {
                            if header.offset < 9 {
                                header.offset = 9;
                            }
                            let skip = header.offset - 9;
                            self.adapter.flush(9).unwrap();

                            self.header = Some(header);
                            self.state = State::Skipping { skip_left: skip };

                            return Ok(HandleBufferResult::Again);
                        }
                    }

                    self.adapter.flush(1).unwrap();
                }

                Ok(HandleBufferResult::NeedMoreData)
            }
            State::Skipping { skip_left: 0 } => {
                self.state = State::Streaming;
                self.streaming_state = Some(StreamingState {
                    audio: None,
                    video: None,
                    got_all_streams: false,
                    last_position: None,
                });

                Ok(HandleBufferResult::Again)
            }
            State::Skipping { ref mut skip_left } => {
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

                        self.handle_audio_tag(&tag_header, data_header)
                    }
                    flavors::TagType::Video => {
                        let data_header = match flavors::video_data_header(&data[15..]) {
                            IResult::Error(_) |
                            IResult::Incomplete(_) => {
                                unimplemented!();
                            }
                            IResult::Done(_, data_header) => data_header,
                        };

                        self.handle_video_tag(&tag_header, data_header)
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
        self.header = None;
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
        None
    }
}
