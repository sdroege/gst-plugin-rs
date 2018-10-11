// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::cmp;
use std::io::{Cursor, Write};

use nom;
use nom::IResult;

use flavors::parser as flavors;

use gst_plugin::adapter::*;
use gst_plugin::bytes::*;
use gst_plugin::element::*;
use gst_plugin::error::*;
use gst_plugin_simple::demuxer::*;

use gst;

use num_rational::Rational32;

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
    last_position: gst::ClockTime,

    metadata: Option<Metadata>,

    aac_sequence_header: Option<gst::Buffer>,
    avc_sequence_header: Option<gst::Buffer>,
}

impl StreamingState {
    fn new(audio: bool, video: bool) -> StreamingState {
        StreamingState {
            audio: None,
            expect_audio: audio,
            video: None,
            expect_video: video,
            got_all_streams: false,
            last_position: gst::CLOCK_TIME_NONE,
            metadata: None,
            aac_sequence_header: None,
            avc_sequence_header: None,
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
    aac_sequence_header: Option<gst::Buffer>,
}

// Ignores bitrate
impl PartialEq for AudioFormat {
    fn eq(&self, other: &Self) -> bool {
        self.format.eq(&other.format)
            && self.rate.eq(&other.rate)
            && self.width.eq(&other.width)
            && self.channels.eq(&other.channels)
            && self.aac_sequence_header.eq(&other.aac_sequence_header)
    }
}

impl AudioFormat {
    fn new(
        data_header: &flavors::AudioDataHeader,
        metadata: &Option<Metadata>,
        aac_sequence_header: &Option<gst::Buffer>,
    ) -> AudioFormat {
        let numeric_rate = match (data_header.sound_format, data_header.sound_rate) {
            (flavors::SoundFormat::NELLYMOSER_16KHZ_MONO, _) => 16_000,
            (flavors::SoundFormat::NELLYMOSER_8KHZ_MONO, _) => 8_000,
            (flavors::SoundFormat::MP3_8KHZ, _) => 8_000,
            (flavors::SoundFormat::SPEEX, _) => 16_000,
            (_, flavors::SoundRate::_5_5KHZ) => 5_512,
            (_, flavors::SoundRate::_11KHZ) => 11_025,
            (_, flavors::SoundRate::_22KHZ) => 22_050,
            (_, flavors::SoundRate::_44KHZ) => 44_100,
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
            aac_sequence_header: aac_sequence_header.clone(),
        }
    }

    fn update_with_metadata(&mut self, metadata: &Metadata) -> bool {
        if self.bitrate != metadata.audio_bitrate {
            self.bitrate = metadata.audio_bitrate;
            true
        } else {
            false
        }
    }

    fn to_caps(&self) -> Option<gst::Caps> {
        let mut caps = match self.format {
            flavors::SoundFormat::MP3 | flavors::SoundFormat::MP3_8KHZ => Some(
                gst::Caps::new_simple("audio/mpeg", &[("mpegversion", &1i32), ("layer", &3i32)]),
            ),
            flavors::SoundFormat::PCM_NE | flavors::SoundFormat::PCM_LE => {
                if self.rate != 0 && self.channels != 0 {
                    // Assume little-endian for "PCM_NE", it's probably more common and we have no
                    // way to know what the endianness of the system creating the stream was
                    Some(gst::Caps::new_simple(
                        "audio/x-raw",
                        &[
                            ("layout", &"interleaved"),
                            ("format", &if self.width == 8 { "U8" } else { "S16LE" }),
                        ],
                    ))
                } else {
                    None
                }
            }
            flavors::SoundFormat::ADPCM => Some(gst::Caps::new_simple(
                "audio/x-adpcm",
                &[("layout", &"swf")],
            )),
            flavors::SoundFormat::NELLYMOSER_16KHZ_MONO
            | flavors::SoundFormat::NELLYMOSER_8KHZ_MONO
            | flavors::SoundFormat::NELLYMOSER => {
                Some(gst::Caps::new_simple("audio/x-nellymoser", &[]))
            }
            flavors::SoundFormat::PCM_ALAW => Some(gst::Caps::new_simple("audio/x-alaw", &[])),
            flavors::SoundFormat::PCM_ULAW => Some(gst::Caps::new_simple("audio/x-mulaw", &[])),
            flavors::SoundFormat::AAC => self.aac_sequence_header.as_ref().map(|header| {
                gst::Caps::new_simple(
                    "audio/mpeg",
                    &[
                        ("mpegversion", &4i32),
                        ("framed", &true),
                        ("stream-format", &"raw"),
                        ("codec_data", &header),
                    ],
                )
            }),
            flavors::SoundFormat::SPEEX => {
                let header = {
                    let header_size = 80;
                    let mut data = Cursor::new(Vec::with_capacity(header_size));
                    data.write_all(b"Speex   1.1.12").unwrap();
                    data.write_all(&[0; 14]).unwrap();
                    data.write_u32le(1).unwrap(); // version
                    data.write_u32le(80).unwrap(); // header size
                    data.write_u32le(16_000).unwrap(); // sample rate
                    data.write_u32le(1).unwrap(); // mode = wideband
                    data.write_u32le(4).unwrap(); // mode bitstream version
                    data.write_u32le(1).unwrap(); // channels
                    data.write_i32le(-1).unwrap(); // bitrate
                    data.write_u32le(0x50).unwrap(); // frame size
                    data.write_u32le(0).unwrap(); // VBR
                    data.write_u32le(1).unwrap(); // frames per packet
                    data.write_u32le(0).unwrap(); // extra headers
                    data.write_u32le(0).unwrap(); // reserved 1
                    data.write_u32le(0).unwrap(); // reserved 2

                    assert_eq!(data.position() as usize, header_size);

                    data.into_inner()
                };
                let header = gst::Buffer::from_mut_slice(header).unwrap();

                let comment = {
                    let comment_size = 4 + 7 /* nothing */ + 4 + 1;
                    let mut data = Cursor::new(Vec::with_capacity(comment_size));
                    data.write_u32le(7).unwrap(); // length of "nothing"
                    data.write_all(b"nothing").unwrap(); // "vendor" string
                    data.write_u32le(0).unwrap(); // number of elements
                    data.write_u8(1).unwrap();

                    assert_eq!(data.position() as usize, comment_size);

                    data.into_inner()
                };
                let comment = gst::Buffer::from_mut_slice(comment).unwrap();

                Some(gst::Caps::new_simple(
                    "audio/x-speex",
                    &[("streamheader", &gst::Array::new(&[&header, &comment]))],
                ))
            }
            flavors::SoundFormat::DEVICE_SPECIFIC => {
                // Nobody knows
                None
            }
        };

        if self.rate != 0 {
            if let Some(ref mut caps) = caps.as_mut() {
                caps.get_mut()
                    .unwrap()
                    .set_simple(&[("rate", &(self.rate as i32))])
            }
        }
        if self.channels != 0 {
            if let Some(ref mut caps) = caps.as_mut() {
                caps.get_mut()
                    .unwrap()
                    .set_simple(&[("channels", &(self.channels as i32))])
            }
        }

        caps
    }
}

#[derive(Debug, Eq, Clone)]
struct VideoFormat {
    format: flavors::CodecId,
    width: Option<u32>,
    height: Option<u32>,
    pixel_aspect_ratio: Option<Rational32>,
    framerate: Option<Rational32>,
    bitrate: Option<u32>,
    avc_sequence_header: Option<gst::Buffer>,
}

impl VideoFormat {
    fn new(
        data_header: &flavors::VideoDataHeader,
        metadata: &Option<Metadata>,
        avc_sequence_header: &Option<gst::Buffer>,
    ) -> VideoFormat {
        VideoFormat {
            format: data_header.codec_id,
            width: metadata.as_ref().and_then(|m| m.video_width),
            height: metadata.as_ref().and_then(|m| m.video_height),
            pixel_aspect_ratio: metadata.as_ref().and_then(|m| m.video_pixel_aspect_ratio),
            framerate: metadata.as_ref().and_then(|m| m.video_framerate),
            bitrate: metadata.as_ref().and_then(|m| m.video_bitrate),
            avc_sequence_header: avc_sequence_header.clone(),
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

    fn to_caps(&self) -> Option<gst::Caps> {
        let mut caps = match self.format {
            flavors::CodecId::SORENSON_H263 => Some(gst::Caps::new_simple(
                "video/x-flash-video",
                &[("flvversion", &1i32)],
            )),
            flavors::CodecId::SCREEN => Some(gst::Caps::new_simple("video/x-flash-screen", &[])),
            flavors::CodecId::VP6 => Some(gst::Caps::new_simple("video/x-vp6-flash", &[])),
            flavors::CodecId::VP6A => Some(gst::Caps::new_simple("video/x-vp6-flash-alpha", &[])),
            flavors::CodecId::SCREEN2 => Some(gst::Caps::new_simple("video/x-flash-screen2", &[])),
            flavors::CodecId::H264 => self.avc_sequence_header.as_ref().map(|header| {
                gst::Caps::new_simple(
                    "video/x-h264",
                    &[("stream-format", &"avc"), ("codec_data", &header)],
                )
            }),
            flavors::CodecId::H263 => Some(gst::Caps::new_simple("video/x-h263", &[])),
            flavors::CodecId::MPEG4Part2 => Some(gst::Caps::new_simple(
                "video/x-h263",
                &[("mpegversion", &4i32), ("systemstream", &false)],
            )),
            flavors::CodecId::JPEG => {
                // Unused according to spec
                None
            }
        };

        if let (Some(width), Some(height)) = (self.width, self.height) {
            if let Some(ref mut caps) = caps.as_mut() {
                caps.get_mut()
                    .unwrap()
                    .set_simple(&[("width", &(width as i32)), ("height", &(height as i32))])
            }
        }

        if let Some(par) = self.pixel_aspect_ratio {
            if *par.numer() != 0 && par.numer() != par.denom() {
                if let Some(ref mut caps) = caps.as_mut() {
                    caps.get_mut().unwrap().set_simple(&[(
                        "pixel-aspect-ratio",
                        &gst::Fraction::new(*par.numer(), *par.denom()),
                    )])
                }
            }
        }

        if let Some(fps) = self.framerate {
            if *fps.numer() != 0 {
                if let Some(ref mut caps) = caps.as_mut() {
                    caps.get_mut().unwrap().set_simple(&[(
                        "framerate",
                        &gst::Fraction::new(*fps.numer(), *fps.denom()),
                    )])
                }
            }
        }

        caps
    }
}

// Ignores bitrate
impl PartialEq for VideoFormat {
    fn eq(&self, other: &Self) -> bool {
        self.format.eq(&other.format)
            && self.width.eq(&other.width)
            && self.height.eq(&other.height)
            && self.pixel_aspect_ratio.eq(&other.pixel_aspect_ratio)
            && self.framerate.eq(&other.framerate)
            && self.avc_sequence_header.eq(&other.avc_sequence_header)
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct Metadata {
    duration: gst::ClockTime,

    creation_date: Option<String>,
    creator: Option<String>,
    title: Option<String>,
    metadata_creator: Option<String>, /* TODO: seek_table: _,
                                       * filepositions / times metadata arrays */

    audio_bitrate: Option<u32>,

    video_width: Option<u32>,
    video_height: Option<u32>,
    video_pixel_aspect_ratio: Option<Rational32>,
    video_framerate: Option<Rational32>,
    video_bitrate: Option<u32>,
}

impl Metadata {
    fn new(script_data: &flavors::ScriptData) -> Metadata {
        assert_eq!(script_data.name, "onMetaData");

        let mut metadata = Metadata {
            duration: gst::CLOCK_TIME_NONE,
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
            flavors::ScriptDataValue::Object(ref objects)
            | flavors::ScriptDataValue::ECMAArray(ref objects) => objects,
            _ => return metadata,
        };

        let mut par_n = None;
        let mut par_d = None;

        for arg in args {
            match (arg.name, &arg.data) {
                ("duration", &flavors::ScriptDataValue::Number(duration)) => {
                    metadata.duration = ((duration * 1000.0 * 1000.0 * 1000.0) as u64).into();
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
                ("framerate", &flavors::ScriptDataValue::Number(framerate)) if framerate >= 0.0 => {
                    if let Some(framerate) = Rational32::approximate_float(framerate) {
                        metadata.video_framerate = Some(framerate);
                    }
                }
                ("AspectRatioX", &flavors::ScriptDataValue::Number(par_x)) if par_x > 0.0 => {
                    par_n = Some(par_x as i32);
                }
                ("AspectRatioY", &flavors::ScriptDataValue::Number(par_y)) if par_y > 0.0 => {
                    par_d = Some(par_y as i32);
                }
                ("videodatarate", &flavors::ScriptDataValue::Number(datarate)) => {
                    metadata.video_bitrate = Some((datarate * 1024.0) as u32);
                }
                _ => {}
            }
        }

        if let (Some(par_n), Some(par_d)) = (par_n, par_d) {
            metadata.video_pixel_aspect_ratio = Some(Rational32::new(par_n, par_d));
        }

        metadata
    }
}

pub struct FlvDemux {
    cat: gst::DebugCategory,
    state: State,
    adapter: Adapter,
    // Only in >= State::Streaming
    streaming_state: Option<StreamingState>,
}

impl FlvDemux {
    pub fn new(_demuxer: &Element) -> FlvDemux {
        FlvDemux {
            cat: gst::DebugCategory::new(
                "rsflvdemux",
                gst::DebugColorFlags::empty(),
                "Rust FLV demuxer",
            ),
            state: State::Stopped,
            adapter: Adapter::new(),
            streaming_state: None,
        }
    }

    pub fn new_boxed(demuxer: &Element) -> Box<DemuxerImpl> {
        Box::new(Self::new(demuxer))
    }

    fn handle_script_tag(
        &mut self,
        demuxer: &Element,
        tag_header: &flavors::TagHeader,
    ) -> Result<HandleBufferResult, FlowError> {
        if self.adapter.get_available() < (15 + tag_header.data_size) as usize {
            return Ok(HandleBufferResult::NeedMoreData);
        }

        self.adapter.flush(15).unwrap();

        let buffer = self
            .adapter
            .get_buffer(tag_header.data_size as usize)
            .unwrap();
        let map = buffer.map_readable().unwrap();
        let data = map.as_slice();

        match flavors::script_data(data) {
            IResult::Done(_, ref script_data) if script_data.name == "onMetaData" => {
                gst_trace!(self.cat, obj: demuxer, "Got script tag: {:?}", script_data);

                let metadata = Metadata::new(script_data);
                gst_debug!(self.cat, obj: demuxer, "Got metadata: {:?}", metadata);

                let streaming_state = self.streaming_state.as_mut().unwrap();

                let audio_changed = streaming_state
                    .audio
                    .as_mut()
                    .map(|a| a.update_with_metadata(&metadata))
                    .unwrap_or(false);
                let video_changed = streaming_state
                    .video
                    .as_mut()
                    .map(|v| v.update_with_metadata(&metadata))
                    .unwrap_or(false);
                streaming_state.metadata = Some(metadata);

                if audio_changed || video_changed {
                    let mut streams = Vec::new();

                    if audio_changed {
                        if let Some(caps) = streaming_state.audio.as_ref().and_then(|a| a.to_caps())
                        {
                            streams.push(Stream::new(AUDIO_STREAM_ID, caps, String::from("audio")));
                        }
                    }
                    if video_changed {
                        if let Some(caps) = streaming_state.video.as_ref().and_then(|v| v.to_caps())
                        {
                            streams.push(Stream::new(VIDEO_STREAM_ID, caps, String::from("video")));
                        }
                    }

                    return Ok(HandleBufferResult::StreamsChanged(streams));
                }
            }
            IResult::Done(_, ref script_data) => {
                gst_trace!(self.cat, obj: demuxer, "Got script tag: {:?}", script_data);
            }
            IResult::Error(_) | IResult::Incomplete(_) => {
                // ignore
            }
        }

        Ok(HandleBufferResult::Again)
    }

    fn update_audio_stream(
        &mut self,
        demuxer: &Element,
        data_header: &flavors::AudioDataHeader,
    ) -> Result<HandleBufferResult, FlowError> {
        gst_trace!(
            self.cat,
            obj: demuxer,
            "Got audio data header: {:?}",
            data_header
        );

        let streaming_state = self.streaming_state.as_mut().unwrap();

        let new_audio_format = AudioFormat::new(
            data_header,
            &streaming_state.metadata,
            &streaming_state.aac_sequence_header,
        );

        if streaming_state.audio.as_ref() != Some(&new_audio_format) {
            gst_debug!(
                self.cat,
                obj: demuxer,
                "Got new audio format: {:?}",
                new_audio_format
            );
            let new_stream = streaming_state.audio == None;

            let caps = new_audio_format.to_caps();
            if let Some(caps) = caps {
                streaming_state.audio = Some(new_audio_format);
                let stream = Stream::new(AUDIO_STREAM_ID, caps, String::from("audio"));
                if new_stream {
                    return Ok(HandleBufferResult::StreamAdded(stream));
                } else {
                    return Ok(HandleBufferResult::StreamChanged(stream));
                }
            } else {
                streaming_state.audio = None;
            }
        }

        if !streaming_state.got_all_streams && streaming_state.audio != None
            && (streaming_state.expect_video && streaming_state.video != None
                || !streaming_state.expect_video)
        {
            streaming_state.got_all_streams = true;
            return Ok(HandleBufferResult::HaveAllStreams);
        }

        Ok(HandleBufferResult::Again)
    }

    fn handle_audio_tag(
        &mut self,
        demuxer: &Element,
        tag_header: &flavors::TagHeader,
        data_header: &flavors::AudioDataHeader,
    ) -> Result<HandleBufferResult, FlowError> {
        let res = self.update_audio_stream(demuxer, data_header);
        match res {
            Ok(HandleBufferResult::Again) => (),
            _ => return res,
        }

        if self.adapter.get_available() < (15 + tag_header.data_size) as usize {
            return Ok(HandleBufferResult::NeedMoreData);
        }

        // AAC special case
        if data_header.sound_format == flavors::SoundFormat::AAC {
            // Not big enough for the AAC packet header, ship!
            if tag_header.data_size < 1 + 1 {
                self.adapter
                    .flush(15 + tag_header.data_size as usize)
                    .unwrap();
                gst_warning!(
                    self.cat,
                    obj: demuxer,
                    "Too small packet for AAC packet header {}",
                    15 + tag_header.data_size
                );
                return Ok(HandleBufferResult::Again);
            }

            let mut data = [0u8; 17];
            self.adapter.peek_into(&mut data).unwrap();
            match flavors::aac_audio_packet_header(&data[16..]) {
                IResult::Error(_) | IResult::Incomplete(_) => {
                    unimplemented!();
                }
                IResult::Done(_, header) => {
                    gst_trace!(self.cat, obj: demuxer, "Got AAC packet header {:?}", header);
                    match header.packet_type {
                        flavors::AACPacketType::SequenceHeader => {
                            self.adapter.flush(15 + 1 + 1).unwrap();
                            let buffer = self
                                .adapter
                                .get_buffer((tag_header.data_size - 1 - 1) as usize)
                                .unwrap();
                            gst_debug!(
                                self.cat,
                                obj: demuxer,
                                "Got AAC sequence header {:?} of size {}",
                                buffer,
                                tag_header.data_size - 1 - 1
                            );

                            let streaming_state = self.streaming_state.as_mut().unwrap();
                            streaming_state.aac_sequence_header = Some(buffer);
                            return Ok(HandleBufferResult::Again);
                        }
                        flavors::AACPacketType::Raw => {
                            // fall through
                        }
                    }
                }
            }
        }

        let streaming_state = self.streaming_state.as_ref().unwrap();

        if streaming_state.audio == None {
            self.adapter
                .flush((tag_header.data_size + 15) as usize)
                .unwrap();
            return Ok(HandleBufferResult::Again);
        }

        let audio = streaming_state.audio.as_ref().unwrap();
        self.adapter.flush(16).unwrap();

        let offset = match audio.format {
            flavors::SoundFormat::AAC => 1,
            _ => 0,
        };

        if tag_header.data_size == 0 {
            return Ok(HandleBufferResult::Again);
        }

        if tag_header.data_size < offset {
            self.adapter
                .flush((tag_header.data_size - 1) as usize)
                .unwrap();
            return Ok(HandleBufferResult::Again);
        }

        if offset > 0 {
            self.adapter.flush(offset as usize).unwrap();
        }

        let mut buffer = self
            .adapter
            .get_buffer((tag_header.data_size - 1 - offset) as usize)
            .unwrap();

        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_mseconds(tag_header.timestamp as u64));
        }

        gst_trace!(
            self.cat,
            obj: demuxer,
            "Outputting audio buffer {:?} for tag {:?} of size {}",
            buffer,
            tag_header,
            tag_header.data_size - 1
        );

        Ok(HandleBufferResult::BufferForStream(AUDIO_STREAM_ID, buffer))
    }

    fn update_video_stream(
        &mut self,
        demuxer: &Element,
        data_header: &flavors::VideoDataHeader,
    ) -> Result<HandleBufferResult, FlowError> {
        gst_trace!(
            self.cat,
            obj: demuxer,
            "Got video data header: {:?}",
            data_header
        );

        let streaming_state = self.streaming_state.as_mut().unwrap();

        let new_video_format = VideoFormat::new(
            data_header,
            &streaming_state.metadata,
            &streaming_state.avc_sequence_header,
        );

        if streaming_state.video.as_ref() != Some(&new_video_format) {
            gst_debug!(
                self.cat,
                obj: demuxer,
                "Got new video format: {:?}",
                new_video_format
            );

            let new_stream = streaming_state.video == None;

            let caps = new_video_format.to_caps();
            if let Some(caps) = caps {
                streaming_state.video = Some(new_video_format);
                let stream = Stream::new(VIDEO_STREAM_ID, caps, String::from("video"));
                if new_stream {
                    return Ok(HandleBufferResult::StreamAdded(stream));
                } else {
                    return Ok(HandleBufferResult::StreamChanged(stream));
                }
            } else {
                streaming_state.video = None;
            }
        }

        if !streaming_state.got_all_streams && streaming_state.video != None
            && (streaming_state.expect_audio && streaming_state.audio != None
                || !streaming_state.expect_audio)
        {
            streaming_state.got_all_streams = true;
            return Ok(HandleBufferResult::HaveAllStreams);
        }

        Ok(HandleBufferResult::Again)
    }

    fn handle_video_tag(
        &mut self,
        demuxer: &Element,
        tag_header: &flavors::TagHeader,
        data_header: &flavors::VideoDataHeader,
    ) -> Result<HandleBufferResult, FlowError> {
        let res = self.update_video_stream(demuxer, data_header);
        match res {
            Ok(HandleBufferResult::Again) => (),
            _ => return res,
        }

        if self.adapter.get_available() < (15 + tag_header.data_size) as usize {
            return Ok(HandleBufferResult::NeedMoreData);
        }

        let mut cts = 0;

        // AVC/H264 special case
        if data_header.codec_id == flavors::CodecId::H264 {
            // Not big enough for the AVC packet header, ship!
            if tag_header.data_size < 1 + 4 {
                self.adapter
                    .flush(15 + tag_header.data_size as usize)
                    .unwrap();
                gst_warning!(
                    self.cat,
                    obj: demuxer,
                    "Too small packet for AVC packet header {}",
                    15 + tag_header.data_size
                );
                return Ok(HandleBufferResult::Again);
            }

            let mut data = [0u8; 20];
            self.adapter.peek_into(&mut data).unwrap();
            match flavors::avc_video_packet_header(&data[16..]) {
                IResult::Error(_) | IResult::Incomplete(_) => {
                    unimplemented!();
                }
                IResult::Done(_, header) => {
                    gst_trace!(self.cat, obj: demuxer, "Got AVC packet header {:?}", header);
                    match header.packet_type {
                        flavors::AVCPacketType::SequenceHeader => {
                            self.adapter.flush(15 + 1 + 4).unwrap();
                            let buffer = self
                                .adapter
                                .get_buffer((tag_header.data_size - 1 - 4) as usize)
                                .unwrap();
                            gst_debug!(
                                self.cat,
                                obj: demuxer,
                                "Got AVC sequence header {:?} of size {}",
                                buffer,
                                tag_header.data_size - 1 - 4
                            );

                            let streaming_state = self.streaming_state.as_mut().unwrap();
                            streaming_state.avc_sequence_header = Some(buffer);
                            return Ok(HandleBufferResult::Again);
                        }
                        flavors::AVCPacketType::NALU => {
                            cts = header.composition_time;
                        }
                        flavors::AVCPacketType::EndOfSequence => {
                            // Skip
                            self.adapter
                                .flush(15 + tag_header.data_size as usize)
                                .unwrap();
                            return Ok(HandleBufferResult::Again);
                        }
                    }
                }
            }
        }

        let streaming_state = self.streaming_state.as_ref().unwrap();

        if streaming_state.video == None {
            self.adapter
                .flush((tag_header.data_size + 15) as usize)
                .unwrap();
            return Ok(HandleBufferResult::Again);
        }

        let video = streaming_state.video.as_ref().unwrap();
        let is_keyframe = data_header.frame_type == flavors::FrameType::Key;

        self.adapter.flush(16).unwrap();

        let offset = match video.format {
            flavors::CodecId::VP6 | flavors::CodecId::VP6A => 1,
            flavors::CodecId::H264 => 4,
            _ => 0,
        };

        if tag_header.data_size == 0 {
            return Ok(HandleBufferResult::Again);
        }

        if tag_header.data_size < offset {
            self.adapter
                .flush((tag_header.data_size - 1) as usize)
                .unwrap();
            return Ok(HandleBufferResult::Again);
        }

        if offset > 0 {
            self.adapter.flush(offset as usize).unwrap();
        }

        let mut buffer = self
            .adapter
            .get_buffer((tag_header.data_size - 1 - offset) as usize)
            .unwrap();

        {
            let buffer = buffer.get_mut().unwrap();
            if !is_keyframe {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
            buffer.set_dts(gst::ClockTime::from_mseconds(tag_header.timestamp as u64));

            // Prevent negative numbers
            let pts = if cts < 0 && tag_header.timestamp < (-cts) as u32 {
                0
            } else {
                ((tag_header.timestamp as i64) + (cts as i64)) as u64
            };
            buffer.set_pts(gst::ClockTime::from_mseconds(pts));
        }

        gst_trace!(
            self.cat,
            obj: demuxer,
            "Outputting video buffer {:?} for tag {:?} of size {}, keyframe: {}",
            buffer,
            tag_header,
            tag_header.data_size - 1 - offset,
            is_keyframe
        );

        Ok(HandleBufferResult::BufferForStream(VIDEO_STREAM_ID, buffer))
    }

    fn update_state(&mut self, demuxer: &Element) -> Result<HandleBufferResult, FlowError> {
        match self.state {
            State::Stopped => unreachable!(),
            State::NeedHeader => {
                while self.adapter.get_available() >= 9 {
                    let mut data = [0u8; 9];
                    self.adapter.peek_into(&mut data).unwrap();

                    match flavors::header(&data) {
                        IResult::Error(_) | IResult::Incomplete(_) => {
                            // fall through
                        }
                        IResult::Done(_, ref header) => {
                            gst_debug!(self.cat, obj: demuxer, "Found FLV header: {:?}", header);

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
            State::Skipping {
                audio,
                video,
                skip_left: 0,
            } => {
                self.state = State::Streaming;
                self.streaming_state = Some(StreamingState::new(audio, video));

                Ok(HandleBufferResult::Again)
            }
            State::Skipping {
                ref mut skip_left, ..
            } => {
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
                    IResult::Error(_) | IResult::Incomplete(_) => {
                        unimplemented!();
                    }
                    IResult::Done(_, previous_size) => {
                        gst_trace!(
                            self.cat,
                            obj: demuxer,
                            "Previous tag size {}",
                            previous_size
                        );
                        // Nothing to do here, we just consume it for now
                    }
                }

                let tag_header = match flavors::tag_header(&data[4..]) {
                    IResult::Error(_) | IResult::Incomplete(_) => unimplemented!(),
                    IResult::Done(_, tag_header) => tag_header,
                };

                let res = match tag_header.tag_type {
                    flavors::TagType::Script => {
                        gst_trace!(self.cat, obj: demuxer, "Found script tag");

                        self.handle_script_tag(demuxer, &tag_header)
                    }
                    flavors::TagType::Audio => {
                        gst_trace!(self.cat, obj: demuxer, "Found audio tag");

                        let data_header = match flavors::audio_data_header(&data[15..]) {
                            IResult::Error(_) | IResult::Incomplete(_) => unimplemented!(),
                            IResult::Done(_, data_header) => data_header,
                        };

                        self.handle_audio_tag(demuxer, &tag_header, &data_header)
                    }
                    flavors::TagType::Video => {
                        gst_trace!(self.cat, obj: demuxer, "Found video tag");

                        let data_header = match flavors::video_data_header(&data[15..]) {
                            IResult::Error(_) | IResult::Incomplete(_) => unimplemented!(),
                            IResult::Done(_, data_header) => data_header,
                        };

                        self.handle_video_tag(demuxer, &tag_header, &data_header)
                    }
                };

                if let Ok(HandleBufferResult::BufferForStream(_, ref buffer)) = res {
                    let streaming_state = self.streaming_state.as_mut().unwrap();

                    if buffer.get_pts() != gst::CLOCK_TIME_NONE {
                        let pts = buffer.get_pts();
                        streaming_state.last_position = streaming_state
                            .last_position
                            .map(|last| cmp::max(last.into(), pts))
                            .unwrap_or(pts);
                    } else if buffer.get_dts() != gst::CLOCK_TIME_NONE {
                        let dts = buffer.get_dts();
                        streaming_state.last_position = streaming_state
                            .last_position
                            .map(|last| cmp::max(last.into(), dts))
                            .unwrap_or(dts);
                    }
                }

                res
            }
        }
    }
}

impl DemuxerImpl for FlvDemux {
    fn start(
        &mut self,
        _demuxer: &Element,
        _upstream_size: Option<u64>,
        _random_access: bool,
    ) -> Result<(), gst::ErrorMessage> {
        self.state = State::NeedHeader;

        Ok(())
    }

    fn stop(&mut self, _demuxer: &Element) -> Result<(), gst::ErrorMessage> {
        self.state = State::Stopped;
        self.adapter.clear();
        self.streaming_state = None;

        Ok(())
    }

    fn seek(
        &mut self,
        _demuxer: &Element,
        _start: gst::ClockTime,
        _stop: gst::ClockTime,
    ) -> Result<SeekResult, gst::ErrorMessage> {
        unimplemented!();
    }

    fn handle_buffer(
        &mut self,
        demuxer: &Element,
        buffer: Option<gst::Buffer>,
    ) -> Result<HandleBufferResult, FlowError> {
        if let Some(buffer) = buffer {
            self.adapter.push(buffer);
        }

        self.update_state(demuxer)
    }

    fn end_of_stream(&mut self, _demuxer: &Element) -> Result<(), gst::ErrorMessage> {
        // nothing to do here, all data we have left is incomplete
        Ok(())
    }

    fn is_seekable(&self, _demuxer: &Element) -> bool {
        false
    }

    fn get_position(&self, _demuxer: &Element) -> gst::ClockTime {
        if let Some(StreamingState { last_position, .. }) = self.streaming_state {
            return last_position;
        }

        gst::CLOCK_TIME_NONE
    }

    fn get_duration(&self, _demuxer: &Element) -> gst::ClockTime {
        if let Some(StreamingState {
            metadata: Some(Metadata { duration, .. }),
            ..
        }) = self.streaming_state
        {
            return duration;
        }

        gst::CLOCK_TIME_NONE
    }
}
