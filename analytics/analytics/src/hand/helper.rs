// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use byte_slice_cast::AsSliceOf;
use gst::glib;

// Re-export IoU calculation from root helpers for convenience
pub(crate) use crate::helpers::bbox_iou_f32 as bbox_iou;

pub(crate) fn extract_f32_tensor(
    buffer: &gst::BufferRef,
    tensor_id: glib::Quark,
) -> Option<(Vec<f32>, Vec<usize>)> {
    for meta in buffer.iter_meta::<gst_analytics::TensorMeta>() {
        let tensor = meta
            .typed_tensor(
                tensor_id,
                gst_analytics::TensorDataType::Float32,
                gst_analytics::TensorDimOrder::RowMajor,
                &[usize::MAX, usize::MAX, usize::MAX, usize::MAX],
            )
            .or_else(|| {
                meta.typed_tensor(
                    tensor_id,
                    gst_analytics::TensorDataType::Float32,
                    gst_analytics::TensorDimOrder::RowMajor,
                    &[usize::MAX, usize::MAX, usize::MAX],
                )
            })
            .or_else(|| {
                meta.typed_tensor(
                    tensor_id,
                    gst_analytics::TensorDataType::Float32,
                    gst_analytics::TensorDimOrder::RowMajor,
                    &[usize::MAX, usize::MAX],
                )
            })
            .or_else(|| {
                meta.typed_tensor(
                    tensor_id,
                    gst_analytics::TensorDataType::Float32,
                    gst_analytics::TensorDimOrder::RowMajor,
                    &[usize::MAX],
                )
            });

        let Some(tensor) = tensor else { continue };

        let map = tensor.data().map_readable().ok()?;
        let data = map.as_slice_of::<f32>().ok()?;
        return Some((data.to_vec(), tensor.dims().to_vec()));
    }

    None
}

/// Convert a hand bbox and hand-axis rotation into oriented object-detection
/// metadata params, applying `rotation_offset` (radians) to the rotation.
///
/// Hand-detection models express the hand-axis rotation against different angle
/// conventions, while the oriented-OD metadata uses 0 rad aligned to +X. The
/// caller supplies `rotation_offset` to reconcile its model's convention with that
/// baseline (for example, a model whose 0 rad aligns with +Y passes -PI/2).
pub(crate) fn oriented_od_params_from_bbox_and_rotation(
    bbox: (f32, f32, f32, f32),
    rotation: f32,
    rotation_offset: f32,
    video_size: Option<(i32, i32)>,
) -> Option<(i32, i32, i32, i32, f32)> {
    let (min_x, min_y, max_x, max_y) = bbox;
    if !min_x.is_finite() || !min_y.is_finite() || !max_x.is_finite() || !max_y.is_finite() {
        return None;
    }

    let (x0, y0, x1, y1) = (min_x.floor(), min_y.floor(), max_x.ceil(), max_y.ceil());

    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    if let Some((frame_width, frame_height)) = video_size
        && frame_width > 0
        && frame_height > 0
    {
        let fw = frame_width as f32;
        let fh = frame_height as f32;

        // Keep boxes that are partially outside the frame. Only drop boxes that are
        // completely outside (no overlap with the visible frame area).
        if x1 <= 0.0 || y1 <= 0.0 || x0 >= fw || y0 >= fh {
            return None;
        }
    }

    let x = x0 as i32;
    let y = y0 as i32;
    let width = (x1 - x0) as i32;
    let height = (y1 - y0) as i32;

    if width <= 0 || height <= 0 {
        return None;
    }

    // Apply the caller's rotation offset to reconcile the model's convention with
    // the oriented-OD +X baseline.
    let rotation_for_od = rotation + rotation_offset;

    Some((x, y, width, height, rotation_for_od))
}
