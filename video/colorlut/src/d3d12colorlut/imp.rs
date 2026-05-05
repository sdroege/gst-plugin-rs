// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use gst_base::subclass::prelude::*;

use gst_video::VideoFormat;

use gst_d3d12::prelude::*;

use crate::d3d12colorlut::{shader::Constants, *};
use crate::parser::*;

use std::sync::LazyLock;
use std::sync::Mutex;

use windows::{
    Win32::Graphics::{Direct3D::ID3DBlob, Direct3D12::*, Dxgi::Common::*},
    core::Interface,
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "d3d12colorlut",
        gst::DebugColorFlags::empty(),
        Some("D3D12 Color LUT"),
    )
});

struct Lut1DTexture {
    r: ID3D12Resource,
    g: ID3D12Resource,
    b: ID3D12Resource,
}

struct Lut1DUpload {
    buffer: ID3D12Resource,
    footprints: [D3D12_PLACED_SUBRESOURCE_FOOTPRINT; 3],
}

struct Lut3DUpload {
    buffer: ID3D12Resource,
    footprints: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
}

#[allow(unused)]
enum PendingUpload {
    Lut1D {
        ca: gst_d3d12::D3D12CmdAlloc,
        upload: Lut1DUpload,
    },
    Lut3D {
        ca: gst_d3d12::D3D12CmdAlloc,
        upload: Lut3DUpload,
    },
}

#[allow(unused)]
enum LutResource {
    Lut1D {
        textures: Lut1DTexture,
        srv_heap: ID3D12DescriptorHeap,
    },
    Lut3D {
        texture: ID3D12Resource,
        srv_heap: ID3D12DescriptorHeap,
    },
}

impl LutResource {
    fn num_srv(&self) -> u32 {
        match self {
            Self::Lut1D { .. } => 3,
            Self::Lut3D { .. } => 1,
        }
    }

    fn srv_heap(&self) -> &ID3D12DescriptorHeap {
        match self {
            Self::Lut1D { srv_heap, .. } => srv_heap,
            Self::Lut3D { srv_heap, .. } => srv_heap,
        }
    }
}

struct PsoSet {
    rgba: ID3D12PipelineState,
}

struct Context {
    device: gst_d3d12::D3D12Device,
    cq: gst_d3d12::D3D12CmdQueue,

    device_handle: ID3D12Device,
    fence: ID3D12Fence,
    rs: ID3D12RootSignature,
    cl: ID3D12GraphicsCommandList,
    pso: PsoSet,

    lut_resource: LutResource,

    ca_pool: gst_d3d12::D3D12CmdAllocPool,
    desc_pool: gst_d3d12::D3D12DescHeapPool,

    fence_val: u64,
}

impl Drop for Context {
    fn drop(&mut self) {
        if self.fence_val > 0 {
            let _ = self
                .device
                .fence_wait(D3D12_COMMAND_LIST_TYPE_DIRECT, self.fence_val);
        }
    }
}

impl Context {
    fn pso_for_format(&self, format: VideoFormat) -> &ID3D12PipelineState {
        match format {
            VideoFormat::Rgba64Le | VideoFormat::Rgb10a2Le | VideoFormat::Rgba => &self.pso.rgba,
            _ => unreachable!(),
        }
    }
}

#[derive(Default)]
struct Settings {
    location: Option<String>,
}

#[derive(Default)]
struct State {
    info: Option<gst_video::VideoInfo>,
    cbuf: Option<Constants>,
    lut: Option<CubeLut>,
    device: Option<gst_d3d12::D3D12Device>,
    context: Option<Context>,
    incaps: Option<gst::Caps>,
    outcaps: Option<gst::Caps>,
}

#[derive(Default)]
pub struct D3D12ColorLut {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for D3D12ColorLut {
    const NAME: &'static str = "GstD3D12ColorLut";
    type Type = super::D3D12ColorLut;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for D3D12ColorLut {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("location")
                    .nick("Location")
                    .blurb("Location of the LUT file to read from")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "location" => {
                let mut settings = self.settings.lock().unwrap();
                settings.location = value.get().unwrap();
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "location" => {
                let settings = self.settings.lock().unwrap();
                settings.location.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for D3D12ColorLut {}

impl ElementImpl for D3D12ColorLut {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "D3D12 Color LUT",
                "Filter/Video",
                "Apply Color LUT using D3D12",
                "Seungha Yang <seungha@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_video::VideoCapsBuilder::new()
                .format_list([
                    VideoFormat::Rgba64Le,
                    VideoFormat::Rgb10a2Le,
                    VideoFormat::Rgba,
                ])
                .features(["memory:D3D12Memory"])
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

fn offset_cpu_handle(
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
    offset: u32,
    increment: u32,
) -> D3D12_CPU_DESCRIPTOR_HANDLE {
    D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: handle.ptr + (offset * increment) as usize,
    }
}

fn offset_gpu_handle(
    handle: D3D12_GPU_DESCRIPTOR_HANDLE,
    offset: u32,
    increment: u32,
) -> D3D12_GPU_DESCRIPTOR_HANDLE {
    D3D12_GPU_DESCRIPTOR_HANDLE {
        ptr: handle.ptr + (offset * increment) as u64,
    }
}

fn align(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}

impl BaseTransformImpl for D3D12ColorLut {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let location = self
            .settings
            .lock()
            .unwrap()
            .location
            .clone()
            .ok_or_else(|| {
                gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["LUT file location is not configured"]
                )
            })?;

        let lut = CubeLut::parse_file(&location).map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::Read,
                ["Failed to parse LUT file {location}: {err}"]
            )
        })?;

        gst::trace!(CAT, imp = self, "Parsed LUT: {lut:?}");

        // TODO: Better to add gst_d3d12_ensure_element_data rust binding or
        // similar helper to make use of GstContext, but not strictly needed
        // since GstD3D12Device/ID3D12Device is a singleton already
        let device = gst_d3d12::D3D12Device::new(0).ok_or_else(|| {
            gst::error_msg!(
                gst::ResourceError::Failed,
                ["Could not create D3D12 device"]
            )
        })?;

        let context = self.create_context(&device, &lut)?;

        *self.state.lock().unwrap() = State {
            lut: Some(lut),
            device: Some(device),
            context: Some(context),
            ..Default::default()
        };

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = Default::default();
        Ok(())
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst::debug!(
            CAT,
            imp = self,
            "Set caps, in: {incaps:?}, out: {outcaps:?}"
        );

        let mut state = self.state.lock().unwrap();
        if state.lut.is_none() {
            return Err(gst::loggable_error!(CAT, "No LUT configured"));
        }

        if state.device.is_none() {
            return Err(gst::loggable_error!(CAT, "No Context configured"));
        }

        let info = match gst_video::VideoInfo::from_caps(incaps) {
            Err(_) => return Err(gst::loggable_error!(CAT, "Failed to parse output caps")),
            Ok(info) => info,
        };

        if state.context.is_none() {
            let context = self
                .create_context(state.device.as_ref().unwrap(), state.lut.as_ref().unwrap())
                .map_err(|err| gst::loggable_error!(CAT, "Couldn't create new context: {err}"))?;
            state.context = Some(context);
        }

        state.cbuf = Some(Constants::new(&info, state.lut.as_ref().unwrap()));
        state.info = Some(info);
        state.incaps = Some(incaps.clone());
        state.outcaps = Some(outcaps.clone());

        Ok(())
    }

    fn propose_allocation(
        &self,
        decide_query: Option<&gst::query::Allocation>,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        self.parent_propose_allocation(decide_query, query)?;

        let state = self.state.lock().unwrap();
        let device = state
            .device
            .as_ref()
            .ok_or_else(|| gst::loggable_error!(CAT, "Device not configured"))?;

        let (caps, need_pool) = query.get_owned();
        let caps = caps.ok_or_else(|| gst::loggable_error!(CAT, "No caps specified"))?;

        if need_pool {
            let info = gst_video::VideoInfo::from_caps(&caps)?;
            let pool = gst_d3d12::D3D12BufferPool::new(device);

            let mut config = pool.config();
            config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
            config.set_params(Some(&caps), info.size() as u32, 0, 0);

            pool.set_config(config)?;

            // Gets updated size
            let config = pool.config();
            let size = config
                .params()
                .map(|(_, size, _, _)| size)
                .ok_or_else(|| gst::loggable_error!(CAT, "Couldn't get updated size"))?;

            query.add_allocation_pool(Some(&pool), size, 0, 0);
        }

        query.add_allocation_meta::<gst_video::VideoMeta>(None);

        Ok(())
    }

    fn decide_allocation(
        &self,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        let state = self.state.lock().unwrap();
        let device = state
            .device
            .as_ref()
            .ok_or_else(|| gst::loggable_error!(CAT, "Device not configured"))?;

        let caps = query
            .get_owned()
            .0
            .ok_or_else(|| gst::loggable_error!(CAT, "No caps specified"))?;

        let info = gst_video::VideoInfo::from_caps(&caps)?;
        let mut update_pool = true;

        let (pool, _size, min, max) =
            query.allocation_pools().next().clone().unwrap_or_else(|| {
                update_pool = false;
                (None, info.size() as u32, 0, 0)
            });

        let pool = pool
            .and_then(|pool| pool.downcast::<gst_d3d12::D3D12BufferPool>().ok())
            .filter(|pool| pool.device().is_equal(Some(device)))
            .unwrap_or_else(|| gst_d3d12::D3D12BufferPool::new(device));

        let mut config = pool.config();
        config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
        config.set_params(Some(&caps), info.size() as u32, min, max);

        let params = if let Some(mut params) = config.d3d12_allocation_params() {
            let _ = params.set_resource_flags(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);
            params
        } else {
            gst_d3d12::D3D12AllocationParams::new(
                device,
                &info,
                gst_d3d12::D3D12AllocationFlags::D3d12AllocationFlagDefault,
                D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS
                    | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
                    | D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                D3D12_HEAP_FLAG_SHARED,
            )
            .ok_or_else(|| gst::loggable_error!(CAT, "Couldn't create allocation params"))?
        };

        config.set_d3d12_allocation_params(&params);
        pool.set_config(config)?;

        // Gets updated size
        let config = pool.config();
        let size = config
            .params()
            .map(|(_, size, _, _)| size)
            .ok_or_else(|| gst::loggable_error!(CAT, "Couldn't get updated size"))?;

        if update_pool {
            query.set_nth_allocation_pool(0, Some(&pool), size, min, max);
        } else {
            query.add_allocation_pool(Some(&pool), size, min, max);
        }

        self.parent_decide_allocation(query)
    }

    fn before_transform(&self, inbuf: &gst::BufferRef) {
        let mut state = self.state.lock().unwrap();

        let Some(device) = state.device.as_ref() else {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["No device configured"]);
            return;
        };

        if state.incaps.is_none() || state.outcaps.is_none() {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["No caps configured"]);
            return;
        }

        let Some(mem) = inbuf.memory(0) else {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["Empty buffer"]);
            return;
        };

        let Some(dmem) = mem.downcast_memory_ref::<gst_d3d12::D3D12Memory>() else {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["Wrong memory type"]);
            return;
        };

        let mem_device = dmem.device();
        if device.is_equal(Some(&mem_device)) {
            return;
        }

        gst::debug!(
            CAT,
            imp = self,
            "Device updated from {device:?} to {mem_device:?}"
        );
        state.device = Some(mem_device);
        state.context = None;
        let incaps = state.incaps.as_ref().unwrap().clone();
        let outcaps = state.outcaps.as_ref().unwrap().clone();
        drop(state);

        if let Err(err) = self.set_caps(&incaps, &outcaps) {
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["Failed to recreate D3D12 context: {err}"]
            );
            return;
        };
        self.obj().reconfigure_src();
    }

    fn transform(
        &self,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // We do support single plane RGBA now
        if inbuf.n_memory() != 1 {
            gst::error!(
                CAT,
                imp = self,
                "Not a single memory input buffer, n_memory: {}",
                inbuf.n_memory()
            );
            return Err(gst::FlowError::Error);
        }

        if outbuf.n_memory() != 1 {
            gst::error!(
                CAT,
                imp = self,
                "Not a single memory output buffer, n_memory: {}",
                outbuf.n_memory()
            );
            return Err(gst::FlowError::Error);
        }

        let mut state = self.state.lock().unwrap();
        let cbuf = state.cbuf.as_ref().unwrap();
        let context = state.context.as_ref().unwrap();

        let desc_heap: gst_d3d12::D3D12DescHeap = context.desc_pool.acquire().unwrap();
        let desc_handle = desc_heap.handle();

        let device = &context.device_handle;

        let mut in_fences = Vec::new();

        unsafe {
            let inc_size =
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            let desc_start = desc_handle.GetCPUDescriptorHandleForHeapStart();
            let mut dst_offset = 0u32;

            // Copy source SRV to ours
            for mem in inbuf.iter_memories() {
                let dmem = mem.downcast_memory_ref::<gst_d3d12::D3D12Memory>().unwrap();
                let cached_srv = dmem.shader_resource_view_heap().unwrap();
                let num_desc = dmem.plane_count();

                device.CopyDescriptorsSimple(
                    num_desc,
                    offset_cpu_handle(desc_start, dst_offset, inc_size),
                    cached_srv.GetCPUDescriptorHandleForHeapStart(),
                    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                );
                dst_offset += num_desc;

                if let Some((fence, fence_val)) = dmem.fence() {
                    in_fences.push((fence, fence_val));
                }
            }

            // Create LUT SRV, inc dst_offset
            device.CopyDescriptorsSimple(
                context.lut_resource.num_srv(),
                offset_cpu_handle(desc_start, 1, inc_size),
                context
                    .lut_resource
                    .srv_heap()
                    .GetCPUDescriptorHandleForHeapStart(),
                D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            );

            dst_offset = 4;

            // Copy destination UAV to ours
            for mem in outbuf.iter_memories_mut().unwrap() {
                let dmem = mem.downcast_memory_mut::<gst_d3d12::D3D12Memory>().unwrap();
                let cached_uav = dmem.unordered_access_view_heap().unwrap();
                let num_desc = dmem.plane_count();

                device.CopyDescriptorsSimple(
                    num_desc,
                    offset_cpu_handle(desc_start, dst_offset, inc_size),
                    cached_uav.GetCPUDescriptorHandleForHeapStart(),
                    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                );
                dst_offset += num_desc;
            }
        }

        let ca = context.ca_pool.acquire().ok_or_else(|| {
            gst::error!(CAT, imp = self, "Couldn't acquire command allocator");
            gst::FlowError::Error
        })?;

        unsafe {
            ca.handle().Reset().map_err(|err| {
                gst::error!(CAT, "Couldn't reset command allocator: {err:?}");
                gst::FlowError::Error
            })?;

            context.cl.Reset(&ca.handle(), None).map_err(|err| {
                gst::error!(CAT, "Couldn't reset command list: {err:?}");
                gst::FlowError::Error
            })?;

            let inc_size =
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);

            let gpu_start = desc_handle.GetGPUDescriptorHandleForHeapStart();

            context.cl.SetComputeRootSignature(&context.rs);
            context
                .cl
                .SetPipelineState(context.pso_for_format(state.info.as_ref().unwrap().format()));

            context.cl.SetDescriptorHeaps(&[Some(desc_handle.clone())]);

            // source SRV
            context
                .cl
                .SetComputeRootDescriptorTable(0, offset_gpu_handle(gpu_start, 0, inc_size));

            context
                .cl
                .SetComputeRootDescriptorTable(1, offset_gpu_handle(gpu_start, 1, inc_size));

            context
                .cl
                .SetComputeRootDescriptorTable(2, offset_gpu_handle(gpu_start, 4, inc_size));

            context.cl.SetComputeRoot32BitConstants(
                3,
                std::mem::size_of::<Constants>() as u32 / 4,
                cbuf as *const Constants as *const std::ffi::c_void,
                0,
            );

            let groups_x = cbuf.width.div_ceil(8);
            let groups_y = cbuf.height.div_ceil(8);

            context.cl.Dispatch(groups_x, groups_y, 1);

            context.cl.Close().map_err(|err| {
                gst::error!(CAT, "Couldn't close command list: {err:?}");
                gst::FlowError::Error
            })?;
        }

        for (f, v) in in_fences {
            context.cq.execute_wait(&f, v).unwrap();
        }

        let fence_val = context
            .cq
            .execute_command_lists(&[Some(context.cl.cast().unwrap())])
            .map_err(|err| {
                gst::error!(CAT, "Couldn't execute command list: {err:?}");
                gst::FlowError::Error
            })?;

        context.cq.set_notify(fence_val, move || {
            drop(ca);
            drop(desc_heap);
        });

        for mem in outbuf.iter_memories_mut().unwrap() {
            let dmem = mem.downcast_memory_mut::<gst_d3d12::D3D12Memory>().unwrap();
            dmem.set_fence(Some(&context.fence), fence_val, false);
        }

        state.context.as_mut().unwrap().fence_val = fence_val;

        Ok(gst::FlowSuccess::Ok)
    }
}

impl D3D12ColorLut {
    fn create_rs(&self, device: &ID3D12Device) -> Result<ID3D12RootSignature, gst::ErrorMessage> {
        let blob = shader::get_rs_blob()?;

        unsafe {
            device
                .CreateRootSignature(
                    0,
                    std::slice::from_raw_parts(
                        blob.GetBufferPointer() as *const u8,
                        blob.GetBufferSize(),
                    ),
                )
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Couldn't create root signature: {err:?}"]
                    )
                })
        }
    }

    fn create_pso(
        &self,
        device: &ID3D12Device,
        rs: &ID3D12RootSignature,
        cs_blob: &ID3DBlob,
    ) -> Result<ID3D12PipelineState, gst::ErrorMessage> {
        unsafe {
            let mut desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::ManuallyDrop::new(Some(rs.clone())),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: cs_blob.GetBufferPointer(),
                    BytecodeLength: cs_blob.GetBufferSize(),
                },
                NodeMask: 0,
                CachedPSO: D3D12_CACHED_PIPELINE_STATE::default(),
                Flags: D3D12_PIPELINE_STATE_FLAG_NONE,
            };

            let ret = device.CreateComputePipelineState(&desc);

            std::mem::ManuallyDrop::drop(&mut desc.pRootSignature);

            ret.map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Couldn't create compute PSO: {err:?}"]
                )
            })
        }
    }

    fn create_pso_set(
        &self,
        device: &ID3D12Device,
        rs: &ID3D12RootSignature,
        blobs: &shader::PsoBlobs,
    ) -> Result<PsoSet, gst::ErrorMessage> {
        Ok(PsoSet {
            rgba: self.create_pso(device, rs, &blobs.rgba)?,
        })
    }

    fn create_texture_1d(
        &self,
        device: &ID3D12Device,
        size: usize,
    ) -> Result<ID3D12Resource, gst::ErrorMessage> {
        unsafe {
            let heap_props = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
                MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
                CreationNodeMask: 0,
                VisibleNodeMask: 0,
            };

            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE1D,
                Alignment: 0,
                Width: size as u64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R32_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                Flags: D3D12_RESOURCE_FLAG_NONE,
            };

            let mut texture = None;

            device
                .CreateCommittedResource(
                    &heap_props,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut texture,
                )
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Couldn't create 1D LUT texture: {err:?}"]
                    )
                })?;

            texture.ok_or_else(|| {
                gst::error_msg!(gst::ResourceError::Failed, ["Couldn't get 1D LUT texture"])
            })
        }
    }

    fn create_lut_1d(
        &self,
        device: &ID3D12Device,
        size: usize,
    ) -> Result<(Lut1DTexture, ID3D12DescriptorHeap), gst::ErrorMessage> {
        let r = self.create_texture_1d(device, size)?;
        let g = self.create_texture_1d(device, size)?;
        let b = self.create_texture_1d(device, size)?;

        let lut_srv = unsafe {
            let increment =
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);

            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 3,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })
                .unwrap();

            let dst = heap.GetCPUDescriptorHandleForHeapStart();
            let textures = [&r, &g, &b];

            for (i, texture) in textures.iter().enumerate() {
                let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_FLOAT,
                    ViewDimension: D3D12_SRV_DIMENSION_TEXTURE1D,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture1D: D3D12_TEX1D_SRV {
                            MostDetailedMip: 0,
                            MipLevels: 1,
                            ResourceMinLODClamp: 0.0,
                        },
                    },
                };

                device.CreateShaderResourceView(
                    *texture,
                    Some(&desc),
                    offset_cpu_handle(dst, i as u32, increment),
                );
            }

            heap
        };

        Ok((Lut1DTexture { r, g, b }, lut_srv))
    }

    fn create_upload_1d(
        &self,
        device: &ID3D12Device,
        r: &[f32],
        g: &[f32],
        b: &[f32],
    ) -> Result<Lut1DUpload, gst::ErrorMessage> {
        let size = r.len();

        if size == 0 || g.len() != size || b.len() != size {
            return Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Invalid 1D LUT size"]
            ));
        }

        let tex_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE1D,
            Alignment: 0,
            Width: size as u64,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };

        let mut footprints = [D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); 3];
        let mut total_bytes = 0u64;

        unsafe {
            for fp in &mut footprints {
                total_bytes = align(total_bytes, D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT as u64);

                let mut required_size = 0u64;

                device.GetCopyableFootprints(
                    &tex_desc,
                    0,
                    1,
                    total_bytes,
                    Some(fp as *mut _),
                    None,
                    None,
                    Some(&mut required_size),
                );

                total_bytes += required_size;
            }
        }

        let buffer_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: total_bytes,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };

        let heap_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
            MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
            CreationNodeMask: 0,
            VisibleNodeMask: 0,
        };

        let buffer = unsafe {
            let mut buffer = None;

            device
                .CreateCommittedResource(
                    &heap_props,
                    D3D12_HEAP_FLAG_NONE,
                    &buffer_desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut buffer,
                )
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Couldn't create LUT upload buffer: {err:?}, {buffer_desc:?}"]
                    )
                })?;

            let buffer: ID3D12Resource = buffer.ok_or_else(|| {
                gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Couldn't get LUT upload buffer"]
                )
            })?;

            let mut mapped = std::ptr::null_mut();

            buffer.Map(0, None, Some(&mut mapped)).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Couldn't map LUT upload buffer: {err:?}"]
                )
            })?;

            for (i, src) in [r, g, b].iter().enumerate() {
                let fp = &footprints[i];
                let dst = (mapped as *mut u8).add(fp.Offset as usize) as *mut f32;

                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
            }

            buffer.Unmap(0, None);
            buffer
        };

        Ok(Lut1DUpload { buffer, footprints })
    }

    fn record_upload_1d(
        &self,
        cl: &ID3D12GraphicsCommandList,
        textures: &Lut1DTexture,
        upload: &Lut1DUpload,
    ) {
        let dst_textures = [&textures.r, &textures.g, &textures.b];

        unsafe {
            for (i, texture) in dst_textures.iter().enumerate() {
                let mut dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::ManuallyDrop::new(Some((*texture).clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: 0,
                    },
                };

                let mut src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::ManuallyDrop::new(Some(upload.buffer.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: upload.footprints[i],
                    },
                };

                cl.CopyTextureRegion(&dst, 0, 0, 0, &src, None);

                std::mem::ManuallyDrop::drop(&mut dst.pResource);
                std::mem::ManuallyDrop::drop(&mut src.pResource);
            }

            let mut barriers: Vec<D3D12_RESOURCE_BARRIER> = dst_textures
                .iter()
                .map(|texture| D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: std::mem::ManuallyDrop::new(
                            D3D12_RESOURCE_TRANSITION_BARRIER {
                                pResource: std::mem::ManuallyDrop::new(Some((*texture).clone())),
                                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                                StateBefore: D3D12_RESOURCE_STATE_COPY_DEST,
                                StateAfter: D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                            },
                        ),
                    },
                })
                .collect();

            cl.ResourceBarrier(&barriers);

            for barrier in &mut barriers {
                let transition = &mut barrier.Anonymous.Transition;
                std::mem::ManuallyDrop::drop(&mut transition.pResource);
                std::mem::ManuallyDrop::drop(transition);
            }
        }
    }

    fn create_texture_3d(
        &self,
        device: &ID3D12Device,
        size: usize,
    ) -> Result<ID3D12Resource, gst::ErrorMessage> {
        unsafe {
            let heap_props = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
                MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
                CreationNodeMask: 0,
                VisibleNodeMask: 0,
            };

            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE3D,
                Alignment: 0,
                Width: size as u64,
                Height: size as u32,
                DepthOrArraySize: size as u16,
                MipLevels: 1,
                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                Flags: D3D12_RESOURCE_FLAG_NONE,
            };

            let mut texture = None;

            device
                .CreateCommittedResource(
                    &heap_props,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut texture,
                )
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Couldn't create 3D LUT texture: {err:?}"]
                    )
                })?;

            texture.ok_or_else(|| {
                gst::error_msg!(gst::ResourceError::Failed, ["Couldn't get 3D LUT texture"])
            })
        }
    }

    fn create_lut_3d(
        &self,
        device: &ID3D12Device,
        size: usize,
    ) -> Result<(ID3D12Resource, ID3D12DescriptorHeap), gst::ErrorMessage> {
        let rgb = self.create_texture_3d(device, size)?;
        let lut_srv = unsafe {
            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })
                .unwrap();

            let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE3D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture3D: D3D12_TEX3D_SRV {
                        MostDetailedMip: 0,
                        MipLevels: 1,
                        ResourceMinLODClamp: 0.0,
                    },
                },
            };

            device.CreateShaderResourceView(
                &rgb,
                Some(&desc),
                heap.GetCPUDescriptorHandleForHeapStart(),
            );

            heap
        };

        Ok((rgb, lut_srv))
    }

    fn create_upload_3d(
        &self,
        device: &ID3D12Device,
        size: usize,
        lut: &[[f32; 4]],
    ) -> Result<Lut3DUpload, gst::ErrorMessage> {
        let expected_len = size
            .checked_mul(size)
            .and_then(|v| v.checked_mul(size))
            .ok_or_else(|| {
                gst::error_msg!(gst::ResourceError::Failed, ["Too large LUT size {size}"])
            })?;

        if lut.len() != expected_len {
            return Err(gst::error_msg!(
                gst::ResourceError::Failed,
                [
                    "Invalid 3D LUT size, expected {expected_len} entries, got {}",
                    lut.len()
                ]
            ));
        }

        let texture_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE3D,
            Alignment: 0,
            Width: size as u64,
            Height: size as u32,
            DepthOrArraySize: size as u16,
            MipLevels: 1,
            Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };

        let mut footprints = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();

        let buffer = unsafe {
            let mut total_bytes = 0;

            device.GetCopyableFootprints(
                &texture_desc,
                0,
                1,
                0,
                Some(&mut footprints),
                None,
                None,
                Some(&mut total_bytes),
            );

            let heap_props = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_UPLOAD,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
                MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
                CreationNodeMask: 0,
                VisibleNodeMask: 0,
            };

            let buffer_desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Alignment: 0,
                Width: total_bytes,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_UNKNOWN,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_NONE,
            };

            let mut buffer = None;

            device
                .CreateCommittedResource(
                    &heap_props,
                    D3D12_HEAP_FLAG_NONE,
                    &buffer_desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut buffer,
                )
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Couldn't create 3D LUT upload buffer: {err:?}"]
                    )
                })?;

            let buffer: ID3D12Resource = buffer.ok_or_else(|| {
                gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Couldn't get 3D LUT upload buffer"]
                )
            })?;

            let mut mapped = std::ptr::null_mut();

            buffer.Map(0, None, Some(&mut mapped)).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Couldn't map 3D LUT upload buffer: {err:?}"]
                )
            })?;

            let dst_base = (mapped as *mut u8).add(footprints.Offset as usize);

            let row_pitch = footprints.Footprint.RowPitch as usize;
            let pixel_size = std::mem::size_of::<[f32; 4]>();
            let row_bytes = size * pixel_size;
            let slice_pitch = row_pitch * size;

            for z in 0..size {
                for y in 0..size {
                    let src_offset = (z * size * size + y * size) * pixel_size;
                    let dst_offset = z * slice_pitch + y * row_pitch;

                    std::ptr::copy_nonoverlapping(
                        (lut.as_ptr() as *const u8).add(src_offset),
                        dst_base.add(dst_offset),
                        row_bytes,
                    );
                }
            }

            buffer.Unmap(0, None);
            buffer
        };

        Ok(Lut3DUpload { buffer, footprints })
    }

    fn record_upload_3d(
        &self,
        cl: &ID3D12GraphicsCommandList,
        texture: &ID3D12Resource,
        upload: &Lut3DUpload,
    ) {
        unsafe {
            let mut dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::ManuallyDrop::new(Some(texture.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };

            let mut src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::ManuallyDrop::new(Some(upload.buffer.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: upload.footprints,
                },
            };

            cl.CopyTextureRegion(&dst, 0, 0, 0, &src, None);

            std::mem::ManuallyDrop::drop(&mut dst.pResource);
            std::mem::ManuallyDrop::drop(&mut src.pResource);

            let mut barrier = D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                        pResource: std::mem::ManuallyDrop::new(Some(texture.clone())),
                        Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                        StateBefore: D3D12_RESOURCE_STATE_COPY_DEST,
                        StateAfter: D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                    }),
                },
            };

            cl.ResourceBarrier(&[barrier.clone()]);

            let transition = &mut barrier.Anonymous.Transition;
            std::mem::ManuallyDrop::drop(&mut transition.pResource);
            std::mem::ManuallyDrop::drop(transition);
        }
    }

    fn create_cl(
        &self,
        device: &ID3D12Device,
        ca: &gst_d3d12::D3D12CmdAlloc,
    ) -> Result<ID3D12GraphicsCommandList, gst::ErrorMessage> {
        unsafe {
            let ca_handle = ca.handle();
            ca_handle.Reset().map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Couldn't reset command allocator: {err:?}"]
                )
            })?;

            let cl: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &ca_handle, None)
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Couldn't create command list: {err:?}"]
                    )
                })?;

            Ok(cl)
        }
    }

    fn create_context(
        &self,
        device: &gst_d3d12::D3D12Device,
        lut: &CubeLut,
    ) -> Result<Context, gst::ErrorMessage> {
        let device_handle = device.device_handle();

        let rs = self.create_rs(&device_handle)?;
        let pso_blobs = match &lut.kind {
            CubeLutKind::Lut1D { .. } => shader::get_cs_1d(),
            CubeLutKind::Lut3D { .. } => shader::get_cs_3d(),
        }?;

        let pso = self.create_pso_set(&device_handle, &rs, &pso_blobs)?;

        let ca_pool =
            gst_d3d12::D3D12CmdAllocPool::new(&device_handle, D3D12_COMMAND_LIST_TYPE_DIRECT);
        let ca = ca_pool.acquire().ok_or_else(|| {
            gst::error_msg!(
                gst::ResourceError::Failed,
                ["Couldn't acquire command allocator"]
            )
        })?;
        let cl = self.create_cl(&device_handle, &ca)?;

        let (lut_resource, pending_upload) = match &lut.kind {
            CubeLutKind::Lut1D { size, r, g, b } => {
                let (textures, srv_heap) = self.create_lut_1d(&device_handle, *size)?;
                let upload = self.create_upload_1d(&device_handle, r, g, b)?;

                self.record_upload_1d(&cl, &textures, &upload);

                (
                    LutResource::Lut1D { textures, srv_heap },
                    PendingUpload::Lut1D { ca, upload },
                )
            }

            CubeLutKind::Lut3D(lut3d) => {
                let (texture, srv_heap) = self.create_lut_3d(&device_handle, lut3d.size())?;
                let upload =
                    self.create_upload_3d(&device_handle, lut3d.size(), lut3d.as_flat())?;

                self.record_upload_3d(&cl, &texture, &upload);

                (
                    LutResource::Lut3D { texture, srv_heap },
                    PendingUpload::Lut3D { ca, upload },
                )
            }
        };

        unsafe {
            cl.Close().map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Couldn't close upload command list: {err:?}"]
                )
            })?;
        }

        let cq = device.cmd_queue(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();

        let fence_val = cq
            .execute_command_lists(&[Some(cl.cast().unwrap())])
            .map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Couldn't execute upload command list: {err:?}"]
                )
            })?;

        // Schedule GC
        cq.set_notify(fence_val, move || {
            drop(pending_upload);
        });

        let desc_pool = gst_d3d12::D3D12DescHeapPool::new(
            &device_handle,
            &D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                // Up to 5,
                // 1: input SRV
                // 3: 1D lut for each channel
                // 1: output UAV
                NumDescriptors: 9,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            },
        );

        Ok(Context {
            device: device.clone(),
            cq,
            device_handle,
            fence: device.fence_handle(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap(),
            rs,
            cl,
            pso,
            lut_resource,
            ca_pool,
            desc_pool,
            fence_val,
        })
    }
}
