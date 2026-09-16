// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

struct Parameters {
  unsigned int width;
  unsigned int height;
  unsigned int channels;
  unsigned int planar;

  unsigned int output_offset;
  unsigned int tensor_float;
  float numeric_scale;
  unsigned int replicate;

  unsigned int video_width;
  unsigned int video_height;

  float content_width;
  float content_height;
  float roi_width;
  float roi_height;

  float matrix[6];
  float scales[3];
  float offsets[3];
};

extern "C" __global__ void
preprocess(TextureObject_t src, unsigned char *dst, Parameters p)
{
  unsigned int x = blockIdx.x * blockDim.x + threadIdx.x;
  unsigned int y = blockIdx.y * blockDim.y + threadIdx.y;
  if (x >= p.width || y >= p.height)
    return;

  // Calculate the source video position for this tensor pixel
  bool padding = x >= p.content_width || y >= p.content_height;
  float tensor_x = (float)x;
  if (p.replicate)
    tensor_x = fminf(tensor_x, p.content_width - 1);

  float roi_scale_x = p.roi_width / p.content_width;
  float roi_scale_y = p.roi_height / p.content_height;
  float sample_x = fminf(fmaxf((tensor_x + 0.5f) * roi_scale_x, 0.5f),
      p.roi_width - 0.5f) / roi_scale_x;
  float sample_y = fminf(fmaxf((y + 0.5f) * roi_scale_y, 0.5f),
      p.roi_height - 0.5f) / roi_scale_y;

  float video_x = p.matrix[0] * sample_x + p.matrix[1] * sample_y + p.matrix[2];
  float video_y = p.matrix[3] * sample_x + p.matrix[4] * sample_y + p.matrix[5];
  video_x = fminf(fmaxf(video_x, 0.5f), (float)p.video_width - 0.5f);
  video_y = fminf(fmaxf(video_y, 0.5f), (float)p.video_height - 0.5f);

  float texture_x = video_x / p.video_width;
  float texture_y = video_y / p.video_height;
  float values[3];
  if (p.channels == 1) {
    values[0] = tex2D<float>(src, texture_x, texture_y);
  } else {
    float4 sampled = tex2D<float4>(src, texture_x, texture_y);
    values[0] = sampled.x;
    values[1] = sampled.y;
    values[2] = sampled.z;
  }

  for (unsigned int channel = 0; channel < p.channels; channel++) {
    // Apply input scaling and padding before normalization
    float channel_value = values[channel] * p.numeric_scale;
    if (padding && !p.replicate)
      channel_value = 0;

    // Normalize the channel value
    channel_value = channel_value * p.scales[channel] + p.offsets[channel];

    // Write the value using the requested tensor layout and data type
    unsigned int index;
    if (p.planar)
      index = (channel * p.height + y) * p.width + x;
    else
      index = (y * p.width + x) * p.channels + channel;

    if (p.tensor_float) {
      ((float *)(dst + p.output_offset))[index] = channel_value;
    } else {
      dst[p.output_offset + index] =
          (unsigned char) fminf(fmaxf(channel_value, 0.0f), 255.0f);
    }
  }
}

struct PackParameters {
  unsigned int width;
  unsigned int height;
  unsigned int channels;
  unsigned int planar;
  unsigned int tensor_float;
  unsigned int video_float;
  unsigned int content_width;
  unsigned int content_height;
  unsigned int strides[3];
  float scales[3];
  float offsets[3];
};

extern "C" __global__ void
pack_tensor(const unsigned char *src0, const unsigned char *src1,
    const unsigned char *src2, unsigned char *dst, PackParameters p)
{
  unsigned int x = blockIdx.x * blockDim.x + threadIdx.x;
  unsigned int y = blockIdx.y * blockDim.y + threadIdx.y;
  if (x >= p.width || y >= p.height)
    return;

  const unsigned char *planes[3] = { src0, src1, src2 };

  for (unsigned int channel = 0; channel < p.channels; channel++) {
    unsigned int plane;
    unsigned int component;
    unsigned int index;

    if (p.planar) {
      plane = channel;
      component = x;
      index = (channel * p.height + y) * p.width + x;
    } else {
      plane = 0;
      component = x * p.channels + channel;
      index = (y * p.width + x) * p.channels + channel;
    }

    float value = 0;
    if (x < p.content_width && y < p.content_height) {
      const unsigned char *row = planes[plane] + y * p.strides[plane];
      if (p.video_float)
        value = ((const float *) row)[component];
      else
        value = row[component];
    }

    value = value * p.scales[channel] + p.offsets[channel];

    if (p.tensor_float)
      ((float *) dst)[index] = value;
    else
      dst[index] = (unsigned char) fminf(fmaxf(value, 0.0f), 255.0f);
  }
}
