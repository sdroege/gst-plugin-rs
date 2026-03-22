// Copyright (C) 2026 Zeeshan Ali <zeenix@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::io::Write;
use std::sync::{LazyLock, Mutex};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;
use image::{DynamicImage, ImageBuffer};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "viuersink",
        gst::DebugColorFlags::empty(),
        Some("Viuer terminal video sink"),
    )
});

struct State {
    video_info: gst_video::VideoInfo,
}

fn default_config() -> viuer::Config {
    viuer::Config {
        restore_cursor: true,
        ..Default::default()
    }
}

pub struct ViuerSink {
    config: Mutex<viuer::Config>,
    state: Mutex<Option<State>>,
}

impl Default for ViuerSink {
    fn default() -> Self {
        Self {
            config: Mutex::new(default_config()),
            state: Mutex::new(None),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ViuerSink {
    const NAME: &'static str = "GstViuerSink";
    type Type = super::ViuerSink;
    type ParentType = gst_video::VideoSink;
}

impl ObjectImpl for ViuerSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("width")
                    .nick("Width")
                    .blurb("Terminal columns for rendering (0 = auto-detect)")
                    .default_value(0)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("height")
                    .nick("Height")
                    .blurb("Terminal rows for rendering (0 = auto-detect)")
                    .default_value(0)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("use-kitty")
                    .nick("Use Kitty")
                    .blurb("Use Kitty graphics protocol if supported")
                    .default_value(true)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("use-iterm")
                    .nick("Use iTerm")
                    .blurb("Use iTerm graphics protocol if supported")
                    .default_value(true)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("use-sixel")
                    .nick("Use Sixel")
                    .blurb("Use Sixel graphics protocol if supported")
                    .default_value(true)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("truecolor")
                    .nick("Truecolor")
                    .blurb("Use truecolor for half-block fallback rendering")
                    .default_value(true)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let config = self.config.lock().unwrap();
        match pspec.name() {
            "width" => config.width.unwrap_or(0).to_value(),
            "height" => config.height.unwrap_or(0).to_value(),
            "use-kitty" => config.use_kitty.to_value(),
            "use-iterm" => config.use_iterm.to_value(),
            "use-sixel" => config.use_sixel.to_value(),
            "truecolor" => config.truecolor.to_value(),
            _ => unimplemented!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut config = self.config.lock().unwrap();
        match pspec.name() {
            "width" => {
                let v = value.get().expect("type checked upstream");
                config.width = if v == 0 { None } else { Some(v) };
            }
            "height" => {
                let v = value.get().expect("type checked upstream");
                config.height = if v == 0 { None } else { Some(v) };
            }
            "use-kitty" => {
                config.use_kitty = value.get().expect("type checked upstream");
            }
            "use-iterm" => {
                config.use_iterm = value.get().expect("type checked upstream");
            }
            "use-sixel" => {
                config.use_sixel = value.get().expect("type checked upstream");
            }
            "truecolor" => {
                config.truecolor = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for ViuerSink {}

impl ElementImpl for ViuerSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Viuer Terminal Video Sink",
                "Sink/Video",
                "Renders video to the terminal using viuer",
                "Zeeshan Ali Khan <zeenix@gmail.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_video::VideoCapsBuilder::new()
                .format_list([
                    gst_video::VideoFormat::Rgb,
                    gst_video::VideoFormat::Rgba,
                    gst_video::VideoFormat::Gray8,
                ])
                .pixel_aspect_ratio(gst::Fraction::new(1, 1))
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseSinkImpl for ViuerSink {
    fn propose_allocation(
        &self,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        query.add_allocation_meta::<gst_video::VideoMeta>(None);
        self.parent_propose_allocation(query)
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        // Hide the cursor so it doesn't blink over the rendered video.
        let mut stdout = std::io::stdout().lock();
        let _ = write!(stdout, "\x1b[?25l");
        let _ = stdout.flush();

        Ok(())
    }

    fn set_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let video_info = gst_video::VideoInfo::from_caps(caps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid caps"))?;

        gst::debug!(CAT, imp = self, "Configured format: {video_info:?}");

        *self.state.lock().unwrap() = Some(State { video_info });

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = None;

        Ok(())
    }
}

impl VideoSinkImpl for ViuerSink {
    fn show_frame(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (frame, format) = {
            let state_guard = self.state.lock().unwrap();
            let state = state_guard.as_ref().ok_or(gst::FlowError::NotNegotiated)?;
            let video_info = &state.video_info;

            let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, video_info)
                .map_err(|_| {
                    gst::error!(CAT, imp = self, "Failed to map video frame");
                    gst::FlowError::Error
                })?;

            (frame, video_info.format())
        };
        let width = frame.width();
        let height = frame.height();
        let data = frame_data(&frame);

        let image = match format {
            gst_video::VideoFormat::Rgb => {
                let buf = ImageBuffer::from_raw(width, height, data).ok_or_else(|| {
                    gst::error!(CAT, imp = self, "Failed to create RGB image buffer");
                    gst::FlowError::Error
                })?;
                DynamicImage::ImageRgb8(buf)
            }
            gst_video::VideoFormat::Rgba => {
                let buf = ImageBuffer::from_raw(width, height, data).ok_or_else(|| {
                    gst::error!(CAT, imp = self, "Failed to create RGBA image buffer");
                    gst::FlowError::Error
                })?;
                DynamicImage::ImageRgba8(buf)
            }
            gst_video::VideoFormat::Gray8 => {
                let buf = ImageBuffer::from_raw(width, height, data).ok_or_else(|| {
                    gst::error!(CAT, imp = self, "Failed to create Gray8 image buffer");
                    gst::FlowError::Error
                })?;
                DynamicImage::ImageLuma8(buf)
            }
            _ => unreachable!(),
        };

        let config = self.config.lock().unwrap().clone();

        viuer::print(&image, &config).map_err(|e| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Write,
                ["viuer::print failed: {e}"]
            );
            gst::FlowError::Error
        })?;

        Ok(gst::FlowSuccess::Ok)
    }
}

/// Extract tightly-packed pixel data from a video frame, repacking if the
/// buffer has stride padding.
fn frame_data(frame: &gst_video::VideoFrameRef<&gst::BufferRef>) -> Vec<u8> {
    assert_eq!(frame.n_planes(), 1);

    let plane = frame.plane_data(0).unwrap();
    let stride = frame.plane_stride()[0] as usize;
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let bpp = frame.format_info().pixel_stride()[0] as usize;
    let line_size = width * bpp;

    if stride == line_size {
        return plane.to_vec();
    }

    plane.chunks_exact(stride).take(height).fold(
        Vec::with_capacity(line_size * height),
        |mut packed, row| {
            packed.extend_from_slice(&row[..line_size]);
            packed
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
        });
    }

    /// Create a buffer with the given pixel data and custom stride (with padding).
    fn padded_buffer(
        format: gst_video::VideoFormat,
        width: u32,
        height: u32,
        stride: i32,
        pixel_data: &[u8],
    ) -> (gst::Buffer, gst_video::VideoInfo) {
        let video_info = gst_video::VideoInfo::builder(format, width, height)
            .build()
            .unwrap();
        let bpp = video_info.format_info().pixel_stride()[0] as usize;
        let line_size = width as usize * bpp;
        let buf_size = stride as usize * height as usize;

        let mut buffer = gst::Buffer::with_size(buf_size).unwrap();
        {
            let buf_mut = buffer.get_mut().unwrap();

            // Write pixel data row-by-row with stride padding.
            let mut map = buf_mut.map_writable().unwrap();
            let dest = map.as_mut_slice();
            for (dst_row, src_row) in dest
                .chunks_exact_mut(stride as usize)
                .zip(pixel_data.chunks_exact(line_size))
            {
                dst_row[..line_size].copy_from_slice(src_row);
            }
        }

        // Attach VideoMeta with custom stride so VideoFrameRef uses it.
        gst_video::VideoMeta::add_full(
            buffer.get_mut().unwrap(),
            gst_video::VideoFrameFlags::empty(),
            format,
            width,
            height,
            &[0],
            &[stride],
        )
        .unwrap();

        (buffer, video_info)
    }

    #[test]
    fn frame_data_no_padding() {
        init();

        let width = 4u32;
        let height = 3u32;
        let format = gst_video::VideoFormat::Rgb;
        let bpp = 3;
        let pixels = (0..width * height * bpp)
            .map(|i| (i % 256) as u8)
            .collect::<Vec<_>>();

        let video_info = gst_video::VideoInfo::builder(format, width, height)
            .build()
            .unwrap();
        let buffer = gst::Buffer::from_slice(pixels.clone());
        let frame =
            gst_video::VideoFrameRef::from_buffer_ref_readable(&buffer, &video_info).unwrap();

        let result = frame_data(&frame);
        assert_eq!(result, pixels);
    }

    #[test]
    fn frame_data_with_padding() {
        init();

        let width = 3u32;
        let height = 2u32;
        let format = gst_video::VideoFormat::Rgb;
        let bpp = 3usize;
        let line_size = width as usize * bpp; // 9 bytes
        let stride = 16i32; // 7 bytes of padding per row

        // Tightly-packed pixel data (what we expect back).
        let pixels = (0..line_size * height as usize)
            .map(|i| (i % 256) as u8)
            .collect::<Vec<_>>();

        let (buffer, video_info) = padded_buffer(format, width, height, stride, &pixels);
        let frame =
            gst_video::VideoFrameRef::from_buffer_ref_readable(&buffer, &video_info).unwrap();

        let result = frame_data(&frame);
        assert_eq!(result, pixels);
    }

    #[test]
    fn frame_data_rgba_with_padding() {
        init();

        let width = 5u32;
        let height = 3u32;
        let format = gst_video::VideoFormat::Rgba;
        let bpp = 4usize;
        let line_size = width as usize * bpp; // 20 bytes
        let stride = 32i32; // 12 bytes of padding per row

        let pixels = (0..line_size * height as usize)
            .map(|i| (i % 256) as u8)
            .collect::<Vec<_>>();

        let (buffer, video_info) = padded_buffer(format, width, height, stride, &pixels);
        let frame =
            gst_video::VideoFrameRef::from_buffer_ref_readable(&buffer, &video_info).unwrap();

        let result = frame_data(&frame);
        assert_eq!(result, pixels);
    }

    #[test]
    fn frame_data_gray8_with_padding() {
        init();

        let width = 7u32;
        let height = 4u32;
        let format = gst_video::VideoFormat::Gray8;
        let bpp = 1usize;
        let line_size = width as usize * bpp; // 7 bytes
        let stride = 8i32; // 1 byte of padding per row

        let pixels = (0..line_size * height as usize)
            .map(|i| (i % 256) as u8)
            .collect::<Vec<_>>();

        let (buffer, video_info) = padded_buffer(format, width, height, stride, &pixels);
        let frame =
            gst_video::VideoFrameRef::from_buffer_ref_readable(&buffer, &video_info).unwrap();

        let result = frame_data(&frame);
        assert_eq!(result, pixels);
    }
}
