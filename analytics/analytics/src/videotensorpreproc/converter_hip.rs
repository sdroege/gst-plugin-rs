// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use super::super::imp::CAT;
use super::{Config, NormParam, RoiLayout};
use gst_hip::prelude::*;
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};

type KernelKey = (gst_hip::HipVendor, u32, [&'static str; 1]);
static KERNEL_BLOBS: LazyLock<Mutex<HashMap<KernelKey, Arc<gst_hip::HipCompiledBlob>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[repr(C)]
struct Parameters {
    width: u32,
    height: u32,
    channels: u32,
    planar: u32,
    output_offset: u32,
    tensor_float: u32,
    numeric_scale: f32,
    replicate: u32,
    video_width: u32,
    video_height: u32,
    content_width: f32,
    content_height: f32,
    roi_width: f32,
    roi_height: f32,
    matrix: [f32; 6],
    scales: [f32; 3],
    offsets: [f32; 3],
}

#[repr(C)]
struct PackParameters {
    width: u32,
    height: u32,
    channels: u32,
    planar: u32,
    tensor_float: u32,
    video_float: u32,
    content_width: u32,
    content_height: u32,
    strides: [u32; 3],
    scales: [f32; 3],
    offsets: [f32; 3],
}

struct ConvertedVideo {
    converter: gst_hip::HipConverter,
    pool: gst_hip::HipBufferPool,
    info: gst_video::VideoInfo,
    content: [usize; 2],
}

impl Drop for ConvertedVideo {
    fn drop(&mut self) {
        let _ = self.pool.set_active(false);
    }
}

pub(super) struct Converter {
    device: gst_hip::HipDevice,
    input_info: gst_video::VideoInfo,
    converted: Option<ConvertedVideo>,
    converter: gst_hip::HipConverter,
    video_pool: gst_hip::HipBufferPool,
    fence_data_pool: gst_hip::HipFenceDataPool,
    module: gst_hip::HipKernelModule,
    args: gst_hip::HipKernelArgs,
    allocator: Option<gst_hip::HipPoolAllocator>,
    capacity: usize,
}

impl Converter {
    pub(super) fn device(&self) -> &gst_hip::HipDevice {
        &self.device
    }

    pub(super) fn new(
        input: &gst_video::VideoInfo,
        config: &Config,
        device: gst_hip::HipDevice,
    ) -> Result<Self, String> {
        let format = match (config.channels, config.video_float) {
            (1, false) => gst_video::VideoFormat::Gray8,
            (1, true) => gst_video::VideoFormat::GrayF32le,
            (3, false) => gst_video::VideoFormat::Rgba,
            (3, true) => gst_video::VideoFormat::RgbaF32le,
            (channels, _) => return Err(format!("Unsupported number of channels {channels}")),
        };

        let info = gst_video::VideoInfo::builder(format, input.width(), input.height())
            .par(input.par())
            .fps(input.fps())
            .build()
            .map_err(|e| e.to_string())?;

        let converter =
            gst_hip::HipConverter::new(&device, input, &info, None).map_err(|e| e.to_string())?;

        let texture_define = match device.vendor() {
            gst_hip::HipVendor::Amd => "-DTextureObject_t=hipTextureObject_t",
            gst_hip::HipVendor::Nvidia => "-DTextureObject_t=cudaTextureObject_t",
            _ => return Err("Unsupported HIP vendor".into()),
        };

        let key = (device.vendor(), device.device_id(), [texture_define]);
        let blob = {
            let mut cache = KERNEL_BLOBS.lock().unwrap();
            if let Some(blob) = cache.get(&key) {
                gst::debug!(CAT, "Reusing HIP kernel blob for {key:?}");
                blob.clone()
            } else {
                gst::debug!(CAT, "Compiling HIP kernel blob for {key:?}");
                let compiled = Arc::new(
                    gst_hip::rtc_compile(&device, include_str!("preprocess.cu"), &key.2)
                        .map_err(|e| e.to_string())?,
                );
                cache.insert(key, compiled.clone());
                compiled
            }
        };

        let module =
            gst_hip::HipKernelModule::from_blob(&device, &blob).map_err(|e| e.to_string())?;

        if !module.ensure_entry_point("preprocess") || !module.ensure_entry_point("pack_tensor") {
            return Err("Couldn't find HIP preprocessing kernel".into());
        }

        let video_pool = gst_hip::HipBufferPool::new(&device);
        let mut pool_config = video_pool.config();
        pool_config.set_params(Some(&info.to_caps().map_err(|e| e.to_string())?), 0, 0, 0);
        pool_config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
        video_pool
            .set_config(pool_config)
            .map_err(|e| e.to_string())?;

        video_pool.set_active(true).map_err(|e| e.to_string())?;

        Ok(Self {
            device,
            input_info: input.clone(),
            converted: None,
            converter,
            video_pool,
            fence_data_pool: gst_hip::HipFenceDataPool::new(),
            module,
            args: gst_hip::HipKernelArgs::new(),
            allocator: None,
            capacity: 0,
        })
    }

    fn ensure_capacity(&mut self, size: usize) -> Result<(), String> {
        if size <= self.capacity {
            return Ok(());
        }

        if size > u32::MAX as usize {
            return Err("Tensor size overflow".into());
        }

        let allocator = gst_hip::HipPoolAllocator::for_bytes(&self.device, size);
        if !allocator.set_active(true) {
            return Err("Couldn't activate HIP tensor allocator".into());
        }

        if let Some(old) = self.allocator.replace(allocator) {
            old.set_active(false);
        }

        self.capacity = size;
        Ok(())
    }

    pub(super) fn process(
        &mut self,
        input: &gst::BufferRef,
        config: &Config,
        layouts: &[RoiLayout],
        replicate: bool,
    ) -> Result<Vec<gst::Buffer>, String> {
        layouts
            .iter()
            .map(|layout| {
                if self.can_convert(layout, replicate) {
                    self.convert(input, config, layout)
                } else {
                    self.ensure_capacity(layout.size)?;
                    self.run(
                        input,
                        config,
                        std::slice::from_ref(layout),
                        layout.size,
                        replicate,
                    )
                }
            })
            .collect()
    }

    pub(super) fn process_batch(
        &mut self,
        input: &gst::BufferRef,
        config: &Config,
        layouts: &[RoiLayout],
        size: usize,
    ) -> Result<gst::Buffer, String> {
        self.ensure_capacity(size)?;
        self.run(input, config, layouts, size, true)
    }

    fn can_convert(&self, layout: &RoiLayout, replicate: bool) -> bool {
        let (content, roi) = layout.geometry.sampling_dimensions();
        let matrix = layout.geometry.matrix();

        // Require no rotation (indices 1, 3) or translation (indices 2, 5)
        matrix[1] == 0.0
            && matrix[3] == 0.0
            && matrix[2] == 0.0
            && matrix[5] == 0.0
            // The ROI must cover the full input frame, so no source crop is needed
            // TODO: Allow cropping once it's supported by the converter
            && roi
                == [
                    self.input_info.width() as f64,
                    self.input_info.height() as f64,
                ]
            // The converter can fill borders, but cannot replicate edge pixels.
            // If replication is requested, the content must fill the canvas.
            && (!replicate || content == layout.canvas)
    }

    fn share_tensor(
        &self,
        input: &gst::BufferRef,
        info: &gst_video::VideoInfo,
        config: &Config,
        layout: &RoiLayout,
    ) -> Option<gst::Buffer> {
        let (content, _) = layout.geometry.sampling_dimensions();

        // Require tensor-compatible input without normalization, format conversion or padding.
        if !config.normalization.is_identity()
            || config.video_float != config.tensor_float
            || content != layout.canvas
            || info.format() != config.video_format()
            || [info.width() as usize, info.height() as usize] != layout.canvas
            || input.n_memory() != 1
            || input.size() != layout.size
        {
            return None;
        }

        let memory = input
            .peek_memory(0)
            .downcast_memory_ref::<gst_hip::HipMemory>()?;
        if !memory.device().is_equal(&self.device) {
            return None;
        }

        let actual_info = memory.video_info().unwrap_or_else(|| info.clone());
        let meta = input.meta::<gst_video::VideoMeta>();
        if meta.as_ref().is_some_and(|meta| {
            meta.format() != info.format()
                || meta.width() != info.width()
                || meta.height() != info.height()
                || meta.n_planes() != info.n_planes()
        }) {
            return None;
        }

        let strides = meta
            .as_ref()
            .map_or(actual_info.stride(), |meta| meta.stride());

        let offsets = meta
            .as_ref()
            .map_or(actual_info.offset(), |meta| meta.offset());

        let planes = info.n_planes() as usize;
        if strides.len() != planes || offsets.len() != planes {
            return None;
        }

        let stride = layout.canvas[0]
            * if config.planar { 1 } else { config.channels }
            * if config.tensor_float { 4 } else { 1 };

        let plane_size = stride * layout.canvas[1];
        for plane in 0..planes {
            if strides[plane] as usize != stride || offsets[plane] != plane * plane_size {
                return None;
            }
        }

        let mut output = gst::Buffer::new();
        output
            .get_mut()
            .unwrap()
            .append_memory(input.peek_memory(0).to_owned());

        Some(output)
    }

    fn prepare_conversion(&mut self, config: &Config, layout: &RoiLayout) -> Result<(), String> {
        let (content, _) = layout.geometry.sampling_dimensions();
        if self.converted.as_ref().is_some_and(|converted| {
            [
                converted.info.width() as usize,
                converted.info.height() as usize,
            ] == layout.canvas
                && converted.content == content
        }) {
            return Ok(());
        }

        let info = gst_video::VideoInfo::builder(
            config.video_format(),
            layout.canvas[0] as u32,
            layout.canvas[1] as u32,
        )
        .par(config.target_par)
        .fps(self.input_info.fps())
        .build()
        .map_err(|e| e.to_string())?;

        let converter = gst_hip::HipConverter::new(&self.device, &self.input_info, &info, None)
            .map_err(|e| e.to_string())?;
        converter.set_dest_width(content[0] as i32);
        converter.set_dest_height(content[1] as i32);
        converter.set_fill_border(content != layout.canvas);

        let pool = gst_hip::HipBufferPool::new(&self.device);
        let mut pool_config = pool.config();
        pool_config.set_params(Some(&info.to_caps().map_err(|e| e.to_string())?), 0, 0, 0);
        pool_config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
        pool.set_config(pool_config).map_err(|e| e.to_string())?;
        pool.set_active(true).map_err(|e| e.to_string())?;
        self.converted = Some(ConvertedVideo {
            converter,
            pool,
            info,
            content,
        });
        Ok(())
    }

    fn convert(
        &mut self,
        input: &gst::BufferRef,
        config: &Config,
        layout: &RoiLayout,
    ) -> Result<gst::Buffer, String> {
        self.prepare_conversion(config, layout)?;

        let input_range = self.input_info.colorimetry().range();
        let output_range = self.converted.as_ref().unwrap().info.colorimetry().range();
        if (input_range == output_range
            || input_range == gst_video::VideoColorRange::Unknown
            || output_range == gst_video::VideoColorRange::Unknown)
            && let Some(output) = self.share_tensor(input, &self.input_info, config, layout)
        {
            gst::debug!(CAT, "HIP preprocessing mode: fast path (input memory)");
            return Ok(output);
        }

        let converted = self.converted.as_ref().unwrap();
        let mut intermediate = converted
            .pool
            .acquire_buffer(None)
            .map_err(|e| e.to_string())?;

        let converter = converted.converter.clone();
        let converted_info = converted.info.clone();

        let source = Self::source_buffer(input)?;
        let stream = self.device.stream();
        if !stream.schedule_wait_buffer(&source) {
            return Err("Couldn't schedule HIP input wait".into());
        }

        let fence_data = self
            .fence_data_pool
            .acquire()
            .ok_or("Couldn't acquire HIP fence data")?;

        let mut result: Result<gst::Buffer, String> = (|| {
            converter
                .convert_frame(
                    &source,
                    intermediate.get_mut().unwrap(),
                    &fence_data,
                    Some(&stream),
                )
                .map_err(|e| e.to_string())?;

            if let Some(mut output) =
                self.share_tensor(&intermediate, &converted_info, config, layout)
            {
                gst::ParentBufferMeta::add(output.get_mut().unwrap(), &intermediate);
                gst::debug!(CAT, "HIP preprocessing mode: fast path (converter output)");
                return Ok(output);
            }

            self.ensure_capacity(layout.size)?;
            let mut output = gst::Buffer::new();
            output.get_mut().unwrap().append_memory(
                self.allocator
                    .as_ref()
                    .unwrap()
                    .acquire_memory()
                    .map_err(|e| e.to_string())?,
            );
            output.get_mut().unwrap().set_size(layout.size);

            let src = intermediate
                .peek_memory(0)
                .downcast_memory_ref::<gst_hip::HipMemory>()
                .ok_or("Expected HIP intermediate memory")?;

            let info = src.video_info().ok_or("Missing HIP video layout")?;
            let meta = intermediate.meta::<gst_video::VideoMeta>();
            let strides = meta.as_ref().map_or(info.stride(), |meta| meta.stride());
            let offsets = meta.as_ref().map_or(info.offset(), |meta| meta.offset());
            let (content, _) = layout.geometry.sampling_dimensions();
            let mut params = PackParameters {
                width: layout.canvas[0] as u32,
                height: layout.canvas[1] as u32,
                channels: config.channels as u32,
                planar: config.planar as u32,
                tensor_float: config.tensor_float as u32,
                video_float: config.video_float as u32,
                content_width: content[0] as u32,
                content_height: content[1] as u32,
                strides: [0; 3],
                scales: [1.0; 3],
                offsets: [0.0; 3],
            };

            for (dest, &stride) in params.strides.iter_mut().zip(strides) {
                *dest = stride as u32;
            }

            if let NormParam::ScaleOffset { scales, offsets } = &config.normalization {
                for c in 0..config.channels {
                    params.scales[c] = scales[c] as f32;
                    params.offsets[c] = offsets[c] as f32;
                }
            }

            let src_map = src.map_readable_hip().map_err(|e| e.to_string())?;
            let dst = output
                .get_mut()
                .unwrap()
                .peek_memory_mut(0)
                .map_err(|e| e.to_string())?
                .downcast_memory_mut::<gst_hip::HipMemory>()
                .ok_or("Expected HIP tensor memory")?;

            let dst_map = dst.map_writable_hip().map_err(|e| e.to_string())?;
            self.args.clear();

            unsafe {
                for offset in offsets.iter().copied().chain([0; 3]).take(3) {
                    self.args
                        .add_memory(src_map.memory(), offset)
                        .map_err(|e| e.to_string())?;
                }
                self.args
                    .add_memory(dst_map.memory(), 0)
                    .map_err(|e| e.to_string())?;
                self.args
                    .add_value(
                        std::slice::from_raw_parts(
                            (&params as *const PackParameters).cast(),
                            std::mem::size_of::<PackParameters>(),
                        ),
                        std::mem::align_of::<PackParameters>(),
                    )
                    .map_err(|e| e.to_string())?;
                self.module
                    .launch(
                        "pack_tensor",
                        Some(&stream),
                        [params.width.div_ceil(64), params.height, 1],
                        [64, 1, 1],
                        &self.args,
                    )
                    .map_err(|e| e.to_string())?;
            }
            drop(dst_map);
            drop(src_map);

            gst::debug!(
                CAT,
                "HIP preprocessing mode: fast path (converter + packing)"
            );
            Ok(output)
        })();
        self.args.clear();

        if let Some(event) = stream.record_event() {
            let gc_data = (intermediate, source, self.module.clone());
            event.set_notify(move || drop((fence_data, gc_data)));
            if let Ok(output) = &mut result {
                gst_hip::buffer_set_event(output.get_mut().unwrap(), Some(&event));
            }
        } else {
            let _ = stream.synchronize();
            return Err("Couldn't record HIP tensor completion".into());
        }
        result
    }

    fn source_buffer(input: &gst::BufferRef) -> Result<gst::Buffer, String> {
        let mut source = gst::Buffer::new();
        for memory in input.iter_memories_owned() {
            source.get_mut().unwrap().append_memory(memory);
        }

        if let Some(meta) = input.meta::<gst_video::VideoMeta>() {
            gst_video::VideoMeta::add_full(
                source.get_mut().unwrap(),
                meta.video_frame_flags(),
                meta.format(),
                meta.width(),
                meta.height(),
                meta.offset(),
                meta.stride(),
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(source)
    }

    fn run(
        &mut self,
        input: &gst::BufferRef,
        config: &Config,
        layouts: &[RoiLayout],
        size: usize,
        replicate: bool,
    ) -> Result<gst::Buffer, String> {
        gst::debug!(
            CAT,
            "HIP preprocessing mode: full conversion (ROI sampling)"
        );

        let mut output = gst::Buffer::new();
        output.get_mut().unwrap().append_memory(
            self.allocator
                .as_ref()
                .unwrap()
                .acquire_memory()
                .map_err(|e| e.to_string())?,
        );
        output.get_mut().unwrap().set_size(size);

        let mut intermediate = self
            .video_pool
            .acquire_buffer(None)
            .map_err(|e| e.to_string())?;

        let source = Self::source_buffer(input)?;

        let stream = self.device.stream();
        if !stream.schedule_wait_buffer(&source) {
            return Err("Couldn't schedule HIP input wait".into());
        }

        let fence_data = self
            .fence_data_pool
            .acquire()
            .ok_or("Couldn't acquire HIP fence data")?;

        self.converter
            .convert_frame(
                &source,
                intermediate.get_mut().unwrap(),
                &fence_data,
                Some(&stream),
            )
            .map_err(|e| e.to_string())?;

        let result: Result<(), String> = (|| {
            let src = intermediate
                .peek_memory(0)
                .downcast_memory_ref::<gst_hip::HipMemory>()
                .ok_or("Expected HIP intermediate memory")?;

            let info = src.video_info().ok_or("Missing HIP video layout")?;

            let texture = src
                .texture(
                    0,
                    gst_hip::HipTextureFilterMode::Linear,
                    gst_hip::HipTextureAddressMode::Clamp,
                )
                .map_err(|e| e.to_string())?;

            let dst = output
                .get_mut()
                .unwrap()
                .peek_memory_mut(0)
                .map_err(|e| e.to_string())?
                .downcast_memory_mut::<gst_hip::HipMemory>()
                .ok_or("Expected HIP tensor memory")?;

            let dst_map = dst.map_writable_hip().map_err(|e| e.to_string())?;

            let mut offset = 0;
            for layout in layouts {
                let (content, roi) = layout.geometry.sampling_dimensions();
                let mut params = Parameters {
                    width: layout.canvas[0] as u32,
                    height: layout.canvas[1] as u32,
                    channels: config.channels as u32,
                    planar: config.planar as u32,
                    output_offset: offset,
                    tensor_float: config.tensor_float as u32,
                    numeric_scale: if config.video_float { 1.0 } else { 255.0 },
                    replicate: replicate as u32,
                    video_width: info.width(),
                    video_height: info.height(),
                    content_width: content[0] as f32,
                    content_height: content[1] as f32,
                    roi_width: roi[0] as f32,
                    roi_height: roi[1] as f32,
                    matrix: layout.geometry.matrix()[..6].try_into().unwrap(),
                    scales: [1.0; 3],
                    offsets: [0.0; 3],
                };

                if let NormParam::ScaleOffset { scales, offsets } = &config.normalization {
                    for c in 0..config.channels {
                        params.scales[c] = scales[c] as f32;
                        params.offsets[c] = offsets[c] as f32;
                    }
                }

                self.args.clear();

                unsafe {
                    self.args.add(texture.as_raw());

                    self.args
                        .add_memory(dst_map.memory(), 0)
                        .map_err(|e| e.to_string())?;

                    // Parameters contains only initialized 32-bit fields, with no padding.
                    self.args
                        .add_value(
                            std::slice::from_raw_parts(
                                (&params as *const Parameters).cast(),
                                std::mem::size_of::<Parameters>(),
                            ),
                            std::mem::align_of::<Parameters>(),
                        )
                        .map_err(|e| e.to_string())?;

                    self.module
                        .launch(
                            "preprocess",
                            Some(&stream),
                            [params.width.div_ceil(64), params.height, 1],
                            [64, 1, 1],
                            &self.args,
                        )
                        .map_err(|e| e.to_string())?;
                }

                offset += layout.size as u32;
            }

            Ok(())
        })();

        self.args.clear();

        let gc_data = (intermediate, source, self.module.clone());
        if let Some(event) = stream.record_event() {
            event.set_notify(move || drop((fence_data, gc_data)));
            gst_hip::buffer_set_event(output.get_mut().unwrap(), Some(&event));
        } else {
            let _ = stream.synchronize();

            return Err("Couldn't record HIP tensor completion".into());
        }

        result?;

        Ok(output)
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        let _ = self.video_pool.set_active(false);
        if let Some(allocator) = &self.allocator {
            allocator.set_active(false);
        }
    }
}
