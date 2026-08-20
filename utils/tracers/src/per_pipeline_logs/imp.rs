// Copyright (C) 2026 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * tracer-per-pipeline-logs:
 *
 * This tracer groups logs per pipeline. When active, `GST_DEBUG_FILE` and `GST_DEBUG` are ignored.
 *
 * Example:
 *
 * ```console
 * $ GST_TRACERS='per-pipeline-logs(pipelines-thresholds="4@pipeline0&videoencoder:9@Discovery-pipeline-video_0-VP8")' gst-launch-1.0 videotestsrc ! webrtcsink run-signalling-server=true
 * ```
 *
 * In this example, logs up to INFO log levels that are detected as relating to the main pipeline
 * will be printed out, as well as videoencoder logs up to TRACE log levels that are detected
 * as relating to the VP8 discovery pipeline.
 *
 * ## Parameters
 *
 * ### `pipelines-thresholds`
 *
 * A string specifying per-pipeline log levels.
 *
 * pipeline thresholds are `&`-separated, and made up of:
 *
 * * A list of comma-separated `category:level` or level (for default threshold) thresholds
 * * A mandatory @pipeline-name suffix, only actual GstPipeline names are supported.
 *
 * Wildcards in category names are supported.
 *
 * ### `silent`
 *
 * A boolean specifying whether the logs should be printed using the default GStreamer
 * log function, false by default.
 *
 * ## Interface
 *
 * The `new-logs` signal is emitted for every new filtered log message, and the thresholds
 * can be reconfigured at runtime using the `set-params` action signal.
 */
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::str::FromStr;
use std::sync::LazyLock;

fn log_handler(
    category: gst::DebugCategory,
    level: gst::DebugLevel,
    file: &gst::glib::GStr,
    function: &gst::glib::GStr,
    line: u32,
    object: Option<&gst::LoggedObject>,
    message: &gst::DebugMessage,
) {
    /* Avoid deadlock from our own logging */
    if category == *CAT {
        return;
    }

    let mut pipeline_name = None;
    let current_thread = gst::glib::thread_guard::thread_id();

    let mut state = STATE.lock().unwrap();

    if !state.seen_categories.contains(category.name()) {
        let mut new_named_thresholds: HashMap<String, HashMap<String, gst::DebugLevel>> =
            HashMap::new();
        for (pipeline_name, patterns) in &state.settings.pattern_thresholds {
            for (pattern, level) in patterns {
                if pattern.matches(category.name()) {
                    new_named_thresholds
                        .entry(pipeline_name.to_string())
                        .or_default()
                        .insert(category.name().to_string(), *level);
                }
            }
        }

        for (pipeline_name, patterns) in new_named_thresholds.drain() {
            state
                .settings
                .named_thresholds
                .entry(pipeline_name)
                .or_default()
                .extend(patterns);
        }

        state.seen_categories.insert(category.name().to_string());
    }

    if let Some(object) = object {
        let object_ptr = object.as_ptr() as usize;
        if let Some(top_level_ptr) = state
            .ancestors
            .get(&object_ptr)
            .and_then(|ancestors| ancestors.front())
            && let Some(name) = state.pipelines.get(top_level_ptr)
        {
            pipeline_name = Some(name);
        } else if let Some(name) = state.pipelines.get(&object_ptr) {
            pipeline_name = Some(name);
        }
    }

    if pipeline_name.is_none()
        && let Some(top_level_ptr) = state.streaming_threads.get(&current_thread)
        && let Some(name) = state.pipelines.get(top_level_ptr)
    {
        pipeline_name = Some(name);
    }

    let selected = if let Some(name) = pipeline_name {
        if let Some(named_thresholds) = state.settings.named_thresholds.get(name)
            && let Some(named_threshold) = named_thresholds.get(category.name())
        {
            level <= *named_threshold
        } else if let Some(default_threshold) = state.settings.default_thresholds.get(name) {
            level <= *default_threshold
        } else {
            false
        }
    } else {
        true
    };

    let tracer_obj = state.tracer_obj.clone();
    let pipeline_name = pipeline_name.map(|pn| pn.to_owned());

    let silent = state.settings.silent;

    drop(state);

    if selected
        && let Some(tracer_obj) = tracer_obj
        && let Some(formatted_message) = message.get()
    {
        if !silent {
            gst::log::log_default(category, level, file, function, line, object, message);
        }

        tracer_obj.emit_by_name::<()>(
            "new-log",
            &[
                &pipeline_name,
                &level,
                &file,
                &function,
                &line,
                &object.map(|o| o.to_string()),
                &category.name(),
                &formatted_message.to_string(),
            ],
        );
    }
}

#[derive(Debug, Default)]
struct Settings {
    default_thresholds: HashMap<String, gst::DebugLevel>,
    named_thresholds: HashMap<String, HashMap<String, gst::DebugLevel>>,
    pattern_thresholds: HashMap<String, Vec<(glob::Pattern, gst::DebugLevel)>>,
    silent: bool,
}

fn debug_level_from_str(value: &str) -> Option<gst::DebugLevel> {
    match value {
        "0" => Some(gst::DebugLevel::None),
        "1" => Some(gst::DebugLevel::Error),
        "2" => Some(gst::DebugLevel::Warning),
        "3" => Some(gst::DebugLevel::Fixme),
        "4" => Some(gst::DebugLevel::Info),
        "5" => Some(gst::DebugLevel::Debug),
        "6" => Some(gst::DebugLevel::Log),
        "7" => Some(gst::DebugLevel::Trace),
        "9" => Some(gst::DebugLevel::Memdump),
        _ => None,
    }
}

impl Settings {
    fn update_from_params(
        &mut self,
        imp: &PerPipelineLogs,
        params: &str,
    ) -> Option<(gst::DebugLevel, HashMap<String, gst::DebugLevel>)> {
        let s = match gst::Structure::from_str(&format!("per-pipeline-logs,{params}")) {
            Ok(s) => s,
            Err(err) => {
                gst::warning!(CAT, imp = imp, "failed to parse tracer parameters: {}", err);
                return None;
            }
        };

        if let Ok(silent) = s.get::<bool>("silent") {
            self.silent = silent;
        }

        if let Ok(pipelines_thresholds) = s.get::<&str>("pipelines-thresholds") {
            gst::log!(
                CAT,
                imp = imp,
                "pipelines thresholds= {}",
                pipelines_thresholds
            );

            let mut global_default_threshold = gst::DebugLevel::None;
            let mut global_pattern_thresholds: HashMap<String, gst::DebugLevel> = HashMap::new();

            for pipeline_thresholds in pipelines_thresholds.split('&') {
                if let Some((thresholds, pipeline_name)) = pipeline_thresholds.split_once('@') {
                    let mut default_threshold = gst::DebugLevel::None;
                    let mut pattern_thresholds = vec![];

                    for threshold in thresholds.split(',') {
                        if let Some((name, threshold)) = threshold.split_once(':') {
                            if let Some(level) = debug_level_from_str(threshold) {
                                let Ok(pattern) = glob::Pattern::new(name) else {
                                    gst::error!(CAT, imp = imp, "Invalid pattern {name}",);
                                    continue;
                                };

                                pattern_thresholds.push((pattern, level));
                                if let Some(threshold) = global_pattern_thresholds.get(name) {
                                    if *threshold < level {
                                        global_pattern_thresholds.insert(name.to_string(), level);
                                    }
                                } else {
                                    global_pattern_thresholds.insert(name.to_string(), level);
                                }
                            } else {
                                gst::error!(
                                    CAT,
                                    imp = imp,
                                    "Failed to parse named threshold {}",
                                    threshold
                                );
                            }
                        } else {
                            if let Some(level) = debug_level_from_str(threshold) {
                                if global_default_threshold < level {
                                    global_default_threshold = level;
                                }
                                default_threshold = level;
                            } else {
                                gst::error!(
                                    CAT,
                                    imp = imp,
                                    "Failed to parse named threshold {}",
                                    threshold
                                );
                            }
                        }
                    }

                    self.default_thresholds
                        .insert(pipeline_name.to_string(), default_threshold);
                    self.pattern_thresholds
                        .insert(pipeline_name.to_string(), pattern_thresholds);
                } else {
                    gst::error!(
                        CAT,
                        imp = imp,
                        "pipelines-thresholds must be in the form default-threshold,threshold-name:threshold,[..]@pipeline-name&[..]"
                    );
                }
            }

            Some((global_default_threshold, global_pattern_thresholds))
        } else {
            None
        }
    }
}

#[derive(Default)]
struct State {
    ancestors: HashMap<usize, VecDeque<usize>>,
    children: HashMap<usize, HashSet<usize>>,
    pipelines: HashMap<usize, String>,
    // In order to capture logs that did not refer to an object, or logs
    // that referred to an object whose topology was not tracked, we make
    // the assumption that logs made from known streaming threads relate
    // to the same top level object as the current top level object for the
    // pad parameter to the push / chain / chain_list hooks.
    streaming_threads: HashMap<usize, usize>,
    settings: Settings,
    tracer_obj: Option<gst::Object>,
    seen_categories: HashSet<String>,
    log_function: Option<gst::DebugLogFunction>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "per-pipeline-logs",
        gst::DebugColorFlags::empty(),
        Some("Tracer to group logs per pipeline"),
    )
});

// TODO: RWLock might be more appropriate
static STATE: LazyLock<Arc<Mutex<State>>> =
    LazyLock::new(|| Arc::new(Mutex::new(State::default())));

#[derive(Default)]
pub struct PerPipelineLogs {}

#[glib::object_subclass]
impl ObjectSubclass for PerPipelineLogs {
    const NAME: &'static str = "GstPerPipelineLogs";
    type Type = super::PerPipelineLogs;
    type ParentType = gst::Tracer;
}

impl ObjectImpl for PerPipelineLogs {
    fn constructed(&self) {
        self.parent_constructed();

        self.register_hook(TracerHook::ObjectParentSet);
        self.register_hook(TracerHook::ObjectCreated);
        self.register_hook(TracerHook::ObjectDestroyed);
        self.register_hook(TracerHook::PadChainPre);
        self.register_hook(TracerHook::PadChainListPre);
        self.register_hook(TracerHook::PadPushPre);

        gst::log::remove_default_log_function();

        if let Some(params) = self.obj().property::<Option<String>>("params") {
            let mut state = STATE.lock().unwrap();
            let thresholds = state.settings.update_from_params(self, &params);
            drop(state);

            if let Some((global_default_threshold, global_pattern_thresholds)) = thresholds {
                // This also resets all the named thresholds
                gst::log::set_default_threshold(global_default_threshold);

                for (name, level) in global_pattern_thresholds {
                    gst::log::set_threshold_for_name(&name, level);
                }
            }
        }

        let log_function = gst::log::add_log_function(log_handler);

        {
            let mut state = STATE.lock().unwrap();
            state.tracer_obj = Some(self.obj().clone().upcast());
            state.log_function = Some(log_function);
        }
    }

    fn dispose(&self) {
        let log_function = STATE.lock().unwrap().log_function.take();
        if let Some(log_function) = log_function {
            gst::log::remove_log_function(log_function);
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
            vec![
                glib::subclass::Signal::builder("new-log")
                    // Pipeline name, level, file, function, line, object name, category name, message
                    .param_types([
                        Option::<String>::static_type(),
                        gst::DebugLevel::static_type(),
                        String::static_type(),
                        String::static_type(),
                        u32::static_type(),
                        Option::<String>::static_type(),
                        String::static_type(),
                        String::static_type(),
                    ])
                    .build(),
                glib::subclass::Signal::builder("set-params")
                    .param_types([String::static_type()])
                    .action()
                    .class_handler(move |args| {
                        let this = args[0].get::<super::PerPipelineLogs>().unwrap();
                        let params = args[1].get::<&str>().unwrap();
                        let mut state = STATE.lock().unwrap();
                        state.seen_categories.clear();
                        let thresholds = state.settings.update_from_params(this.imp(), params);
                        drop(state);

                        if let Some((global_default_threshold, global_pattern_thresholds)) =
                            thresholds
                        {
                            // This also resets all the named thresholds
                            gst::log::set_default_threshold(global_default_threshold);

                            for (name, level) in global_pattern_thresholds {
                                gst::log::set_threshold_for_name(&name, level);
                            }
                        }

                        None
                    })
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }
}

impl State {
    // Prepend ancestor to object ancestor list and recurses into the children, for instance
    // when an element with a pad is added to a bin, the ancestor list for the element looks like [],
    // and the ancestor list for the pad looks like [element_ptr], after this is called
    // the ancestor list for the element is the same as the ancestor list for the bin
    // plus the bin itself, and the ancestor list for the pad is the same as the ancestor
    // list for the element plus the element itself
    fn add_ancestors(&mut self, ancestor_ptrs: &VecDeque<usize>, obj_ptr: usize) {
        let ancestors = self.ancestors.entry(obj_ptr).or_default();

        for ancestor_ptr in ancestor_ptrs.iter().rev() {
            ancestors.push_front(*ancestor_ptr);
        }

        if let Some(children) = self.children.get(&obj_ptr) {
            for child in children.clone() {
                self.add_ancestors(ancestor_ptrs, child);
            }
        }
    }

    // Reverse operation of add_ancestors, taking the same example after this
    // is called the ancestor list for the element looks like [], and the ancestor
    // list for the pad looks like [element_ptr]
    fn remove_ancestors(&mut self, ancestor_ptr: usize, obj_ptr: usize) {
        if let Some(ancestors) = self.ancestors.get_mut(&obj_ptr) {
            while let Some(ancestor) = ancestors.pop_front() {
                if ancestor == ancestor_ptr {
                    break;
                }
            }
        }

        if let Some(children) = self.children.get(&obj_ptr) {
            for child in children.clone() {
                self.remove_ancestors(ancestor_ptr, child);
            }
        }
    }
}

impl PerPipelineLogs {
    fn dump(&self, state: &State) {
        if CAT.above_threshold(gst::DebugLevel::Trace) {
            gst::trace!(CAT, imp = self, "Dumping");

            for (obj_ptr, ancestors) in state.ancestors.iter() {
                gst::trace!(CAT, imp = self, "{obj_ptr} has ancestors {ancestors:?}");
            }

            for (obj_ptr, children) in state.children.iter() {
                gst::trace!(CAT, imp = self, "{obj_ptr} has children {children:?}");
            }

            for (obj_ptr, name) in state.pipelines.iter() {
                gst::trace!(CAT, imp = self, "{obj_ptr} is a pipeline named {name}");
            }
        }
    }
}

impl GstObjectImpl for PerPipelineLogs {}

impl TracerImpl for PerPipelineLogs {
    fn object_parent_set(&self, _ts: u64, object: &gst::Object, parent: Option<&gst::Object>) {
        let mut state = STATE.lock().unwrap();

        let object_ptr = object.as_ptr() as usize;
        if let Some(parent) = parent {
            let parent_ptr = parent.as_ptr() as usize;

            gst::debug!(
                CAT,
                imp = self,
                "parent {:?} with pointer {} added child {:?} with ptr {}",
                parent,
                parent_ptr,
                object,
                object_ptr
            );

            let mut ancestors = state
                .ancestors
                .get(&parent_ptr)
                .cloned()
                .unwrap_or_default();
            ancestors.push_back(parent_ptr);
            state.add_ancestors(&ancestors, object_ptr);

            let children = state.children.entry(parent_ptr).or_default();
            children.insert(object_ptr);
        } else {
            gst::debug!(
                CAT,
                imp = self,
                "parent removed object {:?} with ptr {}",
                object,
                object_ptr
            );

            if let Some(ancestor_ptr) = state
                .ancestors
                .get(&object_ptr)
                .and_then(VecDeque::back)
                .copied()
            {
                gst::debug!(CAT, "parent ptr is {}", ancestor_ptr);
                state.remove_ancestors(ancestor_ptr, object_ptr);

                if let Some(children) = state.children.get_mut(&ancestor_ptr) {
                    children.remove(&object_ptr);
                }
            }
        }

        self.dump(&state);
    }

    fn object_destroyed(&self, _ts: u64, object: std::ptr::NonNull<gst::ffi::GstObject>) {
        let mut state = STATE.lock().unwrap();

        let object_ptr = object.as_ptr() as usize;

        let mut dump = false;

        if state.ancestors.remove(&object_ptr).is_some() {
            gst::debug!(
                CAT,
                imp = self,
                "removed ancestors for object with ptr {object_ptr}"
            );
            dump = true;
        }

        if let Some(children) = state.children.remove(&object_ptr) {
            gst::debug!(
                CAT,
                imp = self,
                "removed children for object with ptr {object_ptr}"
            );
            for child in children {
                state.remove_ancestors(object_ptr, child);
            }
            dump = true;
        }

        if state.pipelines.remove(&object_ptr).is_some() {
            gst::debug!(CAT, imp = self, "removed pipeline with ptr {object_ptr}");
            dump = true;
        }

        if dump {
            self.dump(&state);
        }
    }

    fn object_created(&self, _ts: u64, object: &gst::Object) {
        let object_ptr = object.as_ptr() as usize;

        if let Some(pipeline) = object.downcast_ref::<gst::Pipeline>() {
            let bus_ptr = pipeline.bus().map(|bus| bus.as_ptr() as usize);
            let mut state = STATE.lock().unwrap();
            state
                .pipelines
                .insert(object_ptr, pipeline.name().to_string());
            if let Some(bus_ptr) = bus_ptr {
                state
                    .ancestors
                    .insert(bus_ptr, VecDeque::from([object_ptr]));
            }
            self.dump(&state);
        }
    }

    fn pad_chain_pre(&self, _ts: u64, pad: &gst::Pad, _buffer: &gst::Buffer) {
        let pad_ptr = pad.as_ptr() as usize;

        let thread_id = gst::glib::thread_guard::thread_id();

        let mut state = STATE.lock().unwrap();

        if let Some(&top_level) = state
            .ancestors
            .get(&pad_ptr)
            .and_then(|ancestors| ancestors.front())
        {
            // TODO: figure out how to clean up streaming_threads
            state.streaming_threads.insert(thread_id, top_level);
        }
    }

    fn pad_chain_list_pre(&self, _ts: u64, pad: &gst::Pad, _buffer_list: &gst::BufferList) {
        let pad_ptr = pad.as_ptr() as usize;

        let thread_id = gst::glib::thread_guard::thread_id();

        let mut state = STATE.lock().unwrap();

        if let Some(&top_level) = state
            .ancestors
            .get(&pad_ptr)
            .and_then(|ancestors| ancestors.front())
        {
            // TODO: figure out how to clean up streaming_threads
            state.streaming_threads.insert(thread_id, top_level);
        }
    }

    fn pad_push_pre(&self, _ts: u64, pad: &gst::Pad, _buffer: &gst::Buffer) {
        let pad_ptr = pad.as_ptr() as usize;

        let thread_id = gst::glib::thread_guard::thread_id();

        let mut state = STATE.lock().unwrap();

        if let Some(&top_level) = state
            .ancestors
            .get(&pad_ptr)
            .and_then(|ancestors| ancestors.front())
        {
            // TODO: figure out how to clean up streaming_threads
            state.streaming_threads.insert(thread_id, top_level);
        }
    }

    // TODO: we could perhaps monitor message posting to identify more threads
    // tied with objects
}
