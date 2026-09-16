// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use super::super::imp::CAT;
use super::{Config, NormParam, RoiLayout};
use gst::prelude::*;
use gst_d3d12::prelude::*;
use std::{
    mem::{self, ManuallyDrop},
    sync::LazyLock,
};
use windows::{
    Win32::Graphics::{
        Direct3D::{Fxc::D3DCompile, ID3DBlob},
        Direct3D12::*,
        Dxgi::Common::*,
    },
    core::{Interface, s},
};

static SHADER: LazyLock<Result<ID3DBlob, String>> = LazyLock::new(|| unsafe {
    let source = include_str!("preprocess.hlsl");
    let mut blob = None;
    let mut errors = None;

    D3DCompile(
        source.as_ptr().cast(),
        source.len(),
        None,
        None,
        None,
        s!("main"),
        s!("cs_5_0"),
        0,
        0,
        &mut blob,
        Some(&mut errors),
    )
    .map_err(|err| {
        errors.map_or_else(
            || err.to_string(),
            |blob| {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    blob.GetBufferPointer().cast(),
                    blob.GetBufferSize(),
                ))
                .into_owned()
            },
        )
    })?;
    Ok(blob.unwrap())
});

const NORM_IDENTITY: u32 = 0;
const NORM_SCALE_OFFSET: u32 = 1;

#[repr(C)]
struct Constants {
    width: u32,
    height: u32,
    channels: u32,
    planar: u32,
    output_offset: u32,
    tensor_float: u32,
    norm_type: u32,
    replicate_border: u32,
    video_size: [f32; 2],
    content_size: [f32; 2],
    roi_size: [f32; 2],
    numeric_scale: f32,
    _padding0: f32,
    texture_size: [f32; 2],
    _padding1: [f32; 2],
    tensor_to_video_x: [f32; 4],
    tensor_to_video_y: [f32; 4],
    scales: [f32; 4],
    offsets: [f32; 4],
}

pub(super) struct Converter {
    device: gst_d3d12::D3D12Device,
    queue: gst_d3d12::D3D12CmdQueue,
    converter: gst_d3d12::D3D12Converter,
    video_pool: gst_d3d12::D3D12BufferPool,
    info: gst_video::VideoInfo,
    allocator: Option<gst_d3d12::D3D12PoolAllocator>,
    capacity: u64,
    ca_pool: gst_d3d12::D3D12CmdAllocPool,
    desc_pool: gst_d3d12::D3D12DescHeapPool,
    fence_pool: gst_d3d12::D3D12FenceDataPool,
    fence_value: u64,
    command_list: Option<ID3D12GraphicsCommandList>,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    fence: ID3D12Fence,
}

impl Converter {
    pub(super) fn device(&self) -> &gst_d3d12::D3D12Device {
        &self.device
    }

    pub(super) fn new(
        input: &gst_video::VideoInfo,
        config: &Config,
        device: gst_d3d12::D3D12Device,
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

        let queue = device.cmd_queue(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();

        let converter =
            gst_d3d12::D3D12Converter::new(&device, Some(&queue), input, &info, None, None, None)
                .ok_or("Couldn't create D3D12 converter")?;

        let video_pool = gst_d3d12::D3D12BufferPool::new(&device);
        let params = gst_d3d12::D3D12AllocationParams::new(
            &device,
            &info,
            gst_d3d12::D3D12AllocationFlags::D3d12AllocationFlagDefault,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
            D3D12_HEAP_FLAG_NONE,
        )
        .ok_or("Couldn't create video allocation params")?;

        let mut pool_config = video_pool.config();
        pool_config.set_params(Some(&info.to_caps().map_err(|e| e.to_string())?), 0, 0, 0);
        pool_config.set_d3d12_allocation_params(&params);
        video_pool
            .set_config(pool_config)
            .map_err(|e| e.to_string())?;

        video_pool.set_active(true).map_err(|e| e.to_string())?;

        let native = device.device_handle();
        let blob = ROOT_SIGNATURE.as_ref().map_err(Clone::clone)?;
        let root: ID3D12RootSignature = unsafe {
            native
                .CreateRootSignature(
                    0,
                    std::slice::from_raw_parts(
                        blob.GetBufferPointer().cast(),
                        blob.GetBufferSize(),
                    ),
                )
                .map_err(|e| e.to_string())?
        };
        let blob = SHADER.as_ref().map_err(Clone::clone)?;
        let mut desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: unsafe { blob.GetBufferPointer() },
                BytecodeLength: unsafe { blob.GetBufferSize() },
            },
            ..Default::default()
        };

        let pso = unsafe { native.CreateComputePipelineState(&desc) };
        unsafe { ManuallyDrop::drop(&mut desc.pRootSignature) };

        let pso = pso.map_err(|e| e.to_string())?;

        let ca_pool = gst_d3d12::D3D12CmdAllocPool::new(&native, D3D12_COMMAND_LIST_TYPE_DIRECT);

        let desc_pool = gst_d3d12::D3D12DescHeapPool::new(
            &native,
            &D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            },
        );

        let fence = queue.fence_handle();

        Ok(Self {
            device,
            queue,
            converter,
            video_pool,
            info,
            allocator: None,
            capacity: 0,
            ca_pool,
            desc_pool,
            command_list: None,
            fence_value: 0,
            fence_pool: gst_d3d12::D3D12FenceDataPool::new(),
            root,
            pso,
            fence,
        })
    }

    fn ensure_capacity(&mut self, size: usize) -> Result<(), String> {
        let alignment = D3D12_RAW_UAV_SRV_BYTE_ALIGNMENT as u64;
        let capacity = (size as u64)
            .checked_add(alignment - 1)
            .ok_or("Tensor size overflow")?
            / alignment
            * alignment;

        if capacity <= self.capacity {
            return Ok(());
        }

        // Shader offsets and raw UAV element counts are 32-bit
        if capacity > u32::MAX as u64 {
            return Err("Tensor exceeds shader address range".into());
        }

        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: capacity,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };

        let props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };

        let allocator = gst_d3d12::D3D12PoolAllocator::new(
            &self.device,
            &props,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_COMMON,
            None,
        )
        .ok_or("Couldn't create tensor pool allocator")?;

        if !allocator.set_active(true) {
            return Err("Couldn't activate tensor allocator".into());
        }

        if let Some(old) = self.allocator.replace(allocator) {
            old.set_active(false);
        }

        self.capacity = capacity;

        Ok(())
    }

    pub(super) fn process(
        &mut self,
        input: &gst::BufferRef,
        config: &Config,
        layouts: &[RoiLayout],
        replicate: bool,
    ) -> Result<Vec<gst::Buffer>, String> {
        let size = layouts.iter().map(|layout| layout.size).max().unwrap_or(0);

        self.ensure_capacity(size)?;

        layouts
            .iter()
            .map(|layout| {
                self.run(
                    input,
                    config,
                    std::slice::from_ref(layout),
                    layout.size,
                    replicate,
                )
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

    fn run(
        &mut self,
        input: &gst::BufferRef,
        config: &Config,
        layouts: &[RoiLayout],
        size: usize,
        replicate: bool,
    ) -> Result<gst::Buffer, String> {
        let mut output = gst::Buffer::new();

        output.get_mut().unwrap().append_memory(
            self.allocator
                .as_ref()
                .unwrap()
                .acquire_memory()
                .map_err(|e| e.to_string())?,
        );
        output.get_mut().unwrap().set_size(size);

        let intermediate = self
            .video_pool
            .acquire_buffer(None)
            .map_err(|e| e.to_string())?;

        let fence_data = self
            .fence_pool
            .acquire()
            .ok_or("Couldn't acquire fence data")?;

        let native = self.device.device_handle();
        self.device
            .prepare_graphics_cmd_list(&mut self.command_list, &self.ca_pool, None, &fence_data)
            .map_err(|e| e.to_string())?;

        let cl = self.command_list.as_ref().unwrap();

        // Keep the video memory alive without retaining the writable carrier buffer
        let mut source_buffer = gst::Buffer::new();
        let source_ref = source_buffer.get_mut().unwrap();
        for memory in input.iter_memories_owned() {
            source_ref.append_memory(memory);
        }

        if let Some(meta) = input.meta::<gst_video::VideoMeta>() {
            gst_video::VideoMeta::add_full(
                source_ref,
                meta.video_frame_flags(),
                meta.format(),
                meta.width(),
                meta.height(),
                meta.offset(),
                meta.stride(),
            )
            .map_err(|e| e.to_string())?;
        }

        self.converter
            .convert_buffer(&source_buffer, &intermediate, &fence_data, cl, true)
            .map_err(|e| e.to_string())?;

        let source = intermediate
            .peek_memory(0)
            .downcast_memory_ref::<gst_d3d12::D3D12Memory>()
            .ok_or("Not D3D12 memory")?;

        let source_map = source.map_readable_d3d12().map_err(|e| e.to_string())?;
        let src_resource = source_map.resource();
        let memory = output
            .get_mut()
            .unwrap()
            .peek_memory_mut(0)
            .map_err(|e| e.to_string())?;

        let memory = memory
            .downcast_memory_mut::<gst_d3d12::D3D12Memory>()
            .ok_or("Not D3D12 tensor memory")?;

        let output_map = memory.map_writable_d3d12().map_err(|e| e.to_string())?;
        let dst_resource = output_map.resource();
        let desc_heap = self
            .desc_pool
            .acquire()
            .ok_or("Couldn't acquire descriptor heap")?;

        let heap = desc_heap.handle();

        unsafe {
            let src_desc = src_resource.GetDesc();
            let srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: src_desc.Format,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MipLevels: 1,
                        ..Default::default()
                    },
                },
            };

            native.CreateShaderResourceView(
                src_resource,
                Some(&srv),
                heap.GetCPUDescriptorHandleForHeapStart(),
            );

            transition(
                cl,
                src_resource,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );

            cl.SetPipelineState(&self.pso);
            cl.SetComputeRootSignature(&self.root);
            cl.SetDescriptorHeaps(&[Some(heap.clone())]);
            cl.SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
            cl.SetComputeRootUnorderedAccessView(1, dst_resource.GetGPUVirtualAddress());

            let mut offset = 0;
            for (idx, layout) in layouts.iter().enumerate() {
                let (content, source_size) = layout.geometry.sampling_dimensions();
                let matrix = layout.geometry.matrix();
                let mut constants = Constants {
                    width: layout.canvas[0] as u32,
                    height: layout.canvas[1] as u32,
                    channels: config.channels as u32,
                    planar: config.planar as u32,
                    output_offset: offset,
                    tensor_float: config.tensor_float as u32,
                    norm_type: NORM_IDENTITY,
                    replicate_border: replicate as u32,
                    video_size: [self.info.width() as f32, self.info.height() as f32],
                    content_size: [content[0] as f32, content[1] as f32],
                    roi_size: [source_size[0] as f32, source_size[1] as f32],
                    numeric_scale: if config.video_float { 1.0 } else { 255.0 },
                    _padding0: 0.0,
                    texture_size: [src_desc.Width as f32, src_desc.Height as f32],
                    _padding1: [0.0; 2],
                    tensor_to_video_x: [matrix[0], matrix[1], matrix[2], 0.0],
                    tensor_to_video_y: [matrix[3], matrix[4], matrix[5], 0.0],
                    scales: [1.0; 4],
                    offsets: [0.0; 4],
                };

                match &config.normalization {
                    NormParam::Identity => (),
                    NormParam::ScaleOffset { scales, offsets } => {
                        constants.norm_type = NORM_SCALE_OFFSET;
                        for c in 0..config.channels {
                            constants.scales[c] = scales[c] as f32;
                            constants.offsets[c] = offsets[c] as f32;
                        }
                    }
                }

                cl.SetComputeRoot32BitConstants(
                    2,
                    (mem::size_of::<Constants>() / 4) as u32,
                    (&constants as *const Constants).cast(),
                    0,
                );

                cl.Dispatch(
                    (layout.canvas[0] as u32).div_ceil(64),
                    layout.canvas[1] as u32,
                    1,
                );

                if idx + 1 != layouts.len() {
                    // Adjacent uint8 samples can share a word in the raw UAV.
                    let barrier = D3D12_RESOURCE_BARRIER {
                        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                        Anonymous: D3D12_RESOURCE_BARRIER_0 {
                            UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                pResource: borrowed(dst_resource),
                            }),
                        },
                        ..Default::default()
                    };

                    cl.ResourceBarrier(std::slice::from_ref(&barrier));
                }

                offset += layout.size as u32;
            }

            cl.Close().map_err(|e| e.to_string())?;
        }
        drop(output_map);
        drop(source_map);

        let value = self
            .queue
            .execute_command_lists(&[Some(cl.cast().map_err(|e| e.to_string())?)])
            .map_err(|e| e.to_string())?;

        self.fence_value = value;

        gst_d3d12::buffer_set_fence(output.get_mut().unwrap(), Some(&self.fence), value, false);

        gst::debug!(
            CAT,
            "D3D12 tensor buffer: logical bytes {size}, capacity {}, samples {}, fence {value}",
            self.capacity,
            layouts.len()
        );

        self.queue.set_notify(value, move || {
            drop((fence_data, desc_heap, intermediate, source_buffer));
        });

        Ok(output)
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        if self.fence_value != 0 && self.queue.completed_value() < self.fence_value {
            let _ = self.queue.fence_wait(self.fence_value);
        }

        let _ = self.video_pool.set_active(false);
        if let Some(allocator) = &self.allocator {
            allocator.set_active(false);
        }
    }
}

// Use only in temporary API arguments while the original resource is alive.
// The contained interface has no owned COM reference and must never be dropped
unsafe fn borrowed(resource: &ID3D12Resource) -> ManuallyDrop<Option<ID3D12Resource>> {
    ManuallyDrop::new(Some(unsafe { ID3D12Resource::from_raw(resource.as_raw()) }))
}

unsafe fn transition(
    cl: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    unsafe {
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: borrowed(resource),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: before,
                    StateAfter: after,
                }),
            },
            ..Default::default()
        };

        cl.ResourceBarrier(std::slice::from_ref(&barrier));
    }
}

static ROOT_SIGNATURE: LazyLock<Result<ID3DBlob, String>> = LazyLock::new(|| {
    let range = D3D12_DESCRIPTOR_RANGE {
        RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
        NumDescriptors: 1,
        ..Default::default()
    };

    let parameters = [
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: 1,
                    pDescriptorRanges: &range,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Num32BitValues: (mem::size_of::<Constants>() / 4) as u32,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
    ];

    let sampler = D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_LINEAR_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxLOD: f32::MAX,
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        ..Default::default()
    };

    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: parameters.len() as u32,
        pParameters: parameters.as_ptr(),
        NumStaticSamplers: 1,
        pStaticSamplers: &sampler,
        Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
    };

    unsafe {
        let mut blob = None;
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None)
            .map_err(|e| e.to_string())?;

        Ok(blob.unwrap())
    }
});
