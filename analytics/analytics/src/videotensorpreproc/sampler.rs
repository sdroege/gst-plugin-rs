// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst_video::prelude::*;

#[derive(Clone, Copy, Debug)]
pub(super) struct Roi {
    pub x: f64,
    pub y: f64,
    pub width: u32,
    pub height: u32,
    pub rotation: f64,
}

pub(super) struct Geometry {
    matrix: [f32; 9],
    size: [usize; 2],
    source_size: [f64; 2],
}

impl Geometry {
    pub fn new(roi: Roi, size: [usize; 2]) -> Self {
        let (sin, cos) = roi.rotation.sin_cos();
        let w = roi.width as f64;
        let h = roi.height as f64;
        let sx = w / size[0] as f64;
        let sy = h / size[1] as f64;

        // Tensor -> source:
        // Translation(roi.x + w/2, roi.y + h/2)
        //   * Rotation(rotation)
        //   * Scale(w/tensor_w, h/tensor_h)
        //   * Translation(-tensor_w/2, -tensor_h/2)
        let matrix = [
            cos * sx,
            -sin * sy,
            roi.x + w * 0.5 - cos * w * 0.5 + sin * h * 0.5,
            sin * sx,
            cos * sy,
            roi.y + h * 0.5 - sin * w * 0.5 - cos * h * 0.5,
            0.0,
            0.0,
            1.0,
        ]
        .map(|v| v as f32);

        Self {
            matrix,
            size,
            source_size: [w, h],
        }
    }

    #[cfg(any(windows, hip_enabled))]
    pub fn sampling_dimensions(&self) -> ([usize; 2], [f64; 2]) {
        (self.size, self.source_size)
    }

    pub fn matrix(&self) -> &[f32; 9] {
        &self.matrix
    }

    // Gets the source position corresponding to the tensor (x, y) position
    pub fn point(&self, x: f64, y: f64) -> [f64; 2] {
        let m = self.matrix;
        [
            m[0] as f64 * x + m[1] as f64 * y + m[2] as f64,
            m[3] as f64 * x + m[4] as f64 * y + m[5] as f64,
        ]
    }

    pub fn sample_position(&self, x: usize, y: usize) -> [f64; 2] {
        let mut p = [x as f64 + 0.5, y as f64 + 0.5];

        // Keep sampling within the first and last source pixel centers
        for (axis, value) in p.iter_mut().enumerate() {
            let scale = self.source_size[axis] / self.size[axis] as f64;
            let source_pos = *value * scale;
            let source_pos = source_pos.clamp(0.5, self.source_size[axis] - 0.5);
            *value = source_pos / scale;
        }

        // Sampler coordinates use pixel centers at integer positions
        self.point(p[0], p[1]).map(|v| v - 0.5)
    }
}

struct Plane<'a> {
    data: &'a [u8],
    stride: usize,
}

pub(super) struct Sampler<'a> {
    planes: Vec<Plane<'a>>,
    width: usize,
    height: usize,
    channels: usize,
    float: bool,
}

impl<'a> Sampler<'a> {
    pub fn new<T>(
        frame: &'a gst_video::VideoFrameRef<T>,
        channels: usize,
        float: bool,
    ) -> Result<Self, String> {
        let planes = (0..frame.n_planes())
            .map(|p| {
                Ok(Plane {
                    data: frame.plane_data(p).map_err(|e| e.to_string())?,
                    stride: frame.plane_stride()[p as usize] as usize,
                })
            })
            .collect::<Result<_, String>>()?;

        Ok(Self {
            planes,
            width: frame.width() as usize,
            height: frame.height() as usize,
            channels,
            float,
        })
    }

    // TODO: Might need bilinear/nearest samplers if a simpler sampling method
    // is needed. Only cubic is implemented for now since it is the default in
    // video-converter, and we observed detection quality loss with bilinear sampling
    //
    // For D3D12 and HIP, we use the built-in texture samplers to reuse hardware
    // linear filtering. We can implement a custom filter in the shader if needed.
    pub fn sample_cubic(&self, position: [f64; 2], channel: usize) -> f64 {
        let x = position[0].clamp(0.0, (self.width - 1) as f64);
        let y = position[1].clamp(0.0, (self.height - 1) as f64);
        let x0 = x.floor() as isize;
        let y0 = y.floor() as isize;
        let dx = x - x0 as f64;
        let dy = y - y0 as f64;
        let (plane, channels, component) = if self.planes.len() > 1 {
            (&self.planes[channel], 1, 0)
        } else {
            (&self.planes[0], self.channels, channel)
        };

        let read = |x: isize, y: isize| {
            let x = x.clamp(0, (self.width - 1) as isize) as usize;
            let y = y.clamp(0, (self.height - 1) as isize) as usize;
            let pos =
                y * plane.stride + (x * channels + component) * if self.float { 4 } else { 1 };
            if self.float {
                f32::from_ne_bytes(plane.data[pos..pos + 4].try_into().unwrap()) as f64
            } else {
                plane.data[pos] as f64
            }
        };

        let weight = |distance: f64| {
            let t = distance.abs();
            if t <= 1.0 {
                ((1.5 * t - 2.5) * t) * t + 1.0
            } else if t < 2.0 {
                ((-0.5 * t + 2.5) * t - 4.0) * t + 2.0
            } else {
                0.0
            }
        };

        // Apply the cubic filter horizontally and vertically using a 4x4 neighborhood
        let mut value = 0.0;
        for row in -1..=2 {
            let mut horizontal = 0.0;
            for column in -1..=2 {
                horizontal += read(x0 + column, y0 + row) * weight(dx - column as f64);
            }
            value += horizontal * weight(dy - row as f64);
        }

        value
    }
}
