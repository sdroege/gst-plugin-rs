// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*};

mod converter;
mod imp;
mod model_info;
mod roi_preproc;
mod sampler;

/**
 * SECTION:element-videotensorpreproc
 * @see_also: onnxtensorinference.
 *
 * Preprocesses video frames into tensors suitable for model inference and
 * attaches the tensors to the input buffers as tensor metadata. The original
 * video frames pass through unchanged.
 *
 * The model input format, dimensions and normalization parameters are
 * configured from the modelinfo file.
 *
 * For full-frame inputs, the element converts the pixel format, resizes and
 * pads the image as needed, arranges samples in the model's tensor layout,
 * and applies per-channel scale and offset.
 *
 * ROI preprocessing extracts and resizes regions described by upstream
 * object detection metadata, including rotated regions. If there are no
 * matching detections, it processes the full frame as a single region.
 * ROI preprocessing is currently enabled automatically for CTC text
 * recognition models.
 *
 * Test image, model and labels files can be found here:
 * https://gitlab.collabora.com/gstreamer/onnx-models
 *
 * |[
 * gst-launch-1.0 filesrc location=onnx-models/images/bus.jpg \
 *     ! jpegdec \
 *     ! videotensorpreproc model-info=onnx-models/models/yolo26s.onnx.modelinfo \
 *     ! onnxtensorinference model-file=onnx-models/models/yolo26s.onnx \
 *     ! imagefreeze \
 *     ! yolo26tensordec label-file=onnx-models/labels/COCO_classes.txt \
 *     ! videoconvert ! objectdetectionoverlay ! autovideosink
 * ]| This takes a JPEG, preprocesses the video frame into an input tensor,
 * performs object detection via `onnxtensorinference`, decodes the inferred
 * tensors with `yolo26tensordec`, and overlays the detected objects on the
 * original video frame via `objectdetectionoverlay`.
 *
 * Since: plugins-rs-0.16.0
 */
glib::wrapper! {
    pub struct VideoTensorPreproc(ObjectSubclass<imp::VideoTensorPreproc>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "videotensorpreproc",
        gst::Rank::NONE,
        VideoTensorPreproc::static_type(),
    )
}
