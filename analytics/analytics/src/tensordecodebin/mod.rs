// Copyright (C) 2026 Collabora
//  @author: Daniel Morin <daniel.morin@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-tensordecodebin2
 * @short_description: Find and instantiate compatible tensor decoder
 *
 * This element instantiates a tensor decoder compatible with upstream caps.
 *
 * ## Example launch command:
 * |[
 * gst-launch-1.0 filesrc location=/onnx-models/images/bus.jpg \
 *   ! jpegdec ! videoconvert ! onnxinference execution-provider=cpu \
 *     model-file=/onnx-models/models/ssd_mobilenet_v1_coco.onnx \
 *   ! tensordecodebin2 ! objectdetectionoverlay ! videoconvert ! imagefreeze \
 *   ! autovideosink
 * ]| Assuming the model is an object detection model this pipeline will instantiate
 * a tensor decoder compatible with the upstream tensor caps.
 *
 * Since: plugins-rs-0.16.0
 */
use glib::prelude::*;

mod imp;

glib::wrapper! {
    pub struct TensorDecodeBin(ObjectSubclass<imp::TensorDecodeBin>) @extends gst::Bin, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "tensordecodebin2",
        gst::Rank::NONE,
        TensorDecodeBin::static_type(),
    )
}
