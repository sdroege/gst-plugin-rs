// Copyright (C) 2020 Natanael Mojica <neithanmo@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::{io, io::Write, sync::Arc};

use glib::subclass;
use glib::subclass::prelude::*;

use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_element_error, gst_error, gst_loggable_error};
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;

use atomic_refcell::AtomicRefCell;
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use super::CompressionLevel;
use super::FilterType;

const DEFAULT_COMPRESSION_LEVEL: CompressionLevel = CompressionLevel::Default;
const DEFAULT_FILTER_TYPE: FilterType = FilterType::NoFilter;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rspngenc",
        gst::DebugColorFlags::empty(),
        Some("PNG encoder"),
    )
});

// Inner buffer where the result of frame encoding  is written
// before relay them downstream
struct CacheBuffer {
    buffer: AtomicRefCell<Vec<u8>>,
}

impl CacheBuffer {
    pub fn new() -> Self {
        Self {
            buffer: AtomicRefCell::new(Vec::new()),
        }
    }

    pub fn clear(&self) {
        self.buffer.borrow_mut().clear();
    }

    pub fn write(&self, buf: &[u8]) {
        let mut buffer = self.buffer.borrow_mut();
        buffer.extend_from_slice(buf);
    }

    pub fn consume(&self) -> Vec<u8> {
        let mut buffer = self.buffer.borrow_mut();
        std::mem::replace(&mut *buffer, Vec::new())
    }
}
// The Encoder requires a Writer, so we use here and intermediate structure
// for caching encoded frames
struct CacheWriter {
    cache: Arc<CacheBuffer>,
}

impl CacheWriter {
    pub fn new(cache: Arc<CacheBuffer>) -> Self {
        Self { cache }
    }
}

impl Write for CacheWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.cache.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct Settings {
    compression: CompressionLevel,
    filter: FilterType,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            compression: DEFAULT_COMPRESSION_LEVEL,
            filter: DEFAULT_FILTER_TYPE,
        }
    }
}

static PROPERTIES: [subclass::Property; 2] = [
    subclass::Property("compression-level", |name| {
        glib::ParamSpec::enum_(
            name,
            "Compression level",
            "Selects the compression algorithm to use",
            CompressionLevel::static_type(),
            DEFAULT_COMPRESSION_LEVEL as i32,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("filter", |name| {
        glib::ParamSpec::enum_(
            name,
            "Filter",
            "Selects the filter type to applied",
            FilterType::static_type(),
            DEFAULT_FILTER_TYPE as i32,
            glib::ParamFlags::READWRITE,
        )
    }),
];

struct State {
    video_info: gst_video::VideoInfo,
    cache: Arc<CacheBuffer>,
    writer: Option<png::Writer<CacheWriter>>,
}

impl State {
    fn new(video_info: gst_video::VideoInfo) -> Self {
        let cache = Arc::new(CacheBuffer::new());
        Self {
            video_info,
            cache,
            writer: None,
        }
    }

    fn reset(&mut self, settings: Settings) -> Result<(), gst::LoggableError> {
        // clear the cache
        self.cache.clear();
        let width = self.video_info.width();
        let height = self.video_info.height();
        let mut encoder = png::Encoder::new(CacheWriter::new(self.cache.clone()), width, height);
        let color = match self.video_info.format() {
            gst_video::VideoFormat::Gray8 | gst_video::VideoFormat::Gray16Be => {
                png::ColorType::Grayscale
            }
            gst_video::VideoFormat::Rgb => png::ColorType::RGB,
            gst_video::VideoFormat::Rgba => png::ColorType::RGBA,
            _ => {
                gst_error!(CAT, "format is not supported yet");
                unreachable!()
            }
        };
        let depth = if self.video_info.format() == gst_video::VideoFormat::Gray16Be {
            png::BitDepth::Sixteen
        } else {
            png::BitDepth::Eight
        };

        encoder.set_color(color);
        encoder.set_depth(depth);
        encoder.set_compression(png::Compression::from(settings.compression));
        encoder.set_filter(png::FilterType::from(settings.filter));
        // Write the header for this video format into our inner buffer
        let writer = encoder.write_header().map_err(|e| {
            gst_loggable_error!(CAT, "Failed to create encoder error: {}", e.to_string())
        })?;
        self.writer = Some(writer);
        Ok(())
    }

    fn write_data(&mut self, data: &[u8]) -> Result<(), png::EncodingError> {
        if let Some(writer) = self.writer.as_mut() {
            writer.write_image_data(data)
        } else {
            unreachable!()
        }
    }
}

pub struct PngEncoder {
    state: Mutex<Option<State>>,
    settings: Mutex<Settings>,
}

impl ObjectSubclass for PngEncoder {
    const NAME: &'static str = "PngEncoder";
    type Type = super::PngEncoder;
    type ParentType = gst_video::VideoEncoder;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib::object_subclass!();

    fn new() -> Self {
        Self {
            state: Mutex::new(None),
            settings: Mutex::new(Default::default()),
        }
    }

    fn class_init(klass: &mut Self::Class) {
        klass.set_metadata(
            "PNG encoder",
            "Encoder/Video",
            "PNG encoder",
            "Natanael Mojica <neithanmo@gmail>",
        );

        let sink_caps = gst::Caps::new_simple(
            "video/x-raw",
            &[
                (
                    "format",
                    &gst::List::new(&[
                        &gst_video::VideoFormat::Gray8.to_str(),
                        &gst_video::VideoFormat::Gray16Be.to_str(),
                        &gst_video::VideoFormat::Rgb.to_str(),
                        &gst_video::VideoFormat::Rgba.to_str(),
                    ]),
                ),
                ("width", &gst::IntRange::<i32>::new(1, std::i32::MAX)),
                ("height", &gst::IntRange::<i32>::new(1, std::i32::MAX)),
                (
                    "framerate",
                    &gst::FractionRange::new(
                        gst::Fraction::new(1, 1),
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

        let src_caps = gst::Caps::new_simple("image/png", &[]);
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

impl ObjectImpl for PngEncoder {
    fn set_property(&self, _obj: &Self::Type, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("compression-level", ..) => {
                let mut settings = self.settings.lock();
                settings.compression = value
                    .get_some::<CompressionLevel>()
                    .expect("type checked upstream");
            }
            subclass::Property("filter", ..) => {
                let mut settings = self.settings.lock();
                settings.filter = value
                    .get_some::<FilterType>()
                    .expect("type checked upstream");
            }
            _ => unreachable!(),
        }
    }

    fn get_property(&self, _obj: &Self::Type, id: usize) -> glib::Value {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("compression-level", ..) => {
                let settings = self.settings.lock();
                settings.compression.to_value()
            }
            subclass::Property("filter", ..) => {
                let settings = self.settings.lock();
                settings.filter.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for PngEncoder {}

impl VideoEncoderImpl for PngEncoder {
    fn stop(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        *self.state.lock() = None;
        Ok(())
    }

    fn set_format(
        &self,
        element: &Self::Type,
        state: &gst_video::VideoCodecState<'static, gst_video::video_codec_state::Readable>,
    ) -> Result<(), gst::LoggableError> {
        let video_info = state.get_info();
        gst_debug!(CAT, obj: element, "Setting format {:?}", video_info);
        {
            let settings = self.settings.lock();
            let mut state = State::new(video_info);
            state.reset(*settings)?;
            *self.state.lock() = Some(state);
        }

        let output_state = element
            .set_output_state(gst::Caps::new_simple("image/png", &[]), Some(state))
            .map_err(|_| gst_loggable_error!(CAT, "Failed to set output state"))?;
        element
            .negotiate(output_state)
            .map_err(|_| gst_loggable_error!(CAT, "Failed to negotiate"))
    }

    fn handle_frame(
        &self,
        element: &Self::Type,
        mut frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state_guard = self.state.lock();
        let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        gst_debug!(
            CAT,
            obj: element,
            "Sending frame {}",
            frame.get_system_frame_number()
        );
        {
            let input_buffer = frame
                .get_input_buffer()
                .expect("frame without input buffer");

            let input_map = input_buffer.map_readable().unwrap();
            let data = input_map.as_slice();
            state.write_data(data).map_err(|e| {
                gst_element_error!(element, gst::CoreError::Failed, [&e.to_string()]);
                gst::FlowError::Error
            })?;
        }

        let buffer = state.cache.consume();
        drop(state_guard);

        let output_buffer = gst::Buffer::from_mut_slice(buffer);
        // There are no such incremental frames in the png format
        frame.set_flags(gst_video::VideoCodecFrameFlags::SYNC_POINT);
        frame.set_output_buffer(output_buffer);
        element.finish_frame(Some(frame))
    }
}
