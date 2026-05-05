// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::parser::CubeLut;
use std::sync::LazyLock;
use windows::Win32::Graphics::{
    Direct3D::{Fxc::D3DCompile, ID3DBlob},
    Direct3D12::*,
};

const COLOR_LUT_1D_RGBA_CS: &str = r#"
Texture2D<float4> g_in : register(t0);

Texture1D<float> g_lut_r : register(t1);
Texture1D<float> g_lut_g : register(t2);
Texture1D<float> g_lut_b : register(t3);

RWTexture2D<float4> g_out : register(u0);

SamplerState g_sampler : register(s0);

cbuffer Parameters : register(b0)
{
    uint width;
    uint height;
    uint _padding0;
    uint _padding1;

    float3 domain_scale;
    float _padding2;

    float3 domain_offset;
    float _padding3;
};

[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    if (tid.x >= width || tid.y >= height)
        return;

    float4 in_val = g_in.Load(int3(tid.xy, 0));
    float3 coord = saturate(in_val.rgb * domain_scale + domain_offset);

    float3 out_val;
    out_val.r = g_lut_r.SampleLevel(g_sampler, coord.r, 0);
    out_val.g = g_lut_g.SampleLevel(g_sampler, coord.g, 0);
    out_val.b = g_lut_b.SampleLevel(g_sampler, coord.b, 0);

    g_out[tid.xy] = float4(out_val, in_val.a);
}
"#;

static COLOR_LUT_1D_RGBA_CS_BLOB: LazyLock<Result<ID3DBlob, gst::ErrorMessage>> =
    LazyLock::new(|| compile_shader(COLOR_LUT_1D_RGBA_CS, "main", "cs_5_0"));

const COLOR_LUT_3D_RGBA_CS: &str = r#"
Texture2D<float4> g_in : register(t0);

Texture3D<float4> g_lut : register(t1);

RWTexture2D<float4> g_out : register(u0);

SamplerState g_sampler : register(s0);

cbuffer Parameters : register(b0)
{
    uint width;
    uint height;
    uint _padding0;
    uint _padding1;

    float3 domain_scale;
    float _padding2;

    float3 domain_offset;
    float _padding3;
};

[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    if (tid.x >= width || tid.y >= height)
        return;

    float4 in_val = g_in.Load(int3(tid.xy, 0));
    float3 coord = saturate(in_val.rgb * domain_scale + domain_offset);
    float3 out_val = g_lut.SampleLevel(g_sampler, coord, 0).rgb;

    g_out[tid.xy] = float4(out_val, in_val.a);
}
"#;

static COLOR_LUT_3D_RGBA_CS_BLOB: LazyLock<Result<ID3DBlob, gst::ErrorMessage>> =
    LazyLock::new(|| compile_shader(COLOR_LUT_3D_RGBA_CS, "main", "cs_5_0"));

fn blob_to_string(blob: &ID3DBlob) -> Option<String> {
    unsafe {
        let ptr = blob.GetBufferPointer();
        let len = blob.GetBufferSize();

        if ptr.is_null() || len == 0 {
            return None;
        }

        let bytes = std::slice::from_raw_parts(ptr as *const u8, len);
        // Strip null-terminated C string
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());

        Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }
}

fn compile_shader(source: &str, entry: &str, target: &str) -> Result<ID3DBlob, gst::ErrorMessage> {
    unsafe {
        let mut shader_blob = None;
        let mut error_blob = None;

        let entry = std::ffi::CString::new(entry).unwrap();
        let target = std::ffi::CString::new(target).unwrap();

        D3DCompile(
            source.as_ptr() as *const _,
            source.len(),
            None,
            None,
            None,
            windows::core::PCSTR(entry.as_ptr() as *const u8),
            windows::core::PCSTR(target.as_ptr() as *const u8),
            0,
            0,
            &mut shader_blob,
            Some(&mut error_blob),
        )
        .map_err(|err| {
            let compiler_error = error_blob
                .as_ref()
                .and_then(blob_to_string)
                .unwrap_or_else(|| format!("{err:?}"));

            gst::error_msg!(
                gst::ResourceError::Failed,
                ["Failed to compile shader: {}", compiler_error]
            )
        })?;

        shader_blob.ok_or_else(|| {
            gst::error_msg!(gst::ResourceError::Failed, ["Couldn't get shader blob"])
        })
    }
}

pub struct PsoBlobs {
    pub rgba: ID3DBlob,
    // Add future formats
}

pub(crate) fn get_cs_1d() -> Result<PsoBlobs, gst::ErrorMessage> {
    Ok(PsoBlobs {
        rgba: COLOR_LUT_1D_RGBA_CS_BLOB.clone()?,
    })
}

pub(crate) fn get_cs_3d() -> Result<PsoBlobs, gst::ErrorMessage> {
    Ok(PsoBlobs {
        rgba: COLOR_LUT_3D_RGBA_CS_BLOB.clone()?,
    })
}

static ROOT_SIGNATURE_BLOB: LazyLock<Result<ID3DBlob, gst::ErrorMessage>> =
    LazyLock::new(|| unsafe {
        let srv_ranges = [
            // Input SRV, single plane
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
            // LUT texture SRV, max 3 SRVs in case of 1D LUT
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 3,
                BaseShaderRegister: 1,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
        ];

        // Output UAVs, single plane
        let uav_ranges = [D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
        }];

        // Static linear sampler for lookup operation
        let sampler = [D3D12_STATIC_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D12_COMPARISON_FUNC_ALWAYS,
            BorderColor: D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK,
            MinLOD: 0.0,
            MaxLOD: D3D12_FLOAT32_MAX,
            ShaderRegister: 0,
            RegisterSpace: 0,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        }];

        let params = [
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                        NumDescriptorRanges: 1,
                        pDescriptorRanges: &srv_ranges[0],
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                        NumDescriptorRanges: 1,
                        pDescriptorRanges: &srv_ranges[1],
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                        NumDescriptorRanges: 1,
                        pDescriptorRanges: &uav_ranges[0],
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
                        Num32BitValues: 12,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
        ];

        let desc = D3D12_ROOT_SIGNATURE_DESC {
            NumParameters: params.len() as u32,
            pParameters: params.as_ptr(),
            NumStaticSamplers: sampler.len() as u32,
            pStaticSamplers: sampler.as_ptr(),
            Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
        };

        let mut blob = None;
        let mut error_blob = None;

        D3D12SerializeRootSignature(
            &desc,
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut blob,
            Some(&mut error_blob),
        )
        .map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::Failed,
                ["Couldn't serialize root signature: {err:?}"]
            )
        })?;

        Ok(blob.unwrap())
    });

pub(crate) fn get_rs_blob() -> Result<ID3DBlob, gst::ErrorMessage> {
    ROOT_SIGNATURE_BLOB.clone()
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Constants {
    pub width: u32,
    pub height: u32,
    _padding0: u32,
    _padding1: u32,

    domain_scale: [f32; 3],
    _padding2: f32,

    domain_offset: [f32; 3],
    _padding3: f32,
}

impl Constants {
    pub fn new(info: &gst_video::VideoInfo, lut: &CubeLut) -> Self {
        Self {
            width: info.width(),
            height: info.height(),
            _padding0: 0,
            _padding1: 0,
            domain_scale: lut.domain_scale,
            _padding2: 0.0,
            domain_offset: lut.domain_offset,
            _padding3: 0.0,
        }
    }
}
