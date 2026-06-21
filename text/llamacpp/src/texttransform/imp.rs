// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*, subclass::prelude::*};
use llama_cpp_2::{
    context::{LlamaContext, params::LlamaContextParams},
    llama_batch::LlamaBatch,
    model::{LlamaModel, params::LlamaModelParams},
    sampling::LlamaSampler,
};

use std::{
    num::NonZeroU32,
    str::FromStr,
    sync::{LazyLock, Mutex},
};

use crate::BACKEND;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "llamacpp-texttransform",
        gst::DebugColorFlags::empty(),
        Some("llama.cpp text transform Element"),
    )
});

pub struct TextTransform {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

struct ModelState {
    model: LlamaModel,
    sampler: LlamaSamplerWrapper,
    context: LlamaContextWrapper,
    tmpl: minijinja::Environment<'static>,
}

// FIXME: Not actually 'static but self-referential to model
struct LlamaContextWrapper(LlamaContext<'static>);
unsafe impl Send for LlamaContextWrapper {}
struct LlamaSamplerWrapper(LlamaSampler);
unsafe impl Send for LlamaSamplerWrapper {}

#[derive(Default)]
struct State {
    model: Option<ModelState>,
    messages: Vec<Message>,
}

// Default values of properties
const DEFAULT_MODEL_PATH: Option<&str> = None;
const DEFAULT_HISTORY_SIZE: u32 = 5;
const DEFAULT_CONTEXT_SIZE: u32 = 2048;
const DEFAULT_SYSTEM_PROMPT: Option<&str> = None;
const DEFAULT_TEMP: f32 = 0.8;
const DEFAULT_SEED: u32 = 0xbadc_0ffe;
const DEFAULT_MIN_P: f32 = 0.05;
const DEFAULT_TOP_P: f32 = 0.95;
const DEFAULT_TOP_K: i32 = 40;
const DEFAULT_PENALTY_LAST_N: i32 = 64;
const DEFAULT_PENALTY_REPEAT: f32 = 1.0;
const DEFAULT_PENALTY_FREQ: f32 = 0.0;
const DEFAULT_PENALTY_PRESENT: f32 = 0.0;

struct Settings {
    model_path: Option<String>,
    history_size: u32,
    context_size: u32,
    system_prompt: Option<String>,
    temp: f32,
    seed: u32,
    min_p: f32,
    top_k: i32,
    top_p: f32,
    penalty_last_n: i32,
    penalty_repeat: f32,
    penalty_freq: f32,
    penalty_present: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            model_path: DEFAULT_MODEL_PATH.map(str::to_string),
            history_size: DEFAULT_HISTORY_SIZE,
            context_size: DEFAULT_CONTEXT_SIZE,
            system_prompt: DEFAULT_SYSTEM_PROMPT.map(str::to_string),
            temp: DEFAULT_TEMP,
            seed: DEFAULT_SEED,
            min_p: DEFAULT_MIN_P,
            top_p: DEFAULT_TOP_P,
            top_k: DEFAULT_TOP_K,
            penalty_last_n: DEFAULT_PENALTY_LAST_N,
            penalty_repeat: DEFAULT_PENALTY_REPEAT,
            penalty_freq: DEFAULT_PENALTY_FREQ,
            penalty_present: DEFAULT_PENALTY_PRESENT,
        }
    }
}

impl TextTransform {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buf: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, obj = pad, "Handling buffer {buf:?}");

        let Ok(map) = buf.map_readable() else {
            gst::error!(CAT, imp = self, "Failed to map buffer");
            return Err(gst::FlowError::Error);
        };

        let input = match str::from_utf8(&map) {
            Ok(s) => s,
            Err(err) => {
                gst::error!(CAT, imp = self, "Failed to parse buffer as UTF-8: {err}");
                return Err(gst::FlowError::Error);
            }
        };

        let output = self.transform_text(input)?;
        drop(map);

        let mut outbuf = gst::Buffer::from_mut_slice(output.into_bytes());
        {
            let outbuf = outbuf.get_mut().unwrap();
            if buf
                .copy_into(outbuf, gst::BUFFER_COPY_METADATA, ..)
                .is_err()
            {
                gst::warning!(CAT, imp = self, "Failed to copy metadata to output");
            }
        }

        self.srcpad.push(outbuf)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling event {event:?}");

        match event.view() {
            gst::EventView::FlushStop(_) | gst::EventView::StreamStart(_) => {
                let mut state = self.state.lock().unwrap();
                state.messages.clear();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn src_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling event {event:?}");

        match event.view() {
            gst::EventView::FlushStop(_) => {
                let mut state = self.state.lock().unwrap();
                state.messages.clear();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for TextTransform {
    const NAME: &'static str = "GstLlamaCppTextTransform";
    type Type = super::TextTransform;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                TextTransform::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |identity| identity.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                TextTransform::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.sink_event(pad, event),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .event_function(|pad, parent, event| {
                TextTransform::catch_panic_pad_function(
                    parent,
                    || false,
                    |identity| identity.src_event(pad, event),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::default(),
            settings: Mutex::default(),
        }
    }
}

impl ObjectImpl for TextTransform {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("model-path")
                    .nick("Model Path")
                    .blurb("Path to the GGUF model file")
                    .default_value(DEFAULT_MODEL_PATH)
                    .build(),
                glib::ParamSpecUInt::builder("history-size")
                    .nick("History Size")
                    .blurb("Number of previous messages to keep in context")
                    .default_value(DEFAULT_HISTORY_SIZE)
                    .maximum(100)
                    .build(),
                glib::ParamSpecUInt::builder("context-size")
                    .nick("Context Size")
                    .blurb("Size of the context window for the LLM")
                    .default_value(DEFAULT_CONTEXT_SIZE)
                    .minimum(512)
                    .build(),
                glib::ParamSpecString::builder("system-prompt")
                    .nick("System Prompt")
                    .blurb("System prompt for the LLM")
                    .default_value(DEFAULT_SYSTEM_PROMPT)
                    .build(),
                glib::ParamSpecFloat::builder("temp")
                    .nick("Temperature")
                    .blurb("Sampling temperature")
                    .default_value(DEFAULT_TEMP)
                    .minimum(0.0)
                    .build(),
                glib::ParamSpecUInt::builder("seed")
                    .nick("Seed")
                    .blurb("Random seed for sampling")
                    .default_value(DEFAULT_SEED)
                    .build(),
                glib::ParamSpecFloat::builder("min-p")
                    .nick("Min P")
                    .blurb("Minimum probability threshold (0.0 = disabled)")
                    .default_value(DEFAULT_MIN_P)
                    .minimum(0.0)
                    .maximum(1.0)
                    .build(),
                glib::ParamSpecInt::builder("top-k")
                    .nick("Top K")
                    .blurb("Top-k sampling parameter (<= 0 to use vocab size)")
                    .default_value(DEFAULT_TOP_K)
                    .build(),
                glib::ParamSpecFloat::builder("top-p")
                    .nick("Top P")
                    .blurb("Top-p sampling parameter (1.0 = disabled)")
                    .default_value(DEFAULT_TOP_P)
                    .minimum(0.0)
                    .maximum(1.0)
                    .build(),
                glib::ParamSpecInt::builder("penalty-last-n")
                    .nick("Penalty Last N")
                    .blurb("Last n tokens to penalize (0 = disable, -1 = context size)")
                    .default_value(DEFAULT_PENALTY_LAST_N)
                    .minimum(-1)
                    .build(),
                glib::ParamSpecFloat::builder("penalty-repeat")
                    .nick("Penalty Repeat")
                    .blurb("Repetition penalty (1.0 = disabled)")
                    .default_value(DEFAULT_PENALTY_REPEAT)
                    .minimum(0.0)
                    .build(),
                glib::ParamSpecFloat::builder("penalty-freq")
                    .nick("Penalty Frequency")
                    .blurb("Frequency penalty (0.0 = disabled)")
                    .default_value(DEFAULT_PENALTY_FREQ)
                    .minimum(0.0)
                    .build(),
                glib::ParamSpecFloat::builder("penalty-present")
                    .nick("Penalty Presence")
                    .blurb("Presence penalty (0.0 = disabled)")
                    .default_value(DEFAULT_PENALTY_PRESENT)
                    .minimum(0.0)
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "model-path" => {
                let mut settings = self.settings.lock().unwrap();
                let model_path = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing model-path from {:?} to {:?}",
                    settings.model_path,
                    model_path
                );
                settings.model_path = model_path;
            }
            "history-size" => {
                let mut settings = self.settings.lock().unwrap();
                let history_size = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing history-size from {} to {}",
                    settings.history_size,
                    history_size
                );
                settings.history_size = history_size;
            }
            "context-size" => {
                let mut settings = self.settings.lock().unwrap();
                let context_size = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing context-size from {} to {}",
                    settings.context_size,
                    context_size
                );
                settings.context_size = context_size;
            }
            "system-prompt" => {
                let mut settings = self.settings.lock().unwrap();
                let system_prompt = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing system-prompt from {:?} to {:?}",
                    settings.system_prompt,
                    system_prompt
                );
                settings.system_prompt = system_prompt;
            }
            "temp" => {
                let mut settings = self.settings.lock().unwrap();
                let temp = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing temp from {} to {}",
                    settings.temp,
                    temp
                );
                settings.temp = temp;
            }
            "seed" => {
                let mut settings = self.settings.lock().unwrap();
                let seed = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing seed from {} to {}",
                    settings.seed,
                    seed
                );
                settings.seed = seed;
            }
            "min-p" => {
                let mut settings = self.settings.lock().unwrap();
                let min_p = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing min-p from {} to {}",
                    settings.min_p,
                    min_p
                );
                settings.min_p = min_p;
            }
            "top-k" => {
                let mut settings = self.settings.lock().unwrap();
                let top_k = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing top-k from {} to {}",
                    settings.top_k,
                    top_k
                );
                settings.top_k = top_k;
            }
            "top-p" => {
                let mut settings = self.settings.lock().unwrap();
                let top_p = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing top-p from {} to {}",
                    settings.top_p,
                    top_p
                );
                settings.top_p = top_p;
            }
            "penalty-last-n" => {
                let mut settings = self.settings.lock().unwrap();
                let penalty_last_n = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing penalty-last-n from {} to {}",
                    settings.penalty_last_n,
                    penalty_last_n
                );
                settings.penalty_last_n = penalty_last_n;
            }
            "penalty-repeat" => {
                let mut settings = self.settings.lock().unwrap();
                let penalty_repeat = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing penalty-repeat from {} to {}",
                    settings.penalty_repeat,
                    penalty_repeat
                );
                settings.penalty_repeat = penalty_repeat;
            }
            "penalty-freq" => {
                let mut settings = self.settings.lock().unwrap();
                let penalty_freq = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing penalty-freq from {} to {}",
                    settings.penalty_freq,
                    penalty_freq
                );
                settings.penalty_freq = penalty_freq;
            }
            "penalty-present" => {
                let mut settings = self.settings.lock().unwrap();
                let penalty_present = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing penalty-present from {} to {}",
                    settings.penalty_present,
                    penalty_present
                );
                settings.penalty_present = penalty_present;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "model-path" => {
                let settings = self.settings.lock().unwrap();
                settings.model_path.as_deref().to_value()
            }
            "history-size" => {
                let settings = self.settings.lock().unwrap();
                settings.history_size.to_value()
            }
            "context-size" => {
                let settings = self.settings.lock().unwrap();
                settings.context_size.to_value()
            }
            "system-prompt" => {
                let settings = self.settings.lock().unwrap();
                settings.system_prompt.as_deref().to_value()
            }
            "temp" => {
                let settings = self.settings.lock().unwrap();
                settings.temp.to_value()
            }
            "seed" => {
                let settings = self.settings.lock().unwrap();
                settings.seed.to_value()
            }
            "min-p" => {
                let settings = self.settings.lock().unwrap();
                settings.min_p.to_value()
            }
            "top-k" => {
                let settings = self.settings.lock().unwrap();
                settings.top_k.to_value()
            }
            "top-p" => {
                let settings = self.settings.lock().unwrap();
                settings.top_p.to_value()
            }
            "penalty-last-n" => {
                let settings = self.settings.lock().unwrap();
                settings.penalty_last_n.to_value()
            }
            "penalty-repeat" => {
                let settings = self.settings.lock().unwrap();
                settings.penalty_repeat.to_value()
            }
            "penalty-freq" => {
                let settings = self.settings.lock().unwrap();
                settings.penalty_freq.to_value()
            }
            "penalty-present" => {
                let settings = self.settings.lock().unwrap();
                settings.penalty_present.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

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
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        let res = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::NullToReady => {
                if !self.create_model() {
                    return Err(gst::StateChangeError);
                }
            }
            gst::StateChange::ReadyToNull => {
                *self.state.lock().unwrap() = State::default();
            }
            _ => {}
        }

        Ok(res)
    }
}

impl TextTransform {
    fn create_model(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        let Some(model_path) = settings.model_path.as_ref() else {
            gst::error!(CAT, imp = self, "Have no model path set");
            return false;
        };

        gst::debug!(CAT, imp = self, "Loading model {model_path}");

        let backend = match &*BACKEND {
            Ok(backend) => backend,
            Err(err) => {
                gst::error!(CAT, imp = self, "Failed to initialize llama.cpp: {err}");
                return false;
            }
        };

        let mut model_params = LlamaModelParams::default();

        let mut ctx_params = LlamaContextParams::default()
            .with_n_ctx(Some(NonZeroU32::new(settings.context_size).unwrap()))
            .with_n_threads(4);
        // Keep 128MB of memory free at least
        let mut margins = vec![128_000_000, llama_cpp_2::max_devices()];
        if let Err(err) = std::pin::Pin::new(&mut model_params).fit_params(
            &std::ffi::CString::from_str(model_path).unwrap(),
            &mut ctx_params,
            &mut margins,
            settings.context_size,
            0,
        ) {
            gst::error!(CAT, imp = self, "Failed to fit model: {err}");
            return false;
        }

        let model = match LlamaModel::load_from_file(backend, model_path, &model_params) {
            Ok(model) => model,
            Err(err) => {
                gst::error!(CAT, imp = self, "Failed to load model: {err}");
                return false;
            }
        };

        let mut chain = vec![];
        chain.push(LlamaSampler::temp(settings.temp));
        if settings.min_p > 0.0 {
            chain.push(LlamaSampler::min_p(settings.min_p, 1));
        }
        if settings.top_k > 0 {
            chain.push(LlamaSampler::top_k(settings.top_k));
        }
        if settings.top_p < 1.0 {
            chain.push(LlamaSampler::top_p(settings.top_p, 1));
        }
        if settings.penalty_last_n > 0
            || settings.penalty_repeat != 1.0
            || settings.penalty_freq > 0.0
            || settings.penalty_present > 0.0
        {
            chain.push(LlamaSampler::penalties(
                settings.penalty_last_n,
                settings.penalty_repeat,
                settings.penalty_freq,
                settings.penalty_present,
            ));
        }
        chain.push(LlamaSampler::dist(settings.seed));

        let sampler = LlamaSampler::chain_simple(chain);

        // FIXME: Get rid of non-'static lifetime to be able to store context
        // and model together
        let context = unsafe {
            let context = match model.new_context(backend, ctx_params) {
                Ok(context) => context,
                Err(err) => {
                    gst::error!(CAT, imp = self, "Failed to create model context: {err}");
                    return false;
                }
            };
            std::mem::transmute::<
                llama_cpp_2::context::LlamaContext<'_>,
                llama_cpp_2::context::LlamaContext<'_>,
            >(context)
        };

        let tmpl = match model.chat_template(None) {
            Ok(tmpl) => tmpl,
            Err(err) => {
                gst::error!(CAT, imp = self, "Failed to get model chat template: {err}");
                return false;
            }
        };
        let tmpl = match create_jinja_environment(tmpl.to_str().unwrap()) {
            Ok(tmpl) => tmpl,
            Err(err) => {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to parse model chat template: {err}"
                );
                return false;
            }
        };

        let model_state = ModelState {
            model,
            sampler: LlamaSamplerWrapper(sampler),
            context: LlamaContextWrapper(context),
            tmpl,
        };

        state.model = Some(model_state);

        true
    }

    fn transform_text(&self, input: &str) -> Result<String, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let State {
            model: Some(model),
            messages,
        } = &mut *state
        else {
            gst::error!(CAT, imp = self, "Model not loaded");
            return Err(gst::FlowError::Error);
        };

        let settings = self.settings.lock().unwrap();
        if messages.is_empty() {
            if let Some(ref system_prompt) = settings.system_prompt {
                messages.push(Message {
                    role: Role::System,
                    content: system_prompt.to_string(),
                });
            } else {
                messages.push(Message {
                    role: Role::System,
                    content: String::new(),
                });
            }
        }

        // TODO: make use of llama_memory_seq_rm() etc to keep as much of the
        // KV cache as possible. Needs https://github.com/utilityai/llama-cpp-rs/pull/1050
        // See completion example around
        // https://github.com/ggml-org/llama.cpp/blob/bddfd2b1137cd6e51fbb939081caf50e9f496a66/tools/completion/completion.cpp#L604
        model.context.0.clear_kv_cache();

        while messages.len() > 1 + 2 * settings.history_size as usize {
            // Remove User message
            messages.remove(1);
            // Remove Assistant message
            messages.remove(1);
        }
        drop(settings);

        messages.push(Message {
            role: Role::User,
            content: input.to_string(),
        });

        let prompt = match apply_jinja_template(&model.tmpl, messages) {
            Ok(prompt) => prompt,
            Err(err) => {
                gst::error!(
                    CAT,
                    imp = self,
                    "Can't apply chat template to messages: {err}"
                );
                return Err(gst::FlowError::Error);
            }
        };

        let tokens_list = match model
            .model
            .str_to_token(&prompt, llama_cpp_2::model::AddBos::Always)
        {
            Ok(tokens_list) => tokens_list,
            Err(err) => {
                gst::error!(CAT, imp = self, "Can't convert prompt to tokens: {err}");
                return Err(gst::FlowError::Error);
            }
        };

        if CAT.above_threshold(gst::DebugLevel::Memdump) {
            let mut decoder = encoding_rs::UTF_8.new_decoder();

            let mut prompt_str = String::new();
            for token in &tokens_list {
                prompt_str.push_str(
                    &model
                        .model
                        .token_to_piece(*token, &mut decoder, true, None)
                        .unwrap_or("�".to_string()),
                );
            }

            gst::memdump!(CAT, imp = self, "Prompt: {prompt_str}");
        }

        let mut batch = LlamaBatch::new(512, 1);

        let last_index = (tokens_list.len() - 1) as i32;
        for (i, token) in (0_i32..).zip(tokens_list) {
            let is_last = i == last_index;
            if let Err(err) = batch.add(token, i, &[0], is_last) {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to add prompt token to batch: {err}"
                );
                return Err(gst::FlowError::Error);
            }
        }

        // Process prompt
        if let Err(err) = model.context.0.decode(&mut batch) {
            gst::error!(CAT, imp = self, "Failed to process prompt: {err}");
            return Err(gst::FlowError::Error);
        }

        let mut n_cur = batch.n_tokens();

        let mut output = String::new();

        // Decode until end of generation
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let n_len = model.context.0.n_ctx() as i32;
        let mut n_tokens = 0;
        while n_cur <= n_len {
            // sample the next token
            {
                let token = model
                    .sampler
                    .0
                    .sample(&model.context.0, batch.n_tokens() - 1);

                model.sampler.0.accept(token);

                // is it an end of stream?
                if model.model.is_eog_token(token) {
                    break;
                }

                let token_str = match model.model.token_to_piece(token, &mut decoder, true, None) {
                    Ok(token_str) => token_str,
                    Err(err) => {
                        gst::warning!(CAT, imp = self, "Can't convert token to string: {err}");
                        "�".to_string()
                    }
                };

                output.push_str(&token_str);

                batch.clear();
                if let Err(err) = batch.add(token, n_cur, &[0], true) {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Failed to add decoded token to batch: {err}"
                    );
                    return Err(gst::FlowError::Error);
                }
            }

            if n_cur % 10 == 0 {
                gst::trace!(
                    CAT,
                    imp = self,
                    "Current output (tokens: {n_tokens}, context: {n_cur}): {output}"
                );
            }
            n_cur += 1;
            n_tokens += 1;

            // Generate next token
            if let Err(err) = model.context.0.decode(&mut batch) {
                gst::error!(CAT, imp = self, "Failed to decode next token: {err}");
                return Err(gst::FlowError::Error);
            }
        }

        if n_cur == n_len {
            gst::warning!(CAT, imp = self, "Context too small, output truncated");
        }

        gst::trace!(
            CAT,
            imp = self,
            "Generated output (tokens: {n_tokens}, context: {n_cur}): {output}"
        );

        // Push back Assistant message for next round
        messages.push(Message {
            role: Role::Assistant,
            content: output.clone(),
        });

        Ok(output)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Role {
    System,
    Assistant,
    User,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::Assistant => "assistant",
            Role::User => "user",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Message {
    role: Role,
    content: String,
}

fn create_jinja_environment(
    tmpl: &str,
) -> Result<minijinja::Environment<'static>, minijinja::Error> {
    use minijinja::{Environment, Error, ErrorKind};

    let mut env = Environment::new();
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    minijinja_contrib::add_to_environment(&mut env);
    env.add_template_owned("chat_template", tmpl.to_string())?;
    env.add_function(
        "raise_exception",
        |x: &str| -> Result<(), minijinja::Error> {
            Err(Error::new(
                ErrorKind::UndefinedError,
                format!("Exception: {x}"),
            ))
        },
    );

    Ok(env)
}

fn apply_jinja_template(
    env: &minijinja::Environment,
    messages: &[Message],
) -> Result<String, minijinja::Error> {
    use minijinja::context;

    let tmpl = env.get_template("chat_template")?;
    tmpl.render(context!(
        enable_thinking => false,
        add_generation_prompt => true,
        messages =>
            messages.iter()
            .map(|m| {
                context!(
                    role => m.role.as_str(),
                    content => m.content,
                )
            })
            .collect::<minijinja::Value>()
    ))
}
