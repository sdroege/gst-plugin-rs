// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

static const uint NORM_IDENTITY = 0;
static const uint NORM_SCALE_OFFSET = 1;

cbuffer Parameters : register(b0) {
  uint width;
  uint height;
  uint channels;
  uint planar;

  uint output_offset;
  uint tensor_float;
  uint norm_type;
  uint replicate_border;

  float2 video_size;
  float2 content_size;

  float2 roi_size;
  float numeric_scale;
  float _padding0;

  float2 texture_size;
  float2 _padding1;

  float4 tensor_to_video_x;
  float4 tensor_to_video_y;
  float4 scales;
  float4 offsets;
};

Texture2D<float4> src : register(t0);
RWByteAddressBuffer dst : register(u0);
SamplerState linear_sampler : register(s0);

// Multiple uint8 tensor values can share one 32-bit UAV word
void
StoreUint8(uint address, float value)
{
  uint aligned_address = address & ~3u;
  uint shift = (address & 3u) * 8u;
  uint mask = 0xffu << shift;
  uint packed_value = (uint)clamp(value, 0.0, 255.0) << shift;
  uint old_value = dst.Load(aligned_address);

  while (1) {
    uint new_value = (old_value & ~mask) | (packed_value & mask);
    uint original;
    dst.InterlockedCompareExchange(
        aligned_address, old_value, new_value, original);
    if (original == old_value)
      break;
    old_value = original;
  }
}

[numthreads(64, 1, 1)]
void
main(uint3 tid : SV_DispatchThreadID)
{
  if (tid.x >= width || tid.y >= height)
    return;

  // Calculate the source video position for this tensor pixel
  float2 tensor_pos = float2(tid.xy);
  bool padding = tensor_pos.x >= content_size.x ||
      tensor_pos.y >= content_size.y;

  if (replicate_border != 0)
    tensor_pos.x = min(tensor_pos.x, content_size.x - 1);

  tensor_pos += 0.5;

  float2 roi_scale = roi_size / content_size;
  float2 scaled_pos = tensor_pos * roi_scale;
  float2 sample_pos = clamp(scaled_pos, 0.5, roi_size - 0.5) / roi_scale;

  float2 video_pos = float2(
      dot(tensor_to_video_x.xyz, float3(sample_pos, 1)),
      dot(tensor_to_video_y.xyz, float3(sample_pos, 1)));
  video_pos = clamp(video_pos, 0.5, video_size - 0.5);

  float2 texture_coord = video_pos / texture_size;
  float4 value = src.SampleLevel(linear_sampler, texture_coord, 0);

  // Apply input scaling and padding before normalization
  value *= numeric_scale;
  if (padding && replicate_border == 0)
    value = 0;

  for (uint c = 0; c < channels; c++) {
    // Normalize the channel value
    float channel_value = value[c];
    if (norm_type == NORM_SCALE_OFFSET)
      channel_value = channel_value * scales[c] + offsets[c];

    // Write the value using the requested tensor layout and data type
    uint index;
    if (planar)
      index = (c * height + tid.y) * width + tid.x;
    else
      index = (tid.y * width + tid.x) * channels + c;

    if (tensor_float)
      dst.Store(output_offset + index * 4, asuint(channel_value));
    else
      StoreUint8(output_offset + index, channel_value);
  }
}
