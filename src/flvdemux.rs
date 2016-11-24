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

#[derive(Debug)]
enum StreamingState {
    Stopped,
    Started {
        adapter: Adapter,
        stream_state: StreamState,
    },
}

#[derive(Debug)]
enum StreamState {
    NeedHeader,
    Initialized {
        header: flavors::Header,
        initialized_state: InitializedState,
    },
}

#[derive(Debug)]
enum InitializedState {
    Skipping { skip_left: u32 },
    Setup { setup_state: SetupState },
}

#[derive(Debug)]
struct SetupState {
    audio: Option<flavors::AudioDataHeader>,
    video: Option<flavors::VideoDataHeader>, /* TODO: parse and store various audio/video metadata from ScriptDataObject */
    got_all_streams: bool,
    last_position: Option<u64>,
}

#[derive(Debug)]
pub struct FlvDemux {
    streaming_state: StreamingState,
}

impl FlvDemux {
    pub fn new() -> FlvDemux {
        FlvDemux { streaming_state: StreamingState::Stopped }
    }

    pub fn new_boxed() -> Box<Demuxer> {
        Box::new(FlvDemux::new())
    }

    fn handle_script_tag(adapter: &mut Adapter,
                         header: &flavors::Header,
                         setup_state: &mut SetupState,
                         tag_header: &flavors::TagHeader)
                         -> Result<HandleBufferResult, FlowError> {
        adapter.flush((15 + tag_header.data_size) as usize).unwrap();

        Ok(HandleBufferResult::Again)
    }

    fn update_audio_stream(header: &flavors::Header,
                           setup_state: &mut SetupState,
                           data_header: flavors::AudioDataHeader)
                           -> Result<HandleBufferResult, FlowError> {
        let stream_changed = match setup_state.audio {
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
                    let new_stream = setup_state.audio == None;

                    setup_state.audio = Some(data_header);
                    let stream = Stream::new(0, format, String::from("audio"));
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
        setup_state.audio = Some(data_header);

        if !setup_state.got_all_streams &&
           (header.video && setup_state.video != None || !header.video) {
            setup_state.got_all_streams = true;
            return Ok(HandleBufferResult::HaveAllStreams);
        }

        Ok(HandleBufferResult::Again)
    }

    fn handle_audio_tag(adapter: &mut Adapter,
                        header: &flavors::Header,
                        setup_state: &mut SetupState,
                        tag_header: &flavors::TagHeader,
                        data_header: flavors::AudioDataHeader)
                        -> Result<HandleBufferResult, FlowError> {
        let res = Self::update_audio_stream(header, setup_state, data_header);
        match res {
            Ok(HandleBufferResult::Again) => (),
            _ => return res,
        }

        if adapter.get_available() < (15 + tag_header.data_size) as usize {
            return Ok(HandleBufferResult::NeedMoreData);
        }

        adapter.flush(16).unwrap();
        if tag_header.data_size == 0 {
            return Ok(HandleBufferResult::Again);
        }

        let mut buffer = adapter.get_buffer((tag_header.data_size - 1) as usize)
            .unwrap();
        buffer.set_pts(Some((tag_header.timestamp as u64) * 1000 * 1000))
            .unwrap();

        Ok(HandleBufferResult::BufferForStream(0, buffer))
    }

    fn update_video_stream(header: &flavors::Header,
                           setup_state: &mut SetupState,
                           data_header: flavors::VideoDataHeader)
                           -> Result<HandleBufferResult, FlowError> {
        let stream_changed = match setup_state.video {
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
                    let new_stream = setup_state.video == None;
                    setup_state.video = Some(data_header);

                    let stream = Stream::new(1, format, String::from("video"));
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

        setup_state.video = Some(data_header);

        if !setup_state.got_all_streams &&
           (header.audio && setup_state.audio != None || !header.audio) {
            setup_state.got_all_streams = true;
            return Ok(HandleBufferResult::HaveAllStreams);
        }

        Ok(HandleBufferResult::Again)
    }

    fn handle_video_tag(adapter: &mut Adapter,
                        header: &flavors::Header,
                        setup_state: &mut SetupState,
                        tag_header: &flavors::TagHeader,
                        data_header: flavors::VideoDataHeader)
                        -> Result<HandleBufferResult, FlowError> {
        let res = Self::update_video_stream(header, setup_state, data_header);
        match res {
            Ok(HandleBufferResult::Again) => (),
            _ => return res,
        }

        if adapter.get_available() < (15 + tag_header.data_size) as usize {
            return Ok(HandleBufferResult::NeedMoreData);
        }

        let video = setup_state.video.as_ref().unwrap();
        let is_keyframe = video.frame_type == flavors::FrameType::Key;

        adapter.flush(16).unwrap();

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
            adapter.flush((tag_header.data_size - 1) as usize).unwrap();
            return Ok(HandleBufferResult::Again);
        }

        if offset > 0 {
            adapter.flush(offset as usize).unwrap();
        }

        let mut buffer = adapter.get_buffer((tag_header.data_size - 1 - offset) as usize)
            .unwrap();
        if !is_keyframe {
            buffer.set_flags(BUFFER_FLAG_DELTA_UNIT).unwrap();
        }
        buffer.set_dts(Some((tag_header.timestamp as u64) * 1000 * 1000))
            .unwrap();

        Ok(HandleBufferResult::BufferForStream(1, buffer))
    }

    fn update_setup_state(adapter: &mut Adapter,
                          header: &flavors::Header,
                          setup_state: &mut SetupState)
                          -> Result<HandleBufferResult, FlowError> {
        if adapter.get_available() < 16 {
            return Ok(HandleBufferResult::NeedMoreData);
        }

        let mut data = [0u8; 16];
        adapter.peek_into(&mut data).unwrap();

        match nom::be_u32(&data[0..4]) {
            IResult::Error(_) |
            IResult::Incomplete(_) => {
                unimplemented!();
            }
            IResult::Done(_, previous_size) => {
                ();
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
            flavors::TagType::Script => {
                Self::handle_script_tag(adapter, header, setup_state, &tag_header)
            }
            flavors::TagType::Audio => {
                let data_header = match flavors::audio_data_header(&data[15..]) {
                    IResult::Error(_) |
                    IResult::Incomplete(_) => {
                        unimplemented!();
                    }
                    IResult::Done(_, data_header) => data_header,
                };

                Self::handle_audio_tag(adapter, header, setup_state, &tag_header, data_header)
            }
            flavors::TagType::Video => {
                let data_header = match flavors::video_data_header(&data[15..]) {
                    IResult::Error(_) |
                    IResult::Incomplete(_) => {
                        unimplemented!();
                    }
                    IResult::Done(_, data_header) => data_header,
                };

                Self::handle_video_tag(adapter, header, setup_state, &tag_header, data_header)
            }
        };

        if let Ok(HandleBufferResult::BufferForStream(_, ref buffer)) = res {
            if let Some(pts) = buffer.get_pts() {
                setup_state.last_position =
                    setup_state.last_position.map(|last| cmp::max(last, pts)).or_else(|| Some(pts));
            } else if let Some(dts) = buffer.get_dts() {
                setup_state.last_position =
                    setup_state.last_position.map(|last| cmp::max(last, dts)).or_else(|| Some(dts));
            }
        }

        res
    }

    fn update_initialized_state(adapter: &mut Adapter,
                                header: &flavors::Header,
                                initialized_state: &mut InitializedState)
                                -> Result<HandleBufferResult, FlowError> {
        match *initialized_state {
            InitializedState::Skipping { skip_left: 0 } => {
                *initialized_state = InitializedState::Setup {
                    setup_state: SetupState {
                        audio: None,
                        video: None,
                        got_all_streams: false,
                        last_position: None,
                    },
                };
                Ok(HandleBufferResult::Again)
            }
            InitializedState::Skipping { ref mut skip_left } => {
                let skip = cmp::min(adapter.get_available(), *skip_left as usize);
                adapter.flush(skip).unwrap();
                *skip_left -= skip as u32;

                Ok(HandleBufferResult::Again)
            }
            InitializedState::Setup { ref mut setup_state } => {
                Self::update_setup_state(adapter, header, setup_state)
            }
        }
    }

    fn update_state(adapter: &mut Adapter,
                    stream_state: &mut StreamState)
                    -> Result<HandleBufferResult, FlowError> {
        match *stream_state {
            StreamState::NeedHeader => {
                while adapter.get_available() >= 9 {
                    let mut data = [0u8; 9];
                    adapter.peek_into(&mut data).unwrap();

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
                            adapter.flush(9).unwrap();

                            *stream_state = StreamState::Initialized {
                                header: header,
                                initialized_state: InitializedState::Skipping { skip_left: skip },
                            };

                            return Ok(HandleBufferResult::Again);
                        }
                    }

                    adapter.flush(1).unwrap();
                }

                Ok(HandleBufferResult::NeedMoreData)
            }
            StreamState::Initialized { ref header, ref mut initialized_state } => {
                Self::update_initialized_state(adapter, header, initialized_state)
            }
        }
    }
}

impl Demuxer for FlvDemux {
    fn start(&mut self,
             _upstream_size: Option<u64>,
             _random_access: bool)
             -> Result<(), ErrorMessage> {
        self.streaming_state = StreamingState::Started {
            adapter: Adapter::new(),
            stream_state: StreamState::NeedHeader,
        };

        Ok(())
    }

    fn stop(&mut self) -> Result<(), ErrorMessage> {
        self.streaming_state = StreamingState::Stopped;

        Ok(())
    }

    fn seek(&mut self, start: u64, stop: Option<u64>) -> Result<SeekResult, ErrorMessage> {
        unimplemented!();
    }

    fn handle_buffer(&mut self, buffer: Option<Buffer>) -> Result<HandleBufferResult, FlowError> {
        let (adapter, stream_state) = match self.streaming_state {
            StreamingState::Started { ref mut adapter, ref mut stream_state } => {
                (adapter, stream_state)
            }
            StreamingState::Stopped => {
                unreachable!();
            }
        };

        if let Some(buffer) = buffer {
            adapter.push(buffer);
        }

        Self::update_state(adapter, stream_state)
    }

    fn end_of_stream(&mut self) -> Result<(), ErrorMessage> {
        // nothing to do here, all data we have left is incomplete
        Ok(())
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn get_position(&self) -> Option<u64> {
        if let StreamingState::Started {
            stream_state: StreamState::Initialized {
                initialized_state: InitializedState::Setup {
                    setup_state: SetupState {
                        last_position, ..
                    }, ..
                }, ..
            }, ..
        } = self.streaming_state {
            return last_position;
        }

        None
    }

    fn get_duration(&self) -> Option<u64> {
        None
    }
}
