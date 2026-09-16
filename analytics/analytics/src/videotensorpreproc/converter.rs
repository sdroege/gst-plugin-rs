// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*};
use gst_analytics::{ModelInfo, Tensor, TensorDataType, TensorDimOrder};

#[path = "converter_cpu.rs"]
mod cpu;

#[cfg(hip_enabled)]
#[path = "converter_hip.rs"]
mod hip;

#[cfg(windows)]
#[path = "converter_d3d12.rs"]
mod d3d12;

#[derive(Default)]
pub(super) struct HwDeviceState {
    #[cfg(windows)]
    d3d12: gst_d3d12::D3D12DeviceState,
    #[cfg(hip_enabled)]
    hip: gst_hip::HipDeviceState,
}

impl HwDeviceState {
    pub fn select_device(
        &self,
        _element: &impl IsA<gst::Element>,
        _caps: &gst::CapsRef,
    ) -> Result<Device, String> {
        #[cfg(windows)]
        if _caps
            .features(0)
            .is_some_and(|features| features.contains(gst_d3d12::CAPS_FEATURE_MEMORY_D3D12_MEMORY))
        {
            self.d3d12
                .ensure_element_data(_element, -1)
                .map_err(|err| format!("Couldn't ensure D3D12 device: {err}"))?;
            return Ok(Device::D3D12(self.d3d12.device().unwrap()));
        }

        #[cfg(hip_enabled)]
        if _caps
            .features(0)
            .is_some_and(|features| features.contains(gst_hip::CAPS_FEATURE_MEMORY_HIP_MEMORY))
        {
            return Ok(Device::Hip);
        }

        Ok(Device::Cpu)
    }

    pub fn handle_set_context(&self, _element: &impl IsA<gst::Element>, _context: &gst::Context) {
        #[cfg(windows)]
        self.d3d12.handle_set_context(_element, _context, -1);

        #[cfg(hip_enabled)]
        self.hip
            .handle_set_context(_element, _context, gst_hip::HipVendor::Unknown, -1);
    }

    pub fn handle_context_query(
        &self,
        _element: &impl IsA<gst::Element>,
        _query: &mut gst::QueryRef,
    ) -> bool {
        #[cfg(windows)]
        if let gst::QueryViewMut::Context(context) = _query.view_mut()
            && self.d3d12.handle_context_query(_element, context)
        {
            return true;
        }

        #[cfg(hip_enabled)]
        if let gst::QueryViewMut::Context(context) = _query.view_mut()
            && self.hip.handle_context_query(_element, context)
        {
            return true;
        }

        false
    }

    pub fn clear(&self) {
        #[cfg(windows)]
        self.d3d12.clear();

        #[cfg(hip_enabled)]
        self.hip.clear();
    }
}

pub(super) enum Device {
    Cpu,
    #[cfg(hip_enabled)]
    Hip,
    #[cfg(windows)]
    D3D12(gst_d3d12::D3D12Device),
}

enum Backend {
    #[cfg(hip_enabled)]
    Hip {
        converter: Option<hip::Converter>,
        layout: RoiLayout,
    },
    Cpu(cpu::Converter),
    #[cfg(windows)]
    D3D12 {
        converter: Option<d3d12::Converter>,
        layout: RoiLayout,
    },
}

enum RoiBackend {
    #[cfg(hip_enabled)]
    Hip(Option<hip::Converter>),
    Cpu(cpu::RoiConverter),
    #[cfg(windows)]
    D3D12(Option<d3d12::Converter>),
}

#[derive(Clone)]
enum NormParam {
    Identity,
    ScaleOffset { scales: Vec<f64>, offsets: Vec<f64> },
}

impl NormParam {
    fn is_identity(&self) -> bool {
        matches!(self, Self::Identity)
    }
}

#[derive(Clone)]
pub(super) struct Config {
    width: Option<usize>,
    height: Option<usize>,
    target_par: gst::Fraction,
    width_dim: usize,
    height_dim: usize,
    channels: usize,
    planar: bool,
    tensor_float: bool,
    video_float: bool,
    dims: Vec<i32>,
    id: glib::Quark,
    normalization: NormParam,
}

impl Config {
    pub fn dynamic_batch(&self) -> bool {
        self.dims.len() == 4 && self.dims[0] == -1
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: &ModelInfo,
        name: &str,
        caps: &gst::CapsRef,
        inferred_channels_first: Option<bool>,
        dims: &[i32],
        data_type: TensorDataType,
        order: TensorDimOrder,
        id: glib::Quark,
    ) -> Result<Self, String> {
        // TODO: Add support for column-major tensors.
        // ONNX uses row-major input tensors
        if order != TensorDimOrder::RowMajor {
            return Err("Unsupported dimension order".into());
        }

        if !(2..=4).contains(&dims.len()) || dims.iter().any(|&d| d == 0 || d < -1) {
            return Err("Invalid tensor dimensions".into());
        }

        let s = caps.structure(0).ok_or("Missing input caps")?;

        if s.name() != "video/x-raw" {
            return Err("Not a raw video caps in modelinfo".into());
        }

        let target_par = if s.has_field("pixel-aspect-ratio") {
            s.get::<gst::Fraction>("pixel-aspect-ratio")
                .map_err(|e| format!("Invalid model pixel-aspect-ratio: {e}"))?
        } else {
            gst::Fraction::new(1, 1)
        };

        if target_par.numer() <= 0 || target_par.denom() <= 0 {
            return Err(format!("Invalid target PAR {target_par}"));
        }

        let format = s.get::<&str>("format").map_err(|e| e.to_string())?;
        // Since tensor data uses native endianness, ignore endian suffix in model-info
        let (channels, mut planar, video_float) = match format {
            "RGB" => (3, false, false),
            "RGBP" => (3, true, false),
            "GRAY8" => (1, false, false),
            "RGB_F32" | "RGB_F32LE" | "RGB_F32BE" => (3, false, true),
            "RGBP_F32" | "RGBP_F32LE" | "RGBP_F32BE" => (3, true, true),
            "GRAY_F32" | "GRAY_F32LE" | "GRAY_F32BE" => (1, false, true),
            _ => return Err(format!("Unsupported model video format {format}")),
        };

        if let Some(channels_first) = inferred_channels_first {
            planar = channels_first;
        }

        let tensor_float = match data_type {
            TensorDataType::Uint8 => false,
            TensorDataType::Float32 => true,
            _ => return Err("Unsupported tensor data type".into()),
        };

        let shape = if dims.len() == 4 {
            if dims[0] != 1 && dims[0] != -1 {
                return Err("Only batch size 1 or dynamic batch dim is supported".into());
            }
            &dims[1..]
        } else {
            dims
        };

        if inferred_channels_first.is_none() {
            match shape {
                [c, _, last] if *c == channels && *last != channels => planar = true,
                [first, _, c] if *c == channels && *first != channels => planar = false,
                _ => (),
            }
        }

        let (height, width) = match shape {
            [h, w] if channels == 1 => (*h, *w),
            [c, h, w] if planar && *c == channels => (*h, *w),
            [h, w, c] if !planar && *c == channels => (*h, *w),
            [1, h, w] if channels == 1 => {
                planar = true;
                (*h, *w)
            }
            _ => return Err(format!("Dimensions {dims:?} do not match {format}")),
        };

        let first_spatial_dim =
            usize::from(dims.len() == 4) + usize::from(shape.len() == 3 && planar);

        for (field, expected) in [("width", width), ("height", height)] {
            if expected > 0
                && s.has_field(field)
                && s.get::<i32>(field).map_err(|e| e.to_string())? != expected
            {
                return Err(format!("Model caps {field} mismatch with dimensions"));
            }
        }

        let channels = channels as usize;
        let (scales, offsets) = model
            .input_scales_offsets(
                name,
                &vec![0.0; channels],
                &vec![if video_float { 1.0 } else { 255.0 }; channels],
            )
            .ok_or("Couldn't get offset/scale")?;

        // Same epsilon values as gst_onnx_importer_config_needs_normalization()
        let epsilon = if video_float { 1e-6 } else { 0.001 };
        let normalization = if scales
            .iter()
            .zip(offsets.iter())
            .any(|(&scale, &offset)| (scale - 1.0).abs() > epsilon || offset.abs() > epsilon)
        {
            NormParam::ScaleOffset {
                scales: scales.to_vec(),
                offsets: offsets.to_vec(),
            }
        } else {
            NormParam::Identity
        };

        let width = if width > 0 {
            Some(width as usize)
        } else {
            None
        };

        let height = if height > 0 {
            Some(height as usize)
        } else {
            None
        };

        Ok(Self {
            width,
            height,
            target_par,
            width_dim: first_spatial_dim + 1,
            height_dim: first_spatial_dim,
            channels,
            planar,
            tensor_float,
            video_float,
            dims: dims.to_vec(),
            id,
            normalization,
        })
    }

    fn video_format(&self) -> gst_video::VideoFormat {
        use gst_video::VideoFormat;

        match (
            self.channels,
            self.planar,
            self.video_float,
            cfg!(target_endian = "little"),
        ) {
            (1, _, false, _) => VideoFormat::Gray8,
            (_, false, false, _) => VideoFormat::Rgb,
            (_, true, false, _) => VideoFormat::Rgbp,
            (1, _, true, true) => VideoFormat::GrayF32le,
            (1, _, true, false) => VideoFormat::GrayF32be,
            (_, false, true, true) => VideoFormat::RgbF32le,
            (_, false, true, false) => VideoFormat::RgbF32be,
            (_, true, true, true) => VideoFormat::RgbpF32le,
            (_, true, true, false) => VideoFormat::RgbpF32be,
        }
    }
}

struct TensorLayout {
    canvas: [usize; 2],
    plane_size: usize,
    stride: usize,
    dims: Vec<usize>,
}

pub(super) struct Converter {
    input_info: gst_video::VideoInfo,
    config: Config,
    layout: TensorLayout,
    tensor_rect: gst_video::VideoRectangle,
    video_rect: gst_video::VideoRectangle,
    backend: Backend,
}

impl Converter {
    pub fn new(
        input: &gst_video::VideoInfo,
        config: Config,
        device: Device,
    ) -> Result<Self, String> {
        let [src_x, src_y, src_w, src_h] = [0, 0, input.width(), input.height()];
        let aspect =
            gst_video::calculate_display_ratio(src_w, src_h, input.par(), config.target_par)
                .ok_or("Couldn't calculate target PAR")?;

        let num = aspect.numer() as u64;
        let den = aspect.denom() as u64;
        let dynamic_size = |value: Option<u64>| -> Result<usize, String> {
            value
                .and_then(|v| i32::try_from(v).ok())
                .map(|v| v.max(1) as usize)
                .ok_or_else(|| "Invalid video size".into())
        };

        let resize_to_fit = |w: usize, h: usize| -> Result<_, String> {
            if gst::Fraction::new(w as i32, h as i32) <= aspect {
                Ok((
                    w,
                    (w as u64)
                        .mul_div_floor(den, num)
                        .ok_or("Resize height overflow")? as usize,
                    [w, h],
                ))
            } else {
                Ok((
                    (h as u64)
                        .mul_div_floor(num, den)
                        .ok_or("Resize width overflow")? as usize,
                    h,
                    [w, h],
                ))
            }
        };

        let (width, height, canvas) = match (config.width, config.height) {
            (Some(w), Some(h)) => resize_to_fit(w, h)?,
            (None, Some(h)) => {
                let w = dynamic_size((h as u64).mul_div_round(num, den))?;
                (w, h, [w, h])
            }
            (Some(w), None) => {
                let h = dynamic_size((w as u64).mul_div_round(den, num))?;
                (w, h, [w, h])
            }
            (None, None) => resize_to_fit(src_w as usize, src_h as usize)?,
        };

        if width == 0 || height == 0 {
            return Err("Couldn't calculate video size".into());
        }

        let info = gst_video::VideoInfo::builder(
            config.video_format(),
            canvas[0] as u32,
            canvas[1] as u32,
        )
        .par(config.target_par)
        .fps(input.fps())
        .build()
        .map_err(|e| e.to_string())?;

        let bytes = if config.tensor_float { 4 } else { 1 };
        let plane_channels = if info.n_planes() > 1 {
            1
        } else {
            config.channels
        };

        let tensor_stride = canvas[0]
            .checked_mul(plane_channels)
            .and_then(|n| n.checked_mul(bytes))
            .ok_or("Tensor size overflow")?;

        let plane_size = tensor_stride
            .checked_mul(canvas[1])
            .ok_or("Tensor size overflow")?;

        let tensor_size = plane_size
            .checked_mul(info.n_planes() as usize)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("Tensor size exceeds the buffer pool size limit")?;

        let dest_x = 0;
        let dest_y = 0;
        let tensor_rect =
            gst_video::VideoRectangle::new(dest_x, dest_y, width as i32, height as i32);
        let video_rect =
            gst_video::VideoRectangle::new(src_x as i32, src_y as i32, src_w as i32, src_h as i32);

        let mut tensor_dims: Vec<_> = config.dims.iter().map(|&d| d as usize).collect();
        if tensor_dims.len() == 4 {
            tensor_dims[0] = 1;
        }
        tensor_dims[config.width_dim] = canvas[0];
        tensor_dims[config.height_dim] = canvas[1];

        let layout = TensorLayout {
            canvas,
            plane_size,
            stride: tensor_stride,
            dims: tensor_dims,
        };

        #[cfg(hip_enabled)]
        let device = if matches!(device, Device::Hip)
            && !gst_hip::check_format_support(input.format())
                .ok_or("Couldn't query HIP format support")?
                .contains(gst_hip::HipFormatSupport::CONVERTER)
        {
            Device::Cpu
        } else {
            device
        };

        let backend = match device {
            Device::Cpu => Backend::Cpu(cpu::Converter::new(
                input,
                info,
                &config,
                &layout,
                tensor_size,
                &tensor_rect,
            )?),
            #[cfg(hip_enabled)]
            Device::Hip => Backend::Hip {
                converter: None,
                layout: RoiLayout {
                    canvas,
                    content_width: width,
                    geometry: super::sampler::Geometry::new(
                        super::sampler::Roi {
                            x: video_rect.x as f64,
                            y: video_rect.y as f64,
                            width: src_w,
                            height: src_h,
                            rotation: 0.0,
                        },
                        [width, height],
                    ),
                    size: tensor_size as usize,
                    dims: layout.dims.clone(),
                },
            },
            #[cfg(windows)]
            Device::D3D12(device) => Backend::D3D12 {
                converter: Some(d3d12::Converter::new(input, &config, device)?),
                layout: RoiLayout {
                    canvas,
                    content_width: width,
                    geometry: super::sampler::Geometry::new(
                        super::sampler::Roi {
                            x: video_rect.x as f64,
                            y: video_rect.y as f64,
                            width: src_w,
                            height: src_h,
                            rotation: 0.0,
                        },
                        [width, height],
                    ),
                    size: tensor_size as usize,
                    dims: layout.dims.clone(),
                },
            },
        };

        Ok(Self {
            input_info: input.clone(),
            config,
            layout,
            tensor_rect,
            video_rect,
            backend,
        })
    }

    pub fn update_device(
        &mut self,
        _buffer: &gst::BufferRef,
        _hw_device: &HwDeviceState,
    ) -> Result<(), String> {
        match &mut self.backend {
            Backend::Cpu(_) => Ok(()),
            #[cfg(windows)]
            Backend::D3D12 { converter, .. } => update_d3d12_device(
                converter,
                &self.input_info,
                &self.config,
                _buffer,
                &_hw_device.d3d12,
            ),
            #[cfg(hip_enabled)]
            Backend::Hip { converter, .. } => update_hip_device(
                converter,
                &self.input_info,
                &self.config,
                _buffer,
                &_hw_device.hip,
            ),
        }
    }

    pub fn is_available(&self) -> bool {
        match &self.backend {
            #[cfg(windows)]
            Backend::D3D12 { converter, .. } => converter.is_some(),
            #[cfg(hip_enabled)]
            Backend::Hip { converter, .. } => converter.is_some(),
            Backend::Cpu(_) => true,
        }
    }

    pub fn tensor_to_video_rectangles(
        &self,
    ) -> (&gst_video::VideoRectangle, &gst_video::VideoRectangle) {
        (&self.tensor_rect, &self.video_rect)
    }

    fn tensor(config: &Config, layout: &TensorLayout, buffer: gst::Buffer) -> Tensor {
        Tensor::new_simple(
            config.id,
            if config.tensor_float {
                TensorDataType::Float32
            } else {
                TensorDataType::Uint8
            },
            buffer,
            TensorDimOrder::RowMajor,
            &layout.dims,
        )
    }

    pub fn process(&mut self, input: &gst::BufferRef) -> Result<Tensor, String> {
        let buffer = match &mut self.backend {
            Backend::Cpu(cpu) => cpu.process(input, &self.config, &self.layout)?,
            #[cfg(hip_enabled)]
            Backend::Hip { converter, layout } => converter
                .as_mut()
                .ok_or("HIP backend unavailable")?
                .process(input, &self.config, std::slice::from_ref(layout), false)?
                .remove(0),
            #[cfg(windows)]
            Backend::D3D12 { converter, layout } => converter
                .as_mut()
                .ok_or("D3D12 backend unavailable")?
                .process(input, &self.config, std::slice::from_ref(layout), false)?
                .remove(0),
        };

        Ok(Self::tensor(&self.config, &self.layout, buffer))
    }
}

struct RoiLayout {
    canvas: [usize; 2],
    content_width: usize,
    geometry: super::sampler::Geometry,
    size: usize,
    dims: Vec<usize>,
}

pub(super) struct RoiConverter {
    input_info: gst_video::VideoInfo,
    config: Config,
    backend: RoiBackend,
}

impl RoiConverter {
    pub fn new(
        input: &gst_video::VideoInfo,
        config: Config,
        device: Device,
    ) -> Result<Self, String> {
        if config.height.is_none() {
            return Err("CTC ROI preprocessing requires a fixed model input height".into());
        }

        #[cfg(hip_enabled)]
        let device = if matches!(device, Device::Hip)
            && !gst_hip::check_format_support(input.format())
                .ok_or("Couldn't query HIP format support")?
                .contains(gst_hip::HipFormatSupport::CONVERTER)
        {
            Device::Cpu
        } else {
            device
        };

        let backend = match device {
            #[cfg(hip_enabled)]
            Device::Hip => RoiBackend::Hip(None),
            Device::Cpu => RoiBackend::Cpu(cpu::RoiConverter::new(input, &config)?),
            #[cfg(windows)]
            Device::D3D12(device) => {
                RoiBackend::D3D12(Some(d3d12::Converter::new(input, &config, device)?))
            }
        };

        Ok(Self {
            input_info: input.clone(),
            config,
            backend,
        })
    }

    pub fn update_device(
        &mut self,
        _buffer: &gst::BufferRef,
        _hw_device: &HwDeviceState,
    ) -> Result<(), String> {
        match &mut self.backend {
            RoiBackend::Cpu(_) => Ok(()),
            #[cfg(windows)]
            RoiBackend::D3D12(converter) => update_d3d12_device(
                converter,
                &self.input_info,
                &self.config,
                _buffer,
                &_hw_device.d3d12,
            ),
            #[cfg(hip_enabled)]
            RoiBackend::Hip(converter) => update_hip_device(
                converter,
                &self.input_info,
                &self.config,
                _buffer,
                &_hw_device.hip,
            ),
        }
    }

    pub fn is_available(&self) -> bool {
        match &self.backend {
            #[cfg(windows)]
            RoiBackend::D3D12(converter) => converter.is_some(),
            #[cfg(hip_enabled)]
            RoiBackend::Hip(converter) => converter.is_some(),
            RoiBackend::Cpu(_) => true,
        }
    }

    pub fn input_info(&self) -> &gst_video::VideoInfo {
        &self.input_info
    }

    pub fn tensor_width(&self, roi: super::sampler::Roi) -> Result<usize, String> {
        let (canvas, _, _) = Self::geometry(&self.config, &self.input_info, roi)?;

        Ok(canvas[0])
    }

    fn geometry(
        config: &Config,
        info: &gst_video::VideoInfo,
        roi: super::sampler::Roi,
    ) -> Result<([usize; 2], usize, super::sampler::Geometry), String> {
        let height = config.height.unwrap();
        let aspect = gst_video::calculate_display_ratio(
            roi.width,
            roi.height,
            info.par(),
            config.target_par,
        )
        .ok_or("Couldn't calculate target PAR")?;

        let width = (height as u64)
            .mul_div_ceil(aspect.numer() as u64, aspect.denom() as u64)
            .and_then(|v| i32::try_from(v).ok())
            .ok_or("Invalid ROI tensor width")?
            .max(1) as usize;

        let canvas = [config.width.unwrap_or(width), height];
        let content_width = width.min(canvas[0]);
        let geometry = super::sampler::Geometry::new(roi, [content_width, height]);

        Ok((canvas, content_width, geometry))
    }

    fn layout(&self, roi: super::sampler::Roi) -> Result<RoiLayout, String> {
        let config = &self.config;
        let (canvas, content_width, geometry) = Self::geometry(config, &self.input_info, roi)?;
        let bytes = if config.tensor_float { 4 } else { 1 };
        let size = canvas[0]
            .checked_mul(canvas[1])
            .and_then(|n| n.checked_mul(config.channels))
            .and_then(|n| n.checked_mul(bytes))
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("Tensor size overflow")? as usize;

        let mut dims: Vec<_> = config.dims.iter().map(|&d| d as usize).collect();
        if dims.len() == 4 {
            dims[0] = 1;
        }
        dims[config.width_dim] = canvas[0];
        dims[config.height_dim] = canvas[1];

        Ok(RoiLayout {
            canvas,
            content_width,
            geometry,
            size,
            dims,
        })
    }

    fn tensor(&self, buffer: gst::Buffer, dims: &[usize]) -> Tensor {
        Tensor::new_simple(
            self.config.id,
            if self.config.tensor_float {
                TensorDataType::Float32
            } else {
                TensorDataType::Uint8
            },
            buffer,
            TensorDimOrder::RowMajor,
            dims,
        )
    }

    pub fn process(
        &mut self,
        input: &gst::BufferRef,
        rois: &[super::sampler::Roi],
    ) -> Result<Vec<(Tensor, [f32; 9])>, String> {
        if rois.is_empty() {
            return Ok(Vec::new());
        }

        let layouts = rois
            .iter()
            .map(|&roi| self.layout(roi))
            .collect::<Result<Vec<_>, _>>()?;

        let buffers =
            match &mut self.backend {
                RoiBackend::Cpu(cpu) => cpu.process(input, &self.config, &layouts)?,
                #[cfg(hip_enabled)]
                RoiBackend::Hip(gpu) => gpu.as_mut().ok_or("HIP backend unavailable")?.process(
                    input,
                    &self.config,
                    &layouts,
                    true,
                )?,
                #[cfg(windows)]
                RoiBackend::D3D12(gpu) => gpu
                    .as_mut()
                    .ok_or("D3D12 backend unavailable")?
                    .process(input, &self.config, &layouts, true)?,
            };

        Ok(buffers
            .into_iter()
            .zip(layouts)
            .map(|(buffer, layout)| (self.tensor(buffer, &layout.dims), *layout.geometry.matrix()))
            .collect())
    }

    pub fn process_batch(
        &mut self,
        input: &gst::BufferRef,
        rois: &[super::sampler::Roi],
    ) -> Result<(Tensor, Vec<[f32; 9]>), String> {
        if rois.is_empty() {
            return Err("Cannot create an empty ROI batch".into());
        }

        let layouts = rois
            .iter()
            .map(|&roi| self.layout(roi))
            .collect::<Result<Vec<_>, _>>()?;

        let first = &layouts[0];
        if layouts.iter().any(|layout| layout.canvas != first.canvas) {
            return Err("ROI batch requires equal tensor widths".into());
        }

        let size = first
            .size
            .checked_mul(rois.len())
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("Tensor size overflow")?;

        let mut dims = first.dims.clone();
        dims[0] = rois.len();

        let buffer = match &mut self.backend {
            RoiBackend::Cpu(cpu) => {
                cpu.process_batch(input, &self.config, &layouts, size as usize)?
            }
            #[cfg(hip_enabled)]
            RoiBackend::Hip(gpu) => gpu
                .as_mut()
                .ok_or("HIP backend unavailable")?
                .process_batch(input, &self.config, &layouts, size as usize)?,
            #[cfg(windows)]
            RoiBackend::D3D12(gpu) => gpu
                .as_mut()
                .ok_or("D3D12 backend unavailable")?
                .process_batch(input, &self.config, &layouts, size as usize)?,
        };

        let transforms = layouts
            .iter()
            .map(|layout| *layout.geometry.matrix())
            .collect();

        Ok((self.tensor(buffer, &dims), transforms))
    }
}

#[cfg(windows)]
fn update_d3d12_device(
    converter: &mut Option<d3d12::Converter>,
    input: &gst_video::VideoInfo,
    config: &Config,
    buffer: &gst::BufferRef,
    device_state: &gst_d3d12::D3D12DeviceState,
) -> Result<(), String> {
    use gst_d3d12::prelude::*;

    let incoming_device = (|| {
        let mut device: Option<gst_d3d12::D3D12Device> = None;
        for memory in buffer.iter_memories() {
            let memory = memory
                .downcast_memory_ref::<gst_d3d12::D3D12Memory>()
                .ok_or("Expected D3D12 input memory")?;
            let memory_device = memory.device();
            if device
                .as_ref()
                .is_some_and(|device| !device.is_equal(Some(&memory_device)))
            {
                return Err("Input memories use incompatible D3D12 devices");
            }
            device = Some(memory_device);
        }
        device.ok_or("Missing D3D12 input memory")
    })();

    let incoming_device = match incoming_device {
        Ok(device) => device,
        Err(err) => {
            drop(converter.take());
            return Err(err.into());
        }
    };

    if converter
        .as_ref()
        .is_some_and(|converter| converter.device().is_equal(Some(&incoming_device)))
    {
        return Ok(());
    }

    drop(converter.take());
    device_state.set(Some(&incoming_device));
    *converter = Some(d3d12::Converter::new(input, config, incoming_device)?);
    Ok(())
}

#[cfg(hip_enabled)]
fn update_hip_device(
    converter: &mut Option<hip::Converter>,
    input: &gst_video::VideoInfo,
    config: &Config,
    buffer: &gst::BufferRef,
    device_state: &gst_hip::HipDeviceState,
) -> Result<(), String> {
    use gst_hip::prelude::*;

    let incoming_device = (|| {
        let mut device: Option<gst_hip::HipDevice> = None;
        for memory in buffer.iter_memories() {
            let memory = memory
                .downcast_memory_ref::<gst_hip::HipMemory>()
                .ok_or("Expected Hip input memory")?;
            let memory_device = memory.device();
            if device
                .as_ref()
                .is_some_and(|device| !device.is_equal(&memory_device))
            {
                return Err("Input memories use incompatible Hip devices");
            }
            device = Some(memory_device);
        }
        device.ok_or("Missing Hip input memory")
    })();

    let incoming_device = match incoming_device {
        Ok(device) => device,
        Err(err) => {
            drop(converter.take());
            return Err(err.into());
        }
    };

    if converter
        .as_ref()
        .is_some_and(|converter| converter.device().is_equal(&incoming_device))
    {
        return Ok(());
    }

    drop(converter.take());
    device_state.set(Some(&incoming_device));
    *converter = Some(hip::Converter::new(input, config, incoming_device)?);
    Ok(())
}
