// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use byte_slice_cast::*;
use gst::{glib, prelude::*, subclass::prelude::*};
use gst_analytics::prelude::*;
use gst_base::{prelude::*, subclass::prelude::*};

use std::sync::{LazyLock, Mutex};

use smol_str::SmolStr;

use super::{DecodingMethod, beam_search::BeamSearch};

const CTC_TEXT_RECOGNITION_OUT: &glib::GStr = glib::gstr!("ctc-text-recognition-out");
const CTC_TEXT_RECOGNITION_OUT_PROB: &glib::GStr = glib::gstr!("ctc-text-recognition-out-prob");
const CTC_TEXT_RECOGNITION_OUT_LOGITS: &glib::GStr = glib::gstr!("ctc-text-recognition-out-logits");

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ctctexttensordec",
        gst::DebugColorFlags::empty(),
        Some("CTC text tensor decoder element"),
    )
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TensorOutput {
    Prob,
    Logits,
}

#[derive(Debug)]
struct Settings {
    dictionary_file: Option<String>,
    blank_index: u32,
    implicit_space: bool,
    decoding_method: DecodingMethod,
    beam_width: u32,
    top_k: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dictionary_file: None,
            blank_index: 0,
            implicit_space: false,
            decoding_method: DecodingMethod::BeamSearch,
            beam_width: 10,
            top_k: 0,
        }
    }
}

#[derive(Debug)]
struct State {
    tokens: Vec<SmolStr>,
    blank_index: usize,
    decoding_method: DecodingMethod,
    tensor_output: TensorOutput,
    beam_search: BeamSearch,
}

#[derive(Default)]
pub struct CtcTextTensorDec {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

#[glib::object_subclass]
impl ObjectSubclass for CtcTextTensorDec {
    const NAME: &'static str = "GstCtcTextTensorDec";

    type Type = super::CtcTextTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for CtcTextTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("dictionary-file")
                    .nick("Dictionary File")
                    .blurb("Dictionary file with one UTF-8 token per line")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("blank-index")
                    .nick("Blank Index")
                    .blurb("Index of the CTC blank token")
                    .default_value(Settings::default().blank_index)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("implicit-space")
                    .nick("Implicit Space")
                    .blurb("Append an implicit space token after the dictionary entries")
                    .default_value(Settings::default().implicit_space)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder::<DecodingMethod>("decoding-method")
                    .nick("Decoding Method")
                    .blurb("CTC decoding method")
                    .default_value(Settings::default().decoding_method)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("beam-width")
                    .nick("Beam Width")
                    .blurb("Number of prefixes to keep during beam search")
                    .minimum(1)
                    .default_value(Settings::default().beam_width)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("top-k")
                    .nick("Top K")
                    .blurb(
                        "Number of token candidates to consider at each beam search step (0 = all)",
                    )
                    .default_value(Settings::default().top_k)
                    .mutable_ready()
                    .build(),
            ]
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "dictionary-file" => {
                settings.dictionary_file = value.get().expect("type checked upstream");
            }
            "blank-index" => {
                settings.blank_index = value.get().expect("type checked upstream");
            }
            "implicit-space" => {
                settings.implicit_space = value.get().expect("type checked upstream");
            }
            "decoding-method" => {
                settings.decoding_method = value.get().expect("type checked upstream");
            }
            "beam-width" => {
                settings.beam_width = value.get().expect("type checked upstream");
            }
            "top-k" => {
                settings.top_k = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "dictionary-file" => settings.dictionary_file.to_value(),
            "blank-index" => settings.blank_index.to_value(),
            "implicit-space" => settings.implicit_space.to_value(),
            "decoding-method" => settings.decoding_method.to_value(),
            "beam-width" => settings.beam_width.to_value(),
            "top-k" => settings.top_k.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        self.obj()
            .sink_pad()
            .unset_pad_flags(gst::PadFlags::ACCEPT_INTERSECT);
    }
}

impl GstObjectImpl for CtcTextTensorDec {}

impl ElementImpl for CtcTextTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "CTC Text Tensor Decoder",
                "Tensordecoder/Video",
                "Decodes CTC text recognition tensors",
                "Seungha Yang <seungha@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_video::VideoCapsBuilder::new()
                .field(
                    "tensors",
                    gst::Structure::builder("tensorgroups")
                        .field(
                            CTC_TEXT_RECOGNITION_OUT,
                            gst::UniqueList::new([gst::Caps::builder("tensor/strided")
                                .field(
                                    "tensor-id",
                                    gst::List::new([
                                        CTC_TEXT_RECOGNITION_OUT_PROB,
                                        CTC_TEXT_RECOGNITION_OUT_LOGITS,
                                    ]),
                                )
                                .field(
                                    "dims",
                                    gst::Array::from_values([
                                        gst::IntRange::<i32>::new(0, i32::MAX).to_send_value(),
                                        gst::IntRange::<i32>::new(0, i32::MAX).to_send_value(),
                                        gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                    ]),
                                )
                                .field("dims-order", "row-major")
                                .field("type", "float32")
                                .build()]),
                        )
                        .build(),
                )
                .any_features()
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst_video::VideoCapsBuilder::new().any_features().build(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for CtcTextTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        use std::fs;

        let settings = self.settings.lock().unwrap();
        let Some(dictionary_file) = &settings.dictionary_file else {
            return Err(gst::error_msg!(
                gst::ResourceError::Settings,
                ["No dictionary file configured"]
            ));
        };

        let contents = fs::read_to_string(dictionary_file).map_err(|err| {
            gst::error!(
                CAT,
                imp = self,
                "Failed to open dictionary file '{dictionary_file}': {err}"
            );
            gst::error_msg!(
                gst::ResourceError::OpenRead,
                ["Failed to open dictionary file '{dictionary_file}': {err}"]
            )
        })?;

        let mut tokens = contents.lines().map(SmolStr::new).collect::<Vec<_>>();

        if settings.implicit_space {
            tokens.push(SmolStr::new(" "));
        }

        gst::info!(
            CAT,
            imp = self,
            "Loaded {} tokens, blank index {}",
            tokens.len(),
            settings.blank_index
        );

        *self.state.lock().unwrap() = Some(State {
            tokens,
            blank_index: settings.blank_index as usize,
            decoding_method: settings.decoding_method,
            tensor_output: TensorOutput::Prob,
            beam_search: BeamSearch::new(settings.beam_width as usize, settings.top_k as usize),
        });

        gst::info!(CAT, imp = self, "Started");

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = None;
        gst::info!(CAT, imp = self, "Stopped");
        Ok(())
    }

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp = self, "Received caps {incaps:?}");

        let mut state_guard = self.state.lock().unwrap();
        let Some(state) = state_guard.as_mut() else {
            return Err(gst::loggable_error!(CAT, "Invalid state"));
        };

        let tensors = incaps
            .structure(0)
            .and_then(|s| s.get::<gst::Structure>("tensors").ok())
            .ok_or_else(|| gst::loggable_error!(CAT, "Invalid tensors field"))?;

        let group = tensors
            .get::<gst::UniqueList>(CTC_TEXT_RECOGNITION_OUT)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid CTC tensor group"))?;

        let tensor_caps = group
            .as_slice()
            .first()
            .and_then(|v| v.get::<gst::Caps>().ok())
            .ok_or_else(|| gst::loggable_error!(CAT, "Invalid CTC tensor caps"))?;

        let tensor_structure = tensor_caps
            .structure(0)
            .ok_or_else(|| gst::loggable_error!(CAT, "Invalid CTC tensor caps"))?;

        let tensor_id = tensor_structure
            .get::<glib::GString>("tensor-id")
            .map_err(|_| gst::loggable_error!(CAT, "Invalid tensor-id"))?;

        let tensor_output = if tensor_id == CTC_TEXT_RECOGNITION_OUT_PROB {
            TensorOutput::Prob
        } else if tensor_id == CTC_TEXT_RECOGNITION_OUT_LOGITS {
            TensorOutput::Logits
        } else {
            return Err(gst::loggable_error!(
                CAT,
                "Unsupported CTC tensor-id {tensor_id}"
            ));
        };

        let dims = tensor_structure
            .get::<gst::Array>("dims")
            .map_err(|_| gst::loggable_error!(CAT, "Invalid tensor dims"))?;

        let dims = dims.as_slice();

        if dims.len() != 3 {
            return Err(gst::loggable_error!(
                CAT,
                "Expected 3 tensor dimensions but got {}",
                dims.len()
            ));
        }

        let num_classes = dims[2]
            .get::<i32>()
            .map_err(|_| gst::loggable_error!(CAT, "Invalid number of classes"))?;

        let expected_classes = state.tokens.len() + 1;
        if num_classes as usize != expected_classes {
            return Err(gst::loggable_error!(
                CAT,
                "Tensor has {num_classes} classes but expected {expected_classes}"
            ));
        }

        state.tensor_output = tensor_output;

        gst::debug!(CAT, imp = self, "Using tensor-id {tensor_id}");

        Ok(())
    }

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state_guard = self.state.lock().unwrap();
        let Some(state) = state_guard.as_mut() else {
            return Err(gst::FlowError::Flushing);
        };

        let Some(meta) = find_ctc_tensor_meta(buffer, state.tensor_output) else {
            gst::trace!(CAT, imp = self, "No CTC tensor meta found");
            return Ok(gst::FlowSuccess::Ok);
        };

        let tensor = &meta.as_slice()[0];

        let map = tensor
            .data()
            .map_readable()
            .map_err(|_| gst::FlowError::Error)?;

        let data = map
            .as_slice_of::<f32>()
            .map_err(|_| gst::FlowError::Error)?;

        let dims = tensor.dims();

        let batch_size = dims[0];
        if batch_size == 0 {
            gst::warning!(CAT, imp = self, "Empty batch");
            return Ok(gst::FlowSuccess::Ok);
        }

        let seq_len = dims[1];
        let num_classes = dims[2];

        let Some(batch_stride) = seq_len.checked_mul(num_classes) else {
            gst::error!(
                CAT,
                imp = self,
                "Invalid tensor dimensions {dims:?}: batch stride overflow"
            );
            return Err(gst::FlowError::Error);
        };

        let Some(expected_size) = batch_size.checked_mul(batch_stride) else {
            gst::error!(CAT, imp = self, "Invalid tensor dimensions {dims:?}");
            return Err(gst::FlowError::Error);
        };

        if data.len() != expected_size {
            gst::error!(
                CAT,
                imp = self,
                "Invalid tensor size {}, expected {}",
                data.len(),
                expected_size
            );
            return Err(gst::FlowError::Error);
        }

        let blank_index = state.blank_index;
        let expected_classes = state.tokens.len() + 1;

        if num_classes != expected_classes {
            gst::error!(
                CAT,
                imp = self,
                "Tensor has {num_classes} classes but expected {expected_classes}"
            );
            return Err(gst::FlowError::Error);
        }

        if blank_index >= num_classes {
            gst::error!(
                CAT,
                imp = self,
                "Blank index {blank_index} is outside {num_classes} classes"
            );
            return Err(gst::FlowError::Error);
        }

        let results = data
            .chunks_exact(batch_stride)
            .enumerate()
            .map(|(batch_idx, batch_data)| {
                let (text, confidence) = match state.decoding_method {
                    DecodingMethod::Greedy => decode_greedy(state, batch_data, num_classes),
                    DecodingMethod::BeamSearch => {
                        let logits = state.tensor_output == TensorOutput::Logits;

                        let (indices, confidence) = state.beam_search.decode(
                            batch_data,
                            num_classes,
                            state.blank_index,
                            logits,
                        );

                        let mut text = String::new();

                        for &class_index in indices {
                            append_token(&state.tokens, state.blank_index, &mut text, class_index);
                        }

                        (text, confidence)
                    }
                };

                gst::log!(
                    CAT,
                    imp = self,
                    "Decoded batch {batch_idx}: text '{text}' with confidence {confidence}"
                );

                (text, confidence as f32)
            })
            .collect::<Vec<_>>();

        drop(map);

        let mut rmeta = gst_analytics::AnalyticsRelationMeta::add(buffer);
        for (text, confidence) in results {
            rmeta
                .add_text_mtd(&text, confidence)
                .map_err(|_| gst::FlowError::Error)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

fn decode_greedy(state: &State, data: &[f32], num_classes: usize) -> (String, f64) {
    let mut previous = usize::MAX;
    let mut text = String::new();
    let mut confidence = 0f64;
    let mut num_tokens = 0usize;

    for scores in data.chunks_exact(num_classes) {
        let (class_index, &max_score) = scores
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .unwrap();

        let duplicate = class_index == previous;
        previous = class_index;

        if class_index == state.blank_index || duplicate {
            continue;
        }

        append_token(&state.tokens, state.blank_index, &mut text, class_index);

        let probability = match state.tensor_output {
            TensorOutput::Prob => max_score as f64,
            TensorOutput::Logits => {
                // Convert the max logit to softmax prob
                let max_score = max_score as f64;

                let sum = scores
                    .iter()
                    .map(|&score| ((score as f64) - max_score).exp())
                    .sum::<f64>();

                1.0 / sum
            }
        };

        confidence += probability;
        num_tokens += 1;
    }

    if num_tokens != 0 {
        confidence /= num_tokens as f64;
    }

    (text, confidence)
}

fn append_token(tokens: &[SmolStr], blank_index: usize, text: &mut String, class_index: usize) {
    // Class indices include the blank token, but the dictionary does not.
    // Re-map the class index to the corresponding dictionary entry.
    let token_index = if class_index < blank_index {
        class_index
    } else {
        class_index - 1
    };

    text.push_str(&tokens[token_index]);
}

fn find_ctc_tensor_meta(
    buffer: &gst::BufferRef,
    tensor_output: TensorOutput,
) -> Option<gst::MetaRef<'_, gst_analytics::TensorMeta>> {
    let tensor_id = match tensor_output {
        TensorOutput::Prob => CTC_TEXT_RECOGNITION_OUT_PROB,
        TensorOutput::Logits => CTC_TEXT_RECOGNITION_OUT_LOGITS,
    };

    buffer
        .iter_meta::<gst_analytics::TensorMeta>()
        .find(|meta| {
            meta.typed_tensor(
                glib::Quark::from_static_str(tensor_id),
                gst_analytics::TensorDataType::Float32,
                gst_analytics::TensorDimOrder::RowMajor,
                &[usize::MAX, usize::MAX, usize::MAX],
            )
            .is_some()
        })
}
