// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*, subclass::prelude::*};
use llama_cpp_2::llama_batch::LlamaBatch;

use std::sync::LazyLock;

use crate::basetransform::{
    LlamaBaseTransform, LlamaBaseTransformImpl,
    imp::{Message, ModelState},
};

#[derive(Default)]
pub struct TextTransform {}

#[glib::object_subclass]
impl ObjectSubclass for TextTransform {
    const NAME: &'static str = "GstLlamaCppTextTransform";
    type Type = super::TextTransform;
    type ParentType = LlamaBaseTransform;
}

impl ObjectImpl for TextTransform {}

impl GstObjectImpl for TextTransform {}

impl ElementImpl for TextTransform {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "llama.cpp Text Transform",
                "Text/LLM",
                "Transforms text based on an LLM and a system prompt",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl LlamaBaseTransformImpl for TextTransform {
    fn transform_prompt(
        &self,
        model: &mut ModelState,
        _messages: &[Message],
        prompt: &str,
    ) -> Result<String, gst::FlowError> {
        let tokens_list = model.model.vocab().tokenize(prompt.as_bytes(), true, true);

        if crate::CAT.above_threshold(gst::DebugLevel::Memdump) {
            gst::memdump!(crate::CAT, imp = self, "Prompt: {prompt}");
        }

        let mut batch = LlamaBatch::new(tokens_list.len(), 1);

        let last_index = (tokens_list.len() - 1) as i32;
        for (i, token) in (0_i32..).zip(tokens_list) {
            let is_last = i == last_index;
            if let Err(err) = batch.add(token, i, &[0], is_last) {
                gst::error!(
                    crate::CAT,
                    imp = self,
                    "Failed to add prompt token to batch: {err}"
                );
                return Err(gst::FlowError::Error);
            }
        }

        // Process prompt
        if let Err(err) = model.context.0.decode(&mut batch) {
            gst::error!(crate::CAT, imp = self, "Failed to process prompt: {err}");
            return Err(gst::FlowError::Error);
        }

        // Decode until end of generation
        let n_tokens = batch.n_tokens();
        let obj = self.obj();
        obj.upcast_ref::<LlamaBaseTransform>().imp().generate(
            model,
            &mut batch,
            n_tokens,
            n_tokens - 1,
        )
    }
}
