// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on pngenc for the data flows

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;

use atomic_refcell::AtomicRefCell;
use byte_slice_cast::*;
use image::flat::{NormalForm, SampleLayout};
use image::{
    EncodableLayout, FlatSamples, GenericImage, GenericImageView, ImageBuffer, ImageFormat, Luma,
    PixelWithColorType, Rgb, Rgba,
};

use std::collections::BTreeSet;
use std::io::Cursor;
use std::sync::LazyLock;

use crate::cicp::ImageCicp;
use crate::format::Format;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "imagersenc",
        gst::DebugColorFlags::empty(),
        Some("image-rs encoder"),
    )
});

struct State {
    format: Format,
    video_state: gst_video::video_codec_state::VideoCodecState<
        'static,
        gst_video::video_codec_state::Readable,
    >,
}

#[derive(Default)]
pub struct Encoder {
    state: AtomicRefCell<Option<State>>,
}

#[glib::object_subclass]
impl ObjectSubclass for Encoder {
    const NAME: &'static str = "GstImageRsEncoder";
    type Type = super::Encoder;
    type ParentType = gst_video::VideoEncoder;
}

impl ObjectImpl for Encoder {}

impl GstObjectImpl for Encoder {}

impl ElementImpl for Encoder {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "image-rs encoder",
                "Encoder/Video",
                "Encodes still images",
                "Amyspark <amy@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let formats = {
                let mut downstream_depths = BTreeSet::new();
                for (_, formats) in Format::all_encoding_formats() {
                    downstream_depths.extend(formats);
                }
                downstream_depths.into_iter().collect::<Vec<_>>()
            };
            let sink_caps = gst_video::VideoCapsBuilder::new()
                .format_list(formats)
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let mut src_caps = gst::Caps::new_empty();
            {
                let caps = src_caps.make_mut();

                for (c, _) in Format::all_encoding_formats() {
                    caps.append(c);
                }
            };
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

impl VideoEncoderImpl for Encoder {
    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = None;
        Ok(())
    }

    /// Minimize padding, please!
    /// References:
    /// https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/1.28.1/subprojects/gst-plugins-base/gst-libs/gst/video/video-info.c#L893-900
    ///
    /// https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/1.28.1/subprojects/gst-plugins-base/gst-libs/gst/video/gstvideoencoder.c#L895
    fn propose_allocation(
        &self,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        let params = gst::AllocationParams::default();
        query.add_allocation_param(None::<&gst::Allocator>, params);
        let video_meta = gst::Structure::builder("video-meta").build();
        query.add_allocation_meta::<gst_video::VideoMeta>(Some(&video_meta));
        let res = self.parent_propose_allocation(query);
        gst::debug!(CAT, imp = self, "Minimized padding query: {:?}", query);
        res
    }

    /// Generates the caps corresponding to the given filter
    /// and current src configuration. See qsvvp9enc
    fn caps(&self, filter: Option<&gst::Caps>) -> gst::Caps {
        match self.obj().src_pad().allowed_caps() {
            // Shouldn't be any or empty though (we need a specific
            // output format), just return template caps in this case below
            Some(allowed) if !allowed.is_empty() && !allowed.is_any() => {
                let mut templ_caps = self.obj().sink_pad().pad_template_caps();

                gst::debug!(CAT, imp = self, "template caps {templ_caps}");
                gst::debug!(CAT, imp = self, "allowed caps {allowed}");

                // Grab the template caps, apply format's bit depths
                let format_name = allowed.structure(0).unwrap().name().to_string();

                let pixel_fmts = Format::all_encoding_formats()
                    .into_iter()
                    .find(|c| c.0.iter().any(|v| *v.name() == format_name))
                    .map(|v| v.1);

                if let Some(formats) = pixel_fmts {
                    templ_caps.make_mut().set(
                        "format",
                        gst::List::new(formats.into_iter().map(|f| f.to_str())),
                    );

                    gst::debug!(CAT, imp = self, "pixel formats set {templ_caps}");
                } else {
                    // pixel_fmts may be empty if downstream's chosen output
                    // format is not declared by Format::all_encoding_formats()
                    // It may have been disabled or be a new one from upstream
                    gst::fixme!(
                        CAT,
                        imp = self,
                        "No pixel formats found for {format_name} -- support may have been disabled"
                    );
                    return gst::Caps::new_empty();
                }

                let supported_caps = self.obj().proxy_getcaps(Some(&templ_caps), filter);

                gst::debug!(CAT, imp = self, "Returning {supported_caps}");

                supported_caps
            }
            _ => self.obj().proxy_getcaps(None, filter),
        }
    }

    fn set_format(
        &self,
        state: &gst_video::VideoCodecState<'static, gst_video::video_codec_state::Readable>,
    ) -> Result<(), gst::LoggableError> {
        let instance = self.obj();

        let mut allowed_caps = match instance.src_pad().allowed_caps() {
            None => instance.src_pad().pad_template_caps(),
            Some(caps) => caps,
        };

        if allowed_caps.is_empty() {
            return Err(gst::loggable_error!(
                CAT,
                "Downstream doesn't specify any format or properties"
            ));
        }

        allowed_caps.fixate();

        let s = allowed_caps.structure(0).unwrap();

        let output_state = instance
            .set_output_state(gst::Caps::builder(s.name()).build(), Some(state))
            .map_err(|_| gst::loggable_error!(CAT, "Failed to set output state"))?;
        instance
            .negotiate(output_state)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to negotiate"))?;

        *self.state.borrow_mut() = Some(State {
            format: s
                .try_into()
                .map_err(|v| gst::loggable_error!(CAT, "Failed to determine format: {v}"))?,
            video_state: state.clone(),
        });

        Ok(())
    }

    fn handle_frame(
        &self,
        frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (video_info, format) = {
            let state_guard = self.state.borrow();

            let state = state_guard.as_ref().ok_or(gst::FlowError::NotNegotiated)?;

            (state.video_state.info().clone(), state.format)
        };

        let format = ImageFormat::try_from(format).map_err(|v| {
            gst::error!(CAT, imp = self, "{v}");
            gst::FlowError::NotNegotiated
        })?;

        gst::debug!(
            CAT,
            imp = self,
            "Sending frame {}",
            frame.system_frame_number()
        );

        match video_info.format() {
            gst_video::VideoFormat::Rgba => {
                self.render_to_image::<Rgba<u8>>(frame, &video_info, format)
            }
            gst_video::VideoFormat::Rgb => {
                self.render_to_image::<Rgb<u8>>(frame, &video_info, format)
            }
            gst_video::VideoFormat::Gray8 => {
                self.render_to_image::<Luma<u8>>(frame, &video_info, format)
            }
            #[cfg(target_endian = "little")]
            gst_video::VideoFormat::Gray16Le => {
                self.render_to_image::<Luma<u16>>(frame, &video_info, format)
            }
            #[cfg(target_endian = "big")]
            gst_video::VideoFormat::Gray16Be => {
                self.render_to_image::<Luma<u16>>(frame, &video_info, format)
            }
            #[cfg(target_endian = "little")]
            gst_video::VideoFormat::Rgba64Le => {
                self.render_to_image::<Rgba<u16>>(frame, &video_info, format)
            }
            #[cfg(target_endian = "big")]
            gst_video::VideoFormat::Rgba64Be => {
                self.render_to_image::<Rgba<u16>>(frame, &video_info, format)
            }
            v => {
                gst::error!(CAT, imp = self, "Unknown format {v}");
                Err(gst::FlowError::NotSupported)
            }
        }
    }
}

impl Encoder {
    fn render_to_image<T>(
        &self,
        mut frame: gst_video::VideoCodecFrame,
        video_info: &gst_video::VideoInfo,
        format: ImageFormat,
    ) -> Result<gst::FlowSuccess, gst::FlowError>
    where
        T: PixelWithColorType,
        [T::Subpixel]: EncodableLayout,
        T::Subpixel: byte_slice_cast::FromByteSlice,
    {
        let input_buffer = frame.input_buffer().ok_or_else(|| {
            gst::error!(CAT, imp = self, "Frame without input buffer");
            gst::FlowError::Error
        })?;

        let sample_size = std::mem::size_of::<T::Subpixel>();

        let layout = {
            let input_frame =
                gst_video::VideoFrameRef::from_buffer_ref_readable(input_buffer, video_info)
                    .map_err(|v| {
                        gst::error!(CAT, imp = self, "Buffer {v:?} is not readable");
                        gst::FlowError::Error
                    })?;

            SampleLayout {
                channels: input_frame.n_components().try_into().unwrap(),
                // Planar format (contiguous channels)
                channel_stride: 1,
                width: input_frame.width(),
                width_stride: (input_frame.comp_pstride(0) / sample_size as i32) as usize,
                height: input_frame.height(),
                height_stride: (input_frame.comp_stride(0) / sample_size as i32) as usize,
            }
        };

        let buffer_size = 128 * 1024 * 1024;

        let input_map = input_buffer.map_readable().map_err(|v| {
            gst::error!(CAT, imp = self, "Buffer {v:?} is not readable");
            gst::FlowError::Error
        })?;

        let samples = input_map.as_slice_of::<T::Subpixel>().map_err(|v| {
            gst::error!(
                CAT,
                imp = self,
                "Couldn't cast buffer to the expected format: {v}"
            );
            gst::FlowError::NotSupported
        })?;

        let color_space = ImageCicp::try_from(video_info.colorimetry())
            .inspect_err(|v| {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Failed converting from VideoColorimetry: {v}"
                );
            })
            .ok();

        let output_buffer = if layout.is_normal(NormalForm::RowMajorPacked) {
            gst::trace!(CAT, imp = self, "{layout:?} does not require repacking");

            let mut image = ImageBuffer::<T, _>::from_raw(layout.width, layout.height, samples)
                .ok_or(gst::FlowError::NotSupported)?;

            if let Some(v) = color_space
                && v.is_rgb()
                && let Err(e) = image.set_color_space(v.into())
            {
                gst::warning!(CAT, imp = self, "Failed to set color space: {e}");
            }

            let mut cursor = Cursor::new(Vec::with_capacity(buffer_size));

            image.write_to(&mut cursor, format).map_err(|e| {
                gst::error!(CAT, imp = self, "Failed to write image data: {e}");
                gst::FlowError::Error
            })?;

            gst::Buffer::from_mut_slice(cursor.into_inner())
        } else {
            gst::trace!(CAT, imp = self, "{layout:?} requires repacking");

            let container = FlatSamples {
                samples,
                layout,
                // Do not initialize color type, this is stride governed
                color_hint: None,
            };

            let view = container.as_view::<T>().expect("Mismatched pixel type");

            let mut image = GenericImageView::buffer_like(&view);

            image
                .copy_from(&view, 0, 0)
                .expect("Image buffer too small");

            if let Some(v) = color_space
                && v.is_rgb()
                && let Err(e) = image.set_color_space(v.into())
            {
                gst::warning!(CAT, imp = self, "Failed to set color space: {e}");
            }

            let mut cursor = Cursor::new(Vec::with_capacity(buffer_size));
            image.write_to(&mut cursor, format).map_err(|e| {
                gst::error!(CAT, imp = self, "Failed to write image data: {e}");
                gst::FlowError::Error
            })?;

            gst::Buffer::from_mut_slice(cursor.into_inner())
        };

        drop(input_map);

        // All images outputted by image-rs are whole frames
        // (see comment in pngenc, same applies)
        frame.set_flags(gst_video::VideoCodecFrameFlags::SYNC_POINT);
        frame.set_output_buffer(output_buffer);
        self.obj().finish_frame(frame)
    }
}
