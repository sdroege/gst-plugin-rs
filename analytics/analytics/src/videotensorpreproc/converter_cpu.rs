// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use super::super::{imp::CAT, sampler::Sampler};
use super::{Config, NormParam, RoiLayout, TensorLayout};
use gst::prelude::*;
use gst_video::prelude::*;

fn normalize(param: &NormParam, value: f64, channel: usize) -> f64 {
    match param {
        NormParam::Identity => value,
        NormParam::ScaleOffset { scales, offsets } => value * scales[channel] + offsets[channel],
    }
}

pub(super) struct Converter {
    input_info: gst_video::VideoInfo,
    converter: Option<gst_video::VideoConverter>,
    info: gst_video::VideoInfo,
    converted: Option<gst::Buffer>,
    tensor_pool: gst::BufferPool,
    can_direct_output: bool,
}

impl Converter {
    pub(super) fn new(
        input: &gst_video::VideoInfo,
        info: gst_video::VideoInfo,
        config: &Config,
        layout: &TensorLayout,
        tensor_size: u32,
        tensor_rect: &gst_video::VideoRectangle,
    ) -> Result<Self, String> {
        let [width, height] = [tensor_rect.w as usize, tensor_rect.h as usize];
        let [dest_x, dest_y] = [tensor_rect.x, tensor_rect.y];
        let canvas = layout.canvas;
        let tight_layout = info.size() == tensor_size as usize
            && (0..info.n_planes() as usize).all(|plane| {
                info.stride()[plane] as usize == layout.stride
                    && info.offset()[plane] == plane * layout.plane_size
            });

        let can_direct_output = config.normalization.is_identity()
            && config.video_float == config.tensor_float
            && tight_layout;
        let (converter, converted) = if input.format() == info.format()
            && input.width() == info.width()
            && input.height() == info.height()
            && input.par() == info.par()
            && width == canvas[0]
            && height == canvas[1]
        {
            (None, None)
        } else {
            let mut options = gst_video::VideoConverterConfig::new();
            options.set_dest_x(dest_x);
            options.set_dest_y(dest_y);
            options.set_dest_width(Some(width as i32));
            options.set_dest_height(Some(height as i32));
            options.set_fill_border(true);

            let converter = gst_video::VideoConverter::new(input, &info, Some(options))
                .map_err(|e| e.to_string())?;
            let converted = if can_direct_output {
                None
            } else {
                Some(gst::Buffer::with_size(info.size()).map_err(|e| e.to_string())?)
            };
            (Some(converter), converted)
        };

        let tensor_pool = gst::BufferPool::new();
        let mut pool_config = tensor_pool.config();

        pool_config.set_params(None, tensor_size, 0, 0);
        tensor_pool
            .set_config(pool_config)
            .map_err(|e| e.to_string())?;
        tensor_pool.set_active(true).map_err(|e| e.to_string())?;

        gst::debug!(
            CAT,
            "Configured converter, input: {}x{} {:?}, output: {}x{} {:?}, \
            resized: {}x{}, conversion: {}, normalization: {}, direct-output: {}",
            input.width(),
            input.height(),
            input.format(),
            info.width(),
            info.height(),
            info.format(),
            width,
            height,
            converter.is_some(),
            !config.normalization.is_identity(),
            can_direct_output,
        );

        Ok(Self {
            input_info: input.clone(),
            info,
            converter,
            converted,
            tensor_pool,
            can_direct_output,
        })
    }

    fn pack<T>(
        config: &Config,
        layout: &TensorLayout,
        frame: &gst_video::VideoFrameRef<T>,
        mut buffer: gst::Buffer,
    ) -> Result<gst::Buffer, String> {
        let [width, height] = layout.canvas;
        let bytes = if config.tensor_float { 4 } else { 1 };
        let source_bytes = if config.video_float { 4 } else { 1 };

        let mut output = buffer
            .get_mut()
            .ok_or("Tensor buffer is not writable")?
            .map_writable()
            .map_err(|e| e.to_string())?;

        let planar = frame.n_planes() > 1;
        let plane_channels = if planar { 1 } else { config.channels };

        for plane in 0..frame.n_planes() {
            let pixels = frame.plane_data(plane).map_err(|e| e.to_string())?;
            let stride = frame.plane_stride()[plane as usize] as usize;
            let start = plane as usize * layout.plane_size;
            let output = &mut output[start..start + layout.plane_size];

            if config.normalization.is_identity() && config.video_float == config.tensor_float {
                if stride == layout.stride {
                    output.copy_from_slice(&pixels[..layout.plane_size]);
                } else {
                    for (src_row, dst_row) in Iterator::zip(
                        pixels.chunks(stride).take(height),
                        output.chunks_mut(layout.stride).take(height),
                    ) {
                        dst_row.copy_from_slice(&src_row[..layout.stride]);
                    }
                }
            } else {
                for (src_row, dst_row) in Iterator::zip(
                    pixels.chunks_exact(stride),
                    output.chunks_exact_mut(layout.stride),
                ) {
                    for x in 0..width {
                        for component in 0..plane_channels {
                            let channel = if planar { plane as usize } else { component };
                            let sample = x * plane_channels + component;
                            let pos = sample * source_bytes;
                            let value = if config.video_float {
                                f32::from_ne_bytes(src_row[pos..pos + 4].try_into().unwrap()) as f64
                            } else {
                                src_row[pos] as f64
                            };
                            let value = normalize(&config.normalization, value, channel);
                            let pos = sample * bytes;
                            if config.tensor_float {
                                dst_row[pos..pos + 4]
                                    .copy_from_slice(&(value as f32).to_ne_bytes());
                            } else {
                                dst_row[pos] = value.clamp(0.0, 255.0) as u8;
                            }
                        }
                    }
                }
            }
        }

        drop(output);

        Ok(buffer)
    }

    pub(super) fn process(
        &mut self,
        input: &gst::BufferRef,
        config: &Config,
        layout: &TensorLayout,
    ) -> Result<gst::Buffer, String> {
        let source = gst_video::VideoFrameRef::from_buffer_ref_readable(input, &self.input_info)
            .map_err(|e| e.to_string())?;

        if let Some(converter) = &self.converter {
            let mut tensor_buffer = self
                .tensor_pool
                .acquire_buffer(None)
                .map_err(|e| e.to_string())?;

            if self.can_direct_output {
                {
                    let mut frame = gst_video::VideoFrameRef::from_buffer_ref_writable(
                        tensor_buffer.get_mut().unwrap(),
                        &self.info,
                    )
                    .map_err(|e| e.to_string())?;
                    converter.frame_ref(&source, &mut frame);
                }
                Ok(tensor_buffer)
            } else {
                let mut frame = gst_video::VideoFrameRef::from_buffer_ref_writable(
                    self.converted.as_mut().unwrap().get_mut().unwrap(),
                    &self.info,
                )
                .map_err(|e| e.to_string())?;
                converter.frame_ref(&source, &mut frame);
                Self::pack(config, layout, &frame, tensor_buffer)
            }
        } else {
            if self.can_direct_output {
                let info = source.info();
                if info.stride() == self.info.stride() && info.offset() == self.info.offset() {
                    drop(source);
                    return Ok(input.copy());
                }
            }

            let tensor_buffer = self
                .tensor_pool
                .acquire_buffer(None)
                .map_err(|e| e.to_string())?;

            // Copy to the tensor buffer to avoid circular reference
            Self::pack(config, layout, &source, tensor_buffer)
        }
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        let _ = self.tensor_pool.set_active(false);
    }
}

pub(super) struct RoiConverter {
    input_info: gst_video::VideoInfo,
    info: gst_video::VideoInfo,
    converter: Option<gst_video::VideoConverter>,
    converted: Option<gst::Buffer>,
}

impl RoiConverter {
    pub(super) fn new(input: &gst_video::VideoInfo, config: &Config) -> Result<Self, String> {
        let info =
            gst_video::VideoInfo::builder(config.video_format(), input.width(), input.height())
                .par(input.par())
                .fps(input.fps())
                .build()
                .map_err(|e| e.to_string())?;

        let (converter, converted) = if input.format() == info.format() {
            (None, None)
        } else {
            (
                Some(
                    gst_video::VideoConverter::new(input, &info, None)
                        .map_err(|e| e.to_string())?,
                ),
                Some(gst::Buffer::with_size(info.size()).map_err(|e| e.to_string())?),
            )
        };

        Ok(Self {
            input_info: input.clone(),
            info,
            converter,
            converted,
        })
    }

    pub(super) fn process(
        &mut self,
        input: &gst::BufferRef,
        config: &Config,
        layouts: &[RoiLayout],
    ) -> Result<Vec<gst::Buffer>, String> {
        let source = gst_video::VideoFrameRef::from_buffer_ref_readable(input, &self.input_info)
            .map_err(|e| e.to_string())?;

        if let Some(converter) = &self.converter {
            let mut frame = gst_video::VideoFrameRef::from_buffer_ref_writable(
                self.converted.as_mut().unwrap().get_mut().unwrap(),
                &self.info,
            )
            .map_err(|e| e.to_string())?;
            converter.frame_ref(&source, &mut frame);
            Self::sample_rois(config, &frame, layouts)
        } else {
            Self::sample_rois(config, &source, layouts)
        }
    }

    pub(super) fn process_batch(
        &mut self,
        input: &gst::BufferRef,
        config: &Config,
        layouts: &[RoiLayout],
        size: usize,
    ) -> Result<gst::Buffer, String> {
        let source = gst_video::VideoFrameRef::from_buffer_ref_readable(input, &self.input_info)
            .map_err(|e| e.to_string())?;

        if let Some(converter) = &self.converter {
            let mut frame = gst_video::VideoFrameRef::from_buffer_ref_writable(
                self.converted.as_mut().unwrap().get_mut().unwrap(),
                &self.info,
            )
            .map_err(|e| e.to_string())?;
            converter.frame_ref(&source, &mut frame);
            Self::sample_rois_batch(config, &frame, layouts, size)
        } else {
            Self::sample_rois_batch(config, &source, layouts, size)
        }
    }

    fn sample_rois_batch<T>(
        config: &Config,
        frame: &gst_video::VideoFrameRef<T>,
        layouts: &[RoiLayout],
        size: usize,
    ) -> Result<gst::Buffer, String> {
        let canvas = layouts[0].canvas;
        let sample_size = layouts[0].size;
        let bytes = if config.tensor_float { 4 } else { 1 };
        let sampler = Sampler::new(frame, config.channels, config.video_float)?;
        let mut buffer = gst::Buffer::with_size(size).map_err(|e| e.to_string())?;
        {
            let mut output = buffer
                .get_mut()
                .unwrap()
                .map_writable()
                .map_err(|e| e.to_string())?;

            for (layout, output) in layouts.iter().zip(output.chunks_exact_mut(sample_size)) {
                for y in 0..canvas[1] {
                    for x in 0..canvas[0] {
                        // Replicated padding is a border policy, not an affine transform.
                        let position = layout
                            .geometry
                            .sample_position(x.min(layout.content_width - 1), y);
                        for channel in 0..config.channels {
                            let value = normalize(
                                &config.normalization,
                                sampler.sample_cubic(position, channel),
                                channel,
                            );

                            let pos = if config.planar {
                                (channel * canvas[1] + y) * canvas[0] + x
                            } else {
                                (y * canvas[0] + x) * config.channels + channel
                            } * bytes;

                            if config.tensor_float {
                                output[pos..pos + 4].copy_from_slice(&(value as f32).to_ne_bytes());
                            } else {
                                output[pos] = value.clamp(0.0, 255.0) as u8;
                            }
                        }
                    }
                }
            }
        }

        Ok(buffer)
    }

    fn sample_rois<T>(
        config: &Config,
        frame: &gst_video::VideoFrameRef<T>,
        layouts: &[RoiLayout],
    ) -> Result<Vec<gst::Buffer>, String> {
        let sampler = Sampler::new(frame, config.channels, config.video_float)?;

        layouts
            .iter()
            .map(|layout| {
                let canvas = layout.canvas;
                let bytes = if config.tensor_float { 4 } else { 1 };
                let size = layout.size;

                let mut buffer = gst::Buffer::with_size(size).map_err(|e| e.to_string())?;
                {
                    let mut output = buffer
                        .get_mut()
                        .unwrap()
                        .map_writable()
                        .map_err(|e| e.to_string())?;

                    for y in 0..canvas[1] {
                        for x in 0..canvas[0] {
                            // Replicated padding is a border policy, not an affine transform.
                            let position = layout
                                .geometry
                                .sample_position(x.min(layout.content_width - 1), y);
                            for channel in 0..config.channels {
                                let value = normalize(
                                    &config.normalization,
                                    sampler.sample_cubic(position, channel),
                                    channel,
                                );

                                let pos = if config.planar {
                                    (channel * canvas[1] + y) * canvas[0] + x
                                } else {
                                    (y * canvas[0] + x) * config.channels + channel
                                } * bytes;

                                if config.tensor_float {
                                    output[pos..pos + 4]
                                        .copy_from_slice(&(value as f32).to_ne_bytes());
                                } else {
                                    output[pos] = value.clamp(0.0, 255.0) as u8;
                                }
                            }
                        }
                    }
                }

                Ok(buffer)
            })
            .collect()
    }
}
