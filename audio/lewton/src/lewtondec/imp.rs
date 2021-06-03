// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_warning};
use gst_audio::audio_decoder_error;
use gst_audio::prelude::*;
use gst_audio::subclass::prelude::*;

use atomic_refcell::AtomicRefCell;

use byte_slice_cast::*;

use once_cell::sync::Lazy;

struct State {
    header_bufs: (
        Option<gst::Buffer>,
        Option<gst::Buffer>,
        Option<gst::Buffer>,
    ),
    headerset: Option<lewton::header::HeaderSet>,
    pwr: lewton::audio::PreviousWindowRight,
    audio_info: Option<gst_audio::AudioInfo>,
    reorder_map: Option<[usize; 8]>,
}

#[derive(Default)]
pub struct LewtonDec {
    state: AtomicRefCell<Option<State>>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "lewtondec",
        gst::DebugColorFlags::empty(),
        Some("lewton Vorbis decoder"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for LewtonDec {
    const NAME: &'static str = "LewtonDec";
    type Type = super::LewtonDec;
    type ParentType = gst_audio::AudioDecoder;
}

impl ObjectImpl for LewtonDec {}

impl ElementImpl for LewtonDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "lewton Vorbis decoder",
                "Decoder/Audio",
                "lewton Vorbis decoder",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let sink_caps = gst::Caps::new_simple("audio/x-vorbis", &[]);
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::new_simple(
                "audio/x-raw",
                &[
                    ("format", &gst_audio::AUDIO_FORMAT_F32.to_str()),
                    ("rate", &gst::IntRange::<i32>::new(1, std::i32::MAX)),
                    ("channels", &gst::IntRange::<i32>::new(1, 255)),
                    ("layout", &"interleaved"),
                ],
            );
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl AudioDecoderImpl for LewtonDec {
    fn stop(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = None;

        Ok(())
    }

    fn start(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = Some(State {
            header_bufs: (None, None, None),
            headerset: None,
            pwr: lewton::audio::PreviousWindowRight::new(),
            audio_info: None,
            reorder_map: None,
        });

        Ok(())
    }

    fn set_format(&self, element: &Self::Type, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst_debug!(CAT, obj: element, "Setting format {:?}", caps);

        // When the caps are changing we require new headers
        let mut state_guard = self.state.borrow_mut();
        *state_guard = Some(State {
            header_bufs: (None, None, None),
            headerset: None,
            pwr: lewton::audio::PreviousWindowRight::new(),
            audio_info: None,
            reorder_map: None,
        });

        let mut state = state_guard.as_mut().unwrap();

        let s = caps.structure(0).unwrap();
        if let Ok(Some(streamheaders)) = s.get_optional::<gst::Array>("streamheader") {
            let streamheaders = streamheaders.as_slice();
            if streamheaders.len() < 3 {
                gst_debug!(
                    CAT,
                    obj: element,
                    "Not enough streamheaders, trying in-band"
                );
                return Ok(());
            }

            let ident_buf = streamheaders[0].get::<Option<gst::Buffer>>();
            let comment_buf = streamheaders[1].get::<Option<gst::Buffer>>();
            let setup_buf = streamheaders[2].get::<Option<gst::Buffer>>();
            if let (Ok(Some(ident_buf)), Ok(Some(comment_buf)), Ok(Some(setup_buf))) =
                (ident_buf, comment_buf, setup_buf)
            {
                gst_debug!(CAT, obj: element, "Got streamheader buffers");
                state.header_bufs = (Some(ident_buf), Some(comment_buf), Some(setup_buf));
            }
        }

        Ok(())
    }

    fn flush(&self, element: &Self::Type, _hard: bool) {
        gst_debug!(CAT, obj: element, "Flushing");

        let mut state_guard = self.state.borrow_mut();
        if let Some(ref mut state) = *state_guard {
            state.pwr = lewton::audio::PreviousWindowRight::new();
        }
    }

    fn handle_frame(
        &self,
        element: &Self::Type,
        inbuf: Option<&gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_debug!(CAT, obj: element, "Handling buffer {:?}", inbuf);

        let inbuf = match inbuf {
            None => return Ok(gst::FlowSuccess::Ok),
            Some(inbuf) => inbuf,
        };

        let inmap = inbuf.map_readable().map_err(|_| {
            gst_error!(CAT, obj: element, "Failed to buffer readable");
            gst::FlowError::Error
        })?;

        let mut state_guard = self.state.borrow_mut();
        let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        // Ignore empty packets unless we have no headers yet
        if inmap.len() == 0 {
            element.finish_frame(None, 1)?;

            if state.headerset.is_some() {
                return Ok(gst::FlowSuccess::Ok);
            } else {
                gst_error!(CAT, obj: element, "Got empty packet before all headers");
                return Err(gst::FlowError::Error);
            }
        }

        // If this is a header packet then handle it
        if inmap[0] & 0x01 == 0x01 {
            return self.handle_header(element, state, inbuf, inmap.as_ref());
        }

        // If it's a data packet then try to initialize the headerset now if we didn't yet
        if state.headerset.is_none() {
            self.initialize(element, state)?;
        }

        self.handle_data(element, state, inmap.as_ref())
    }
}

impl LewtonDec {
    fn handle_header(
        &self,
        element: &super::LewtonDec,
        state: &mut State,
        inbuf: &gst::Buffer,
        indata: &[u8],
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // ident header
        if indata[0] == 0x01 {
            gst_debug!(CAT, obj: element, "Got ident header buffer");
            state.header_bufs = (Some(inbuf.clone()), None, None);
        } else if indata[0] == 0x03 {
            // comment header
            if state.header_bufs.0.is_none() {
                gst_warning!(CAT, obj: element, "Got comment header before ident header");
            } else {
                gst_debug!(CAT, obj: element, "Got comment header buffer");
                state.header_bufs.1 = Some(inbuf.clone());
            }
        } else if indata[0] == 0x05 {
            // setup header
            if state.header_bufs.0.is_none() || state.header_bufs.1.is_none() {
                gst_warning!(
                    CAT,
                    obj: element,
                    "Got setup header before ident/comment header"
                );
            } else {
                gst_debug!(CAT, obj: element, "Got setup header buffer");
                state.header_bufs.2 = Some(inbuf.clone());
            }
        }

        element.finish_frame(None, 1)
    }

    fn initialize(
        &self,
        element: &super::LewtonDec,
        state: &mut State,
    ) -> Result<(), gst::FlowError> {
        let (ident_buf, comment_buf, setup_buf) = match state.header_bufs {
            (Some(ref ident_buf), Some(ref comment_buf), Some(ref setup_buf)) => {
                (ident_buf, comment_buf, setup_buf)
            }
            _ => {
                gst::element_error!(
                    element,
                    gst::StreamError::Decode,
                    ["Got no headers before data packets"]
                );
                return Err(gst::FlowError::NotNegotiated);
            }
        };

        // First try to parse the headers
        let ident_map = ident_buf.map_readable().map_err(|_| {
            gst_error!(CAT, obj: element, "Failed to map ident buffer readable");
            gst::FlowError::Error
        })?;
        let ident = lewton::header::read_header_ident(ident_map.as_ref()).map_err(|err| {
            gst::element_error!(
                element,
                gst::StreamError::Decode,
                ["Failed to parse ident header: {:?}", err]
            );
            gst::FlowError::Error
        })?;

        let comment_map = comment_buf.map_readable().map_err(|_| {
            gst_error!(CAT, obj: element, "Failed to map comment buffer readable");
            gst::FlowError::Error
        })?;
        let comment = lewton::header::read_header_comment(comment_map.as_ref()).map_err(|err| {
            gst::element_error!(
                element,
                gst::StreamError::Decode,
                ["Failed to parse comment header: {:?}", err]
            );
            gst::FlowError::Error
        })?;

        let setup_map = setup_buf.map_readable().map_err(|_| {
            gst_error!(CAT, obj: element, "Failed to map setup buffer readable");
            gst::FlowError::Error
        })?;
        let setup = lewton::header::read_header_setup(
            setup_map.as_ref(),
            ident.audio_channels,
            (ident.blocksize_0, ident.blocksize_1),
        )
        .map_err(|err| {
            gst::element_error!(
                element,
                gst::StreamError::Decode,
                ["Failed to parse setup header: {:?}", err]
            );
            gst::FlowError::Error
        })?;

        let mut audio_info = gst_audio::AudioInfo::builder(
            gst_audio::AUDIO_FORMAT_F32,
            ident.audio_sample_rate,
            ident.audio_channels as u32,
        );

        // For 1-8 channels there are defined channel positions, so initialize
        // everything accordingly and set the channel positions in the output
        // format
        let mut reorder_map = None;
        if ident.audio_channels > 1 && ident.audio_channels < 9 {
            let channels = ident.audio_channels as usize;
            let from = &VORBIS_CHANNEL_POSITIONS[channels - 1][..channels];
            let to = &GST_VORBIS_CHANNEL_POSITIONS[channels - 1][..channels];

            audio_info = audio_info.positions(to);

            let mut map = [0; 8];
            if gst_audio::channel_reorder_map(from, to, &mut map[..channels]).is_err() {
                gst_error!(
                    CAT,
                    obj: element,
                    "Failed to generate channel reorder map from {:?} to {:?}",
                    from,
                    to,
                );
            } else {
                // If the reorder map is not the identity matrix
                if !map[..channels].iter().enumerate().all(|(c1, c2)| c1 == *c2) {
                    reorder_map = Some(map);
                }
            }
        }
        let audio_info = audio_info.build().unwrap();

        gst_debug!(
            CAT,
            obj: element,
            "Successfully parsed headers: {:?}",
            audio_info
        );
        state.headerset = Some((ident, comment, setup));
        state.audio_info = Some(audio_info.clone());
        state.reorder_map = reorder_map;

        element.set_output_format(&audio_info)?;
        element.negotiate()?;

        Ok(())
    }

    fn handle_data(
        &self,
        element: &super::LewtonDec,
        state: &mut State,
        indata: &[u8],
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // We ensured above that we have headers here now
        let headerset = state.headerset.as_ref().unwrap();
        let audio_info = state.audio_info.as_ref().unwrap();

        // Decode the input packet
        let decoded = match lewton::audio::read_audio_packet_generic::<
            lewton::samples::InterleavedSamples<f32>,
        >(&headerset.0, &headerset.2, indata, &mut state.pwr)
        {
            Ok(decoded) => decoded,
            Err(err) => {
                return audio_decoder_error!(
                    element,
                    1,
                    gst::StreamError::Decode,
                    ["Failed to decode packet: {:?}", err]
                );
            }
        };

        if decoded.channel_count != audio_info.channels() as usize {
            return audio_decoder_error!(
                element,
                1,
                gst::StreamError::Decode,
                [
                    "Channel count mismatch (got {}, expected {})",
                    decoded.channel_count,
                    audio_info.channels()
                ]
            );
        }

        let sample_count = decoded.samples.len() / audio_info.channels() as usize;
        gst_debug!(CAT, obj: element, "Got {} decoded samples", sample_count);

        if sample_count == 0 {
            return element.finish_frame(None, 1);
        }

        let outbuf = if let Some(ref reorder_map) = state.reorder_map {
            let mut outbuf = element
                .allocate_output_buffer(sample_count as usize * audio_info.bpf() as usize)
                .map_err(|_| {
                    gst::element_error!(
                        element,
                        gst::StreamError::Decode,
                        ["Failed to allocate output buffer"]
                    );
                    gst::FlowError::Error
                })?;
            {
                // And copy the decoded data into our output buffer while reordering the channels to the
                // GStreamer channel order
                let outbuf = outbuf.get_mut().unwrap();
                let mut outmap = outbuf.map_writable().map_err(|_| {
                    gst::element_error!(
                        element,
                        gst::StreamError::Decode,
                        ["Failed to map output buffer writable"]
                    );
                    gst::FlowError::Error
                })?;

                let outdata = outmap.as_mut_slice_of::<f32>().unwrap();
                let channels = audio_info.channels() as usize;
                assert!(reorder_map.len() >= channels);
                assert!(reorder_map[..channels].iter().all(|c| *c < channels));

                for (output, input) in outdata
                    .chunks_exact_mut(channels)
                    .zip(decoded.samples.chunks_exact(channels))
                {
                    for (c, s) in input.iter().enumerate() {
                        output[reorder_map[c]] = *s;
                    }
                }
            }

            outbuf
        } else {
            struct CastVec(Vec<f32>);
            impl AsRef<[u8]> for CastVec {
                fn as_ref(&self) -> &[u8] {
                    self.0.as_byte_slice()
                }
            }
            impl AsMut<[u8]> for CastVec {
                fn as_mut(&mut self) -> &mut [u8] {
                    self.0.as_mut_byte_slice()
                }
            }

            gst::Buffer::from_mut_slice(CastVec(decoded.samples))
        };

        element.finish_frame(Some(outbuf), 1)
    }
}

// http://www.xiph.org/vorbis/doc/Vorbis_I_spec.html#x1-800004.3.9
const VORBIS_CHANNEL_POSITIONS: [[gst_audio::AudioChannelPosition; 8]; 8] = [
    [
        gst_audio::AudioChannelPosition::Mono,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Lfe1,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::SideLeft,
        gst_audio::AudioChannelPosition::SideRight,
        gst_audio::AudioChannelPosition::RearCenter,
        gst_audio::AudioChannelPosition::Lfe1,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::SideLeft,
        gst_audio::AudioChannelPosition::SideRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Lfe1,
    ],
];

const GST_VORBIS_CHANNEL_POSITIONS: [[gst_audio::AudioChannelPosition; 8]; 8] = [
    [
        gst_audio::AudioChannelPosition::Mono,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::Lfe1,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::Lfe1,
        gst_audio::AudioChannelPosition::RearCenter,
        gst_audio::AudioChannelPosition::SideLeft,
        gst_audio::AudioChannelPosition::SideRight,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::Lfe1,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::SideLeft,
        gst_audio::AudioChannelPosition::SideRight,
    ],
];
