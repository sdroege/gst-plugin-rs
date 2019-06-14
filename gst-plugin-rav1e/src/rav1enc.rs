// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::subclass::prelude::*;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;
use gstreamer_video as gst_video;
use rav1e::color;
use rav1e::config;
use rav1e::data;
use std::sync::{Arc, Mutex};

const DEFAULT_SPEED_PRESET: u32 = 5;
const DEFAULT_LOW_LATENCY: bool = false;
const DEFAULT_MIN_KEY_FRAME_INTERVAL: u64 = 12;
const DEFAULT_MAX_KEY_FRAME_INTERVAL: u64 = 240;
const DEFAULT_BITRATE: i32 = 0;
const DEFAULT_QUANTIZER: usize = 100;
const DEFAULT_TILE_COLS_LOG2: usize = 0;
const DEFAULT_TILE_ROWS_LOG2: usize = 0;
const DEFAULT_THREADS: usize = 0;

#[derive(Debug, Clone, Copy)]
struct Settings {
    speed_preset: u32,
    low_latency: bool,
    min_key_frame_interval: u64,
    max_key_frame_interval: u64,
    bitrate: i32,
    quantizer: usize,
    tile_cols_log2: usize,
    tile_rows_log2: usize,
    threads: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            speed_preset: DEFAULT_SPEED_PRESET,
            low_latency: DEFAULT_LOW_LATENCY,
            min_key_frame_interval: DEFAULT_MIN_KEY_FRAME_INTERVAL,
            max_key_frame_interval: DEFAULT_MAX_KEY_FRAME_INTERVAL,
            bitrate: DEFAULT_BITRATE,
            quantizer: DEFAULT_QUANTIZER,
            tile_cols_log2: DEFAULT_TILE_COLS_LOG2,
            tile_rows_log2: DEFAULT_TILE_ROWS_LOG2,
            threads: DEFAULT_THREADS,
        }
    }
}

static PROPERTIES: [subclass::Property; 9] = [
    subclass::Property("speed-preset", |name| {
        glib::ParamSpec::uint(
            name,
            "Speed Preset",
            "Speed preset (10 fastest, 0 slowest)",
            0,
            10,
            DEFAULT_SPEED_PRESET,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("low-latency", |name| {
        glib::ParamSpec::boolean(
            name,
            "Low Latency",
            "Low Latency",
            DEFAULT_LOW_LATENCY,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("min-key-frame-interval", |name| {
        glib::ParamSpec::uint64(
            name,
            "Min Key Frame Interval",
            "Min Key Frame Interval",
            0,
            std::u64::MAX,
            DEFAULT_MIN_KEY_FRAME_INTERVAL,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("max-key-frame-interval", |name| {
        glib::ParamSpec::uint64(
            name,
            "Max Key Frame Interval",
            "Max Key Frame Interval",
            0,
            std::u64::MAX,
            DEFAULT_MAX_KEY_FRAME_INTERVAL,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("bitrate", |name| {
        glib::ParamSpec::int(
            name,
            "Bitrate",
            "Bitrate",
            0,
            std::i32::MAX,
            DEFAULT_BITRATE,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("quantizer", |name| {
        glib::ParamSpec::uint(
            name,
            "Quantizer",
            "Quantizer",
            0,
            std::u32::MAX,
            DEFAULT_QUANTIZER as u32,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("tile-cols-log2", |name| {
        glib::ParamSpec::uint(
            name,
            "Tile Cols Log2",
            "Tile Cols Log2",
            0,
            std::u32::MAX,
            DEFAULT_TILE_COLS_LOG2 as u32,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("tile-rows-log2", |name| {
        glib::ParamSpec::uint(
            name,
            "Tile Rows Log2",
            "Tile Rows Log2",
            0,
            std::u32::MAX,
            DEFAULT_TILE_ROWS_LOG2 as u32,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("threads", |name| {
        glib::ParamSpec::uint(
            name,
            "Threads",
            "Threads",
            0,
            std::u32::MAX,
            DEFAULT_THREADS as u32,
            glib::ParamFlags::READWRITE,
        )
    }),
];

enum Context {
    Eight(rav1e::Context<u8>),
    Sixteen(rav1e::Context<u16>),
}

impl Context {
    fn receive_packet(&mut self) -> Result<(data::FrameType, u64, Vec<u8>), data::EncoderStatus> {
        match self {
            Context::Eight(ref mut context) => context
                .receive_packet()
                .map(|packet| (packet.frame_type, packet.input_frameno, packet.data)),
            Context::Sixteen(ref mut context) => context
                .receive_packet()
                .map(|packet| (packet.frame_type, packet.input_frameno, packet.data)),
        }
    }

    fn send_frame(
        &mut self,
        in_frame: Option<&gst_video::VideoFrameRef<&gst::BufferRef>>,
    ) -> Result<(), data::EncoderStatus> {
        match self {
            Context::Eight(ref mut context) => {
                let enc_frame = in_frame.map(|in_frame| {
                    let mut enc_frame = context.new_frame();
                    let enc_frame_mut = Arc::get_mut(&mut enc_frame)
                        .expect("newly created encoder frame not mutable");
                    enc_frame_mut.planes[0].copy_from_raw_u8(
                        in_frame.plane_data(0).unwrap(),
                        in_frame.plane_stride()[0] as usize,
                        1,
                    );

                    if in_frame.n_planes() > 1 {
                        enc_frame_mut.planes[1].copy_from_raw_u8(
                            in_frame.plane_data(1).unwrap(),
                            in_frame.plane_stride()[1] as usize,
                            1,
                        );
                        enc_frame_mut.planes[2].copy_from_raw_u8(
                            in_frame.plane_data(2).unwrap(),
                            in_frame.plane_stride()[2] as usize,
                            1,
                        );
                    }
                    enc_frame
                });
                context.send_frame(enc_frame)
            }
            Context::Sixteen(ref mut context) => {
                let enc_frame = in_frame.map(|in_frame| {
                    let mut enc_frame = context.new_frame();
                    let enc_frame_mut = Arc::get_mut(&mut enc_frame)
                        .expect("newly created encoder frame not mutable");
                    enc_frame_mut.planes[0].copy_from_raw_u8(
                        in_frame.plane_data(0).unwrap(),
                        in_frame.plane_stride()[0] as usize,
                        2,
                    );

                    if in_frame.n_planes() > 1 {
                        enc_frame_mut.planes[1].copy_from_raw_u8(
                            in_frame.plane_data(1).unwrap(),
                            in_frame.plane_stride()[1] as usize,
                            2,
                        );
                        enc_frame_mut.planes[2].copy_from_raw_u8(
                            in_frame.plane_data(2).unwrap(),
                            in_frame.plane_stride()[2] as usize,
                            2,
                        );
                    }
                    enc_frame
                });
                context.send_frame(enc_frame)
            }
        }
    }

    fn flush(&mut self) {
        match self {
            Context::Eight(ref mut context) => context.flush(),
            Context::Sixteen(ref mut context) => context.flush(),
        }
    }
}

struct State {
    context: Context,
    video_info: gst_video::VideoInfo,
}

struct Rav1Enc {
    cat: gst::DebugCategory,
    state: Mutex<Option<State>>,
    settings: Mutex<Settings>,
}

impl ObjectSubclass for Rav1Enc {
    const NAME: &'static str = "Rav1Enc";
    type ParentType = gst_video::VideoEncoder;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new() -> Self {
        Self {
            cat: gst::DebugCategory::new(
                "rav1enc",
                gst::DebugColorFlags::empty(),
                Some("rav1e AV1 encoder"),
            ),
            state: Mutex::new(None),
            settings: Mutex::new(Default::default()),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "rav1e AV1 encoder",
            "Encoder/Video",
            "rav1e AV1 encoder",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let sink_caps = gst::Caps::new_simple(
            "video/x-raw",
            &[
                (
                    "format",
                    &gst::List::new(&[
                        &gst_video::VideoFormat::I420.to_string(),
                        &gst_video::VideoFormat::Y42b.to_string(),
                        &gst_video::VideoFormat::Y444.to_string(),
                        &gst_video::VideoFormat::I42010le.to_string(),
                        &gst_video::VideoFormat::I42210le.to_string(),
                        &gst_video::VideoFormat::Y44410le.to_string(),
                        &gst_video::VideoFormat::I42012le.to_string(),
                        &gst_video::VideoFormat::I42212le.to_string(),
                        &gst_video::VideoFormat::Y44412le.to_string(),
                        // &gst_video::VideoFormat::Gray8.to_string(),
                    ]),
                ),
                ("width", &gst::IntRange::<i32>::new(1, std::i32::MAX)),
                ("height", &gst::IntRange::<i32>::new(1, std::i32::MAX)),
                (
                    "framerate",
                    &gst::FractionRange::new(
                        gst::Fraction::new(0, 1),
                        gst::Fraction::new(std::i32::MAX, 1),
                    ),
                ),
            ],
        );
        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &sink_caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        let src_caps = gst::Caps::new_simple("video/x-av1", &[]);
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &src_caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for Rav1Enc {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("speed-preset", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.speed_preset = value.get().unwrap();
            }
            subclass::Property("low-latency", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.low_latency = value.get().unwrap();
            }
            subclass::Property("min-key-frame-interval", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.min_key_frame_interval = value.get().unwrap();
            }
            subclass::Property("max-key-frame-interval", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_key_frame_interval = value.get().unwrap();
            }
            subclass::Property("bitrate", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.bitrate = value.get().unwrap();
            }
            subclass::Property("quantizer", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.quantizer = value.get::<u32>().unwrap() as usize;
            }
            subclass::Property("tile-cols-log2", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.tile_cols_log2 = value.get::<u32>().unwrap() as usize;
            }
            subclass::Property("tile-rows-log2", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.tile_rows_log2 = value.get::<u32>().unwrap() as usize;
            }
            subclass::Property("threads", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.threads = value.get::<u32>().unwrap() as usize;
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("speed-preset", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.speed_preset.to_value())
            }
            subclass::Property("low-latency", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.low_latency.to_value())
            }
            subclass::Property("min-key-frame-interval", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.min_key_frame_interval.to_value())
            }
            subclass::Property("max-key-frame-interval", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.max_key_frame_interval.to_value())
            }
            subclass::Property("bitrate", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.bitrate.to_value())
            }
            subclass::Property("quantizer", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok((settings.quantizer as u32).to_value())
            }
            subclass::Property("tile-cols-log2", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok((settings.tile_cols_log2 as u32).to_value())
            }
            subclass::Property("tile-rows-log2", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok((settings.tile_rows_log2 as u32).to_value())
            }
            subclass::Property("threads", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok((settings.threads as u32).to_value())
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for Rav1Enc {}

impl VideoEncoderImpl for Rav1Enc {
    fn stop(&self, element: &gst_video::VideoEncoder) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = None;
        self.parent_stop(element)
    }

    fn set_format(
        &self,
        element: &gst_video::VideoEncoder,
        state: &gst_video::VideoCodecState<gst_video::video_codec_state::Readable>,
    ) -> Result<(), gst::LoggableError> {
        self.finish(element)
            .map_err(|_| gst_loggable_error!(self.cat, "Failed to drain"))?;

        let video_info = state.get_info();
        gst_debug!(self.cat, obj: element, "Setting format {:?}", video_info);

        let settings = self.settings.lock().unwrap();

        // TODO: More properties, HDR information
        let cfg = config::Config {
            enc: config::EncoderConfig {
                width: video_info.width() as usize,
                height: video_info.height() as usize,
                bit_depth: video_info.format_info().depth()[0] as usize,
                chroma_sampling: match video_info.format() {
                    gst_video::VideoFormat::I420
                    | gst_video::VideoFormat::I42010le
                    | gst_video::VideoFormat::I42012le => color::ChromaSampling::Cs420,
                    gst_video::VideoFormat::Y42b
                    | gst_video::VideoFormat::I42210le
                    | gst_video::VideoFormat::I42212le => color::ChromaSampling::Cs422,
                    gst_video::VideoFormat::Y444
                    | gst_video::VideoFormat::Y44410le
                    | gst_video::VideoFormat::Y44412le => color::ChromaSampling::Cs444,
                    // gst_video::VideoFormat::Gray8 => color::ChromaSampling::Cs400,
                    _ => unreachable!(),
                },
                chroma_sample_position: match video_info.chroma_site() {
                    gst_video::VideoChromaSite::H_COSITED => color::ChromaSamplePosition::Vertical,
                    gst_video::VideoChromaSite::COSITED => color::ChromaSamplePosition::Colocated,
                    _ => color::ChromaSamplePosition::Unknown,
                },
                pixel_range: match video_info.colorimetry().range() {
                    gst_video::VideoColorRange::Range0255 => color::PixelRange::Full,
                    gst_video::VideoColorRange::Range16235 => color::PixelRange::Limited,
                    _ => color::PixelRange::Unspecified,
                },
                color_description: {
                    let matrix = match video_info.colorimetry().matrix() {
                        gst_video::VideoColorMatrix::Rgb => color::MatrixCoefficients::Identity,
                        gst_video::VideoColorMatrix::Fcc => color::MatrixCoefficients::BT470M,
                        gst_video::VideoColorMatrix::Bt709 => color::MatrixCoefficients::BT709,
                        gst_video::VideoColorMatrix::Bt601 => color::MatrixCoefficients::ST170M,
                        gst_video::VideoColorMatrix::Smpte240m => color::MatrixCoefficients::ST240M,
                        gst_video::VideoColorMatrix::Bt2020 => {
                            color::MatrixCoefficients::BT2020NonConstantLuminance
                        }
                        _ => color::MatrixCoefficients::Unspecified,
                    };
                    let transfer = match video_info.colorimetry().transfer() {
                        gst_video::VideoTransferFunction::Gamma10 => {
                            color::TransferCharacteristics::Linear
                        }
                        gst_video::VideoTransferFunction::Bt709 => {
                            color::TransferCharacteristics::BT1886
                        }
                        gst_video::VideoTransferFunction::Smpte240m => {
                            color::TransferCharacteristics::ST240M
                        }
                        gst_video::VideoTransferFunction::Srgb => {
                            color::TransferCharacteristics::SRGB
                        }
                        gst_video::VideoTransferFunction::Log100 => {
                            color::TransferCharacteristics::Logarithmic100
                        }
                        gst_video::VideoTransferFunction::Log316 => {
                            color::TransferCharacteristics::Logarithmic316
                        }
                        gst_video::VideoTransferFunction::Bt202012 => {
                            color::TransferCharacteristics::BT2020Twelve
                        }
                        gst_video::VideoTransferFunction::Gamma18
                        | gst_video::VideoTransferFunction::Gamma20
                        | gst_video::VideoTransferFunction::Gamma22
                        | gst_video::VideoTransferFunction::Gamma28
                        | gst_video::VideoTransferFunction::Adobergb
                        | _ => color::TransferCharacteristics::Unspecified,
                    };
                    let primaries = match video_info.colorimetry().primaries() {
                        gst_video::VideoColorPrimaries::Bt709 => color::ColorPrimaries::BT709,
                        gst_video::VideoColorPrimaries::Bt470m => color::ColorPrimaries::BT470M,
                        gst_video::VideoColorPrimaries::Bt470bg => color::ColorPrimaries::BT470BG,
                        gst_video::VideoColorPrimaries::Smpte170m => color::ColorPrimaries::ST170M,
                        gst_video::VideoColorPrimaries::Smpte240m => color::ColorPrimaries::ST240M,
                        gst_video::VideoColorPrimaries::Film => color::ColorPrimaries::Film,
                        gst_video::VideoColorPrimaries::Bt2020 => color::ColorPrimaries::BT2020,
                        gst_video::VideoColorPrimaries::Adobergb | _ => {
                            color::ColorPrimaries::Unspecified
                        }
                    };

                    Some(color::ColorDescription {
                        color_primaries: primaries,
                        transfer_characteristics: transfer,
                        matrix_coefficients: matrix,
                    })
                },
                speed_settings: config::SpeedSettings::from_preset(settings.speed_preset as usize),
                time_base: if video_info.fps() != gst::Fraction::new(0, 1) {
                    data::Rational {
                        num: *video_info.fps().numer() as u64,
                        den: *video_info.fps().denom() as u64,
                    }
                } else {
                    data::Rational { num: 30, den: 1 }
                },
                low_latency: settings.low_latency,
                min_key_frame_interval: settings.min_key_frame_interval,
                max_key_frame_interval: settings.max_key_frame_interval,
                bitrate: settings.bitrate,
                quantizer: settings.quantizer,
                tile_cols_log2: settings.tile_cols_log2,
                tile_rows_log2: settings.tile_rows_log2,
                ..Default::default()
            },
            threads: settings.threads,
        };

        *self.state.lock().unwrap() = Some(State {
            context: if video_info.format_info().depth()[0] > 8 {
                Context::Sixteen(cfg.new_context())
            } else {
                Context::Eight(cfg.new_context())
            },
            video_info: video_info.clone(),
        });

        let output_state = element
            .set_output_state(gst::Caps::new_simple("video/x-av1", &[]), Some(state))
            .map_err(|_| gst_loggable_error!(self.cat, "Failed to set output state"))?;
        element
            .negotiate(output_state)
            .map_err(|_| gst_loggable_error!(self.cat, "Failed to negotiate"))?;

        self.parent_set_format(element, state)
    }

    fn flush(&self, element: &gst_video::VideoEncoder) -> bool {
        gst_debug!(self.cat, obj: element, "Flushing");

        let mut state_guard = self.state.lock().unwrap();
        if let Some(ref mut state) = *state_guard {
            state.context.flush();
            loop {
                match state.context.receive_packet() {
                    Ok(_) | Err(data::EncoderStatus::Encoded) => {
                        gst_debug!(self.cat, obj: element, "Dropping packet on flush",);
                    }
                    _ => break,
                }
            }
        }

        self.parent_flush(element)
    }

    fn finish(
        &self,
        element: &gst_video::VideoEncoder,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_debug!(self.cat, obj: element, "Finishing");

        let mut state_guard = self.state.lock().unwrap();
        if let Some(ref mut state) = *state_guard {
            if let Err(data::EncoderStatus::Failure) = state.context.send_frame(None) {
                return Err(gst::FlowError::Error);
            }
            state.context.flush();
        }
        drop(state_guard);
        self.output_frames(element)
    }

    fn handle_frame(
        &self,
        element: &gst_video::VideoEncoder,
        frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.output_frames(element)?;

        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        gst_debug!(
            self.cat,
            obj: element,
            "Sending frame {}",
            frame.get_system_frame_number()
        );

        let input_buffer = frame
            .get_input_buffer()
            .expect("frame without input buffer");

        let in_frame =
            gst_video::VideoFrameRef::from_buffer_ref_readable(&*input_buffer, &state.video_info)
                .ok_or_else(|| {
                gst_element_error!(
                    element,
                    gst::CoreError::Failed,
                    ["Failed to map output buffer readable"]
                );
                gst::FlowError::Error
            })?;

        match state.context.send_frame(Some(&in_frame)) {
            Ok(_) => {
                gst_debug!(
                    self.cat,
                    obj: element,
                    "Sent frame {}",
                    frame.get_system_frame_number()
                );
            }
            Err(data::EncoderStatus::Failure) => {
                gst_element_error!(element, gst::CoreError::Failed, ["Failed to send frame"]);
                return Err(gst::FlowError::Error);
            }
            Err(_) => (),
        }

        drop(state_guard);
        self.output_frames(element)
    }
}

impl Rav1Enc {
    fn output_frames(
        &self,
        element: &gst_video::VideoEncoder,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state_guard = self.state.lock().unwrap();
        let mut state = match *state_guard {
            None => return Ok(gst::FlowSuccess::Ok),
            Some(ref mut state) => state,
        };

        loop {
            match state.context.receive_packet() {
                Ok((packet_type, packet_number, packet_data)) => {
                    gst_debug!(
                        self.cat,
                        obj: element,
                        "Received packet {} of size {}, frame type {:?}",
                        packet_number,
                        packet_data.len(),
                        packet_type
                    );

                    let frame = element.get_oldest_frame().expect("frame not found");
                    if packet_type == data::FrameType::KEY {
                        frame.set_flags(gst_video::VideoCodecFrameFlags::SYNC_POINT);
                    }
                    let output_buffer = gst::Buffer::from_mut_slice(packet_data);
                    frame.set_output_buffer(output_buffer);
                    drop(state_guard);
                    element.finish_frame(frame)?;
                    state_guard = self.state.lock().unwrap();
                    state = state_guard.as_mut().expect("Not negotiated yet");
                }
                Err(data::EncoderStatus::Encoded) => {
                    gst_debug!(self.cat, obj: element, "Encoded but not output frame yet",);
                }
                Err(data::EncoderStatus::Failure) => {
                    gst_element_error!(
                        element,
                        gst::CoreError::Failed,
                        ["Failed to receive frame"]
                    );
                    return Err(gst::FlowError::Error);
                }
                Err(err) => {
                    gst_debug!(
                        self.cat,
                        obj: element,
                        "Soft error when receiving frame: {:?}",
                        err
                    );
                    return Ok(gst::FlowSuccess::Ok);
                }
            }
        }
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rav1enc",
        gst::Rank::None,
        Rav1Enc::get_type(),
    )
}
