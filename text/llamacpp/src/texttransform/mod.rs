// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(unused_doc_comments)]

use gst::glib;
use gst::prelude::*;

mod imp;

/**
 * SECTION:element-llamacpp-texttransform
 * @see_also: textwrap, textaccumulate, whispertranscriber.
 *
 * [llama.cpp](https://github.com/ggml-org/llama.cpp)-based text transformation element that
 * passes text through a LLM and forwards the output.
 *
 * It is possible to configure a system prompt, various sampling parameters and to keep a history of
 * the last inputs/outputs for producing more consistent outputs.
 *
 * The element can be used for example for text translation.
 *
 * ## Models
 *
 * The model can be selected via the `model-path` property. This expects a local GGUF file, which
 * can be downloaded e.g. from [Hugging Face](https://huggingface.co). Models that are known to work
 * well are
 *
 *   * [Gemma 4 E4B](https://huggingface.co/google/gemma-4-E4B-it), for example
 *     [these](https://huggingface.co/unsloth/gemma-4-E4B-it-GGUF) quantizations.
 *   * [Gemma 4 E2B](https://huggingface.co/google/gemma-4-E4B-it), for example
 *     [these](https://huggingface.co/unsloth/gemma-4-E2B-it-GGUF) quantizations.
 *   * [Qwen 3.5 9B](https://huggingface.co/Qwen/Qwen3.5-9B), for example
 *     [these](https://huggingface.co/unsloth/Qwen3.5-9B-GGUF) quantizations.
 *   * [Qwen 3.5 4B](https://huggingface.co/Qwen/Qwen3.5-4B), for example
 *     [these](https://huggingface.co/unsloth/Qwen3.5-4B-GGUF) quantizations.
 *   * [Hunyuan MT 7B](https://huggingface.co/tencent/Hunyuan-MT-7B), for example
 *     [these](https://huggingface.co/mradermacher/Hunyuan-MT-7B-GGUF) quantizations.
 *
 * It generally makes no sense to use huge models for this element, and even smaller ones than the
 * ones above will give useful results.
 *
 * Keep in mind that all these models have safeguards integrated, which can lead to rejections.
 * For subtitle translations of Rated R movies, for example, it might be necessary to use an
 * abliterated / decensored model like [this](https://huggingface.co/llmfan46/Qwen3.5-9B-ultra-uncensored-heretic-v2-GGUF).
 *
 * ## Examples
 *
 * ### Subtitle translation
 *
 * |[
 * gst-launch-1.0 filesrc location=subtitles.eng.srt ! subparse ! llamacpp-texttransform model-path=/path/to/Hunyuan-MT-7B.Q4_K_M.gguf system-prompt="Translate the following segments into German, without additional explanation." history-size=5 ! overlay.text_sink \
 *     filesrc location=movie.mp4 ! decodebin3 name=dbin \
 *     dbin. ! queue ! videoconvert ! textoverlay name=overlay ! videoconvert ! navseek ! autovideosink \
 *     dbin. ! queue ! audioconvert ! autoaudiosink
 * |] Plays a movie with English subtitles and translates them to German before overlaying.
 *
 * ### Audio transcription and translation
 *
 * |[
 * gst-launch-1.0 filesrc location=movie.mp4 ! decodebin3 name=dbin \
 *     dbin. ! audio/x-raw ! tee name=audio-tee
 *     audio-tee. ! queue max-size-time=10000000000 max-size-buffers=0 max-size-bytes=0 ! audioconvert ! audioresample ! \
 *         whispertranscriber model-path=whisper-ggml-large-v3.bin model-preset=large-v3 chunk-duration=4000 ! \
 *         textaccumulate latency=0 ! queue max-size-time=10000000000 max-size-buffers=0 max-size-bytes=0 ! \
 *         llamacpp-texttransform model-path=Hunyuan-MT-7B.Q4_K_M.gguf system-prompt="Translate the following segments into German, without additional explanation." history-size=5 ! \
 *         textwrap columns=72 ! overlay.text_sink \
 *     dbin. ! queue max-size-time=10000000000 max-size-buffers=0 max-size-bytes=0 ! videoconvert ! \
 *         textoverlay name=overlay ! videoconvert ! autovideosink
 *     audio-tee. ! queue max-size-time=10000000000 max-size-buffers=0 max-size-bytes=0 ! audioconvert ! autoaudiosink
 * |] Plays a movie, transcribes the audio to text, translates the text to German and overlays it on top of the video.
 *
 * Since: plugins-rs-0.16.0
 */
glib::wrapper! {
    pub struct TextTransform(ObjectSubclass<imp::TextTransform>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "llamacpp-texttransform",
        gst::Rank::NONE,
        TextTransform::static_type(),
    )
}
