// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-crafttensordec
 * @see_also: ctctexttensordec
 *
 * Tensor decoder for CRAFT (Character Region Awareness for Text detection)
 * models.
 *
 * The following example detects text regions in a PNG image. Place the
 * companion `detector.onnx.modelinfo` file alongside `detector.onnx`.
 *
 * |[
 * gst-launch-1.0 filesrc location=image.png ! pngdec ! videoconvertscale ! \
 *     onnxinference model-file=detector.onnx ! \
 *     crafttensordec group-text-box=true ! \
 *     objectdetectionoverlay ! videoconvert ! imagefreeze ! autovideosink
 * ]| `videoconvertscale` converts the image to the format and size negotiated
 * by `onnxinference`. `crafttensordec` decodes the CRAFT output, groups nearby
 * text boxes and attaches object detection metadata. `objectdetectionoverlay`
 * draws the boxes, and `imagefreeze` repeats the annotated frame.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::{glib, prelude::*};

mod grouping;
mod imp;
mod postprocess;

glib::wrapper! {
    pub struct CraftTensorDec(ObjectSubclass<imp::CraftTensorDec>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "crafttensordec",
        gst::Rank::PRIMARY,
        CraftTensorDec::static_type(),
    )
}
