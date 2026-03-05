// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Shared utility functions for tensor processing across analytics elements.
//!
//! This module is gated on `v1_28` (see `lib.rs`) because it relies on
//! `gst_analytics::image_util`, which is only available from that version.

/// Calculate Intersection over Union (IoU) for axis-aligned bounding boxes with f32 coordinates.
///
/// Takes two bounding boxes as tuples (min_x, min_y, max_x, max_y) and returns the IoU value
/// as an f32 in the range [0.0, 1.0].
///
/// Only consumed by the `v1_30`-gated `hand` decoders, so it is unused in a
/// `v1_28`-only build.
#[allow(dead_code)]
pub(crate) fn bbox_iou_f32(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    gst_analytics::image_util::iou_f32(
        gst_analytics::image_util::Rect::<f32> {
            x: a.0,
            y: a.1,
            w: a.2 - a.0,
            h: a.3 - a.1,
        },
        gst_analytics::image_util::Rect::<f32> {
            x: b.0,
            y: b.1,
            w: b.2 - b.0,
            h: b.3 - b.1,
        },
    )
}

/// Calculate Intersection over Union (IoU) for axis-aligned bounding boxes with i32 coordinates.
///
/// Takes two bounding boxes as tuples (min_x, min_y, max_x, max_y) and returns the IoU value
/// as an f32 in the range [0.0, 1.0].
///
/// This is provided for other tensor decoders that work with integer-based bounding boxes.
#[allow(dead_code)]
pub(crate) fn bbox_iou_i32(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> f32 {
    gst_analytics::image_util::iou_i32(
        gst_analytics::image_util::Rect::<i32> {
            x: a.0,
            y: a.1,
            w: a.2 - a.0,
            h: a.3 - a.1,
        },
        gst_analytics::image_util::Rect::<i32> {
            x: b.0,
            y: b.1,
            w: b.2 - b.0,
            h: b.3 - b.1,
        },
    )
}
