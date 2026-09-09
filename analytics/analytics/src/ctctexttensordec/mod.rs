// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-ctctexttensordec
 * @see_also: onnxinference.
 *
 * Tensor decoder element for CTC text recognition models.
 * This element performs CTC decoding on inference output tensors and
 * attaches the decoded text to the buffer as analytics metadata.
 *
 * An example PP-OCRv5 recognition model and dictionary can be found at:
 * https://huggingface.co/monkt/paddleocr-onnx
 *
 * |[
 * gst-launch-1.0 filesrc location=/TEXT/IMAGE.png \
 *     ! pngdec ! videoconvert ! videoscale ! video/x-raw,pixel-aspect-ratio=1/1 \
 *     ! onnxinference model-file=/PATH/TO/languages/english/rec.onnx \
 *     ! ctctexttensordec dictionary-file=/PATH/TO/languages/english/dict.txt implicit-space=true \
 *     ! fakesink
 * ]| This takes a PNG, performs text recognition on it via `onnxinference`,
 * and decodes the inferred tensors with `ctctexttensordec`.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::{glib, glib::prelude::*};

mod beam_search;
mod imp;

glib::wrapper! {
    pub struct CtcTextTensorDec(ObjectSubclass<imp::CtcTextTensorDec>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "GstCtcTextTensorDecMethod")]
#[repr(C)]
pub enum DecodingMethod {
    #[default]
    #[enum_value(
        name = "BeamSearch: Decode using CTC prefix beam search",
        nick = "beam-search"
    )]
    BeamSearch,
    #[enum_value(
        name = "Greedy: Decode by selecting the most probable token at each timestep",
        nick = "greedy"
    )]
    Greedy,
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        use gst::prelude::*;
        DecodingMethod::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }

    gst::Element::register(
        Some(plugin),
        "ctctexttensordec",
        gst::Rank::PRIMARY,
        CtcTextTensorDec::static_type(),
    )
}
