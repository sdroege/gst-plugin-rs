// Copyright (C) 2026 Denis Shimizu <denis.shimizu@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

pub mod imp;

/**
 * SECTION:element-zerodcetensordec
 * @see_also: onnxinference, tensor_filter.
 *
 * Tensor decoder element for Zero-DCE low-light image enhancement, and more generally for
 * any image-to-image model emitting the `image-rgb-hwc-out` tensor: denoising, deblurring,
 * super-resolution, style transfer.
 *
 * Zero-DCE was introduced in "Zero-Reference Deep Curve Estimation for Low-Light Image
 * Enhancement" by Chunle Guo, Chongyi Li, Jichang Guo, Chen Change Loy, Junhui Hou, Sam
 * Kwong and Runmin Cong (CVPR 2020, <https://arxiv.org/abs/2001.06826>), and extended in
 * "Learning to Enhance Low-Light Image via Zero-Reference Deep Curve Estimation" by
 * Chongyi Li, Chunle Guo and Chen Change Loy (IEEE TPAMI 2021,
 * <https://arxiv.org/abs/2103.00860>).
 *
 * Unlike the other tensor decoders of this plugin, which attach analytics metadata to the
 * frame they receive, this one replaces the frame content: the inferred image tensor is
 * written over the RGB_F32LE frame, or denormalized to 8 bit samples over the RGB frame.
 *
 * The tensor resolution is a property of the model instance and is not rescaled here, so the
 * frame that reaches this element must already have been scaled to it before inference.
 *
 * Model file can be found here : https://gitlab.collabora.com/gstreamer/onnx-models
 *
 * |[
 * gst-launch-1.0 v4l2src device=/dev/video1 ! videoconvertscale ! \
 *     onnxinference model-file=/work/data/models/models/zero_dce++.onnx ! \
 *     zerodcetensordec ! videoconvert ! glimagesink
 * ]| This takes a frame from a video device, performs low-light image enhancement via
 * `onnxinference` on it and decodes the inferred tensor with `zerodcetensordec` into
 * the output frame displayed by `glimagesink`.
 *
 * Since: plugins-rs-0.16.0
 */
glib::wrapper! {
    pub struct ZeroDceTensorDec(ObjectSubclass<imp::ZeroDceTensorDec>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "zerodcetensordec",
        gst::Rank::PRIMARY,
        ZeroDceTensorDec::static_type(),
    )
}
