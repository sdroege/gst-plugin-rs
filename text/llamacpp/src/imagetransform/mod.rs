// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(unused_doc_comments)]

use gst::glib;

mod imp;

/**
 * SECTION:element-llamacpp-imagetransform
 * @see_also: llamacpp-texttransform.
 *
 * [llama.cpp](https://github.com/ggml-org/llama.cpp)-based element that passes an image through a
 * vision LLM and forwards the generated text.
 *
 * It is possible to configure a system prompt, various sampling parameters and to keep a history of
 * the last inputs/outputs for producing more consistent outputs.
 *
 * The element can be used for example for image descriptions, which can then be used for text to
 * speech, or for understanding the content of a video frame by frame.
 *
 * ## Models
 *
 * The model and the multimodal projection model can be selected via the `model-path` and
 * `mmproj-path` properties. These expect local GGUF files, which can be downloaded e.g. from
 * [Hugging Face](https://huggingface.co). The `model-path` should point to the GGUF file of a
 * vision LLM and `mmproj-path` to the GGUF file of the corresponding multimodal projection model.
 * Models that are known to work well are for example
 *
 *   * [Qwen 3.5 9B](https://huggingface.co/Qwen/Qwen3.5-9B) or
 *     [Qwen 3.5 4B](https://huggingface.co/Qwen/Qwen3.5-4B).
 *
 * Keep in mind that all these models have safeguards integrated, which can lead to rejections.
 *
 * ## Examples
 *
 * ### Image description
 *
 * |[
 * gst-launch-1.0 filesrc location=photo.jpg ! image/jpeg ! llamacpp-imagetransform model-path=/path/to/Qwen3.5-9B.gguf mmproj-path=/path/to/mmproj-Qwen3.5-9B.gguf system-prompt="Describe the image in one sentence." ! fakesink
 * ]|
 *
 * Since: plugins-rs-0.16.0
 */
glib::wrapper! {
    pub struct ImageTransform(ObjectSubclass<imp::ImageTransform>) @extends crate::basetransform::LlamaBaseTransform, gst::Element, gst::Object;
}
