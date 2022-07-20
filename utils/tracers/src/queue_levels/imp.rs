// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/// This tracer provides an easy way to collect queue levels over time of all queues inside a
/// pipeline.
///
/// Example:
///
/// ```console
/// $ GST_TRACERS='queue-levels(file="/tmp/queue_levels.log")' gst-launch-1.0 audiotestsrc ! queue ! fakesink
/// ```
///
/// The generated file is a CSV file of the format
///
/// ```csv
/// timestamp,queue name,queue pointer,cur-level-bytes,cur-level-time,cur-level-buffers,max-size-bytes,max-size-time,max-size-buffers
/// ```
///
/// ## Parameters
///
/// ### `file`
///
/// Specifies the path to the file that will collect the CSV file with the queue levels.
///
/// By default the file is written to `/tmp/queue_levels.log`.
///
/// ### `include-filter`
///
/// Specifies a regular expression for the queue object names that should be included.
///
/// By default this is not set.
///
/// ### `exclude-filter`
///
/// Specifies a regular expression for the queue object names that should **not** be included.
///
/// By default this is not set.
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;
use regex::Regex;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "queue-levels",
        gst::DebugColorFlags::empty(),
        Some("Tracer to collect queue levels"),
    )
});

static QUEUE_TYPE: Lazy<glib::Type> = Lazy::new(|| {
    if let Ok(queue) = gst::ElementFactory::make("queue", None) {
        queue.type_()
    } else {
        gst::warning!(CAT, "Can't instantiate queue element");
        glib::Type::INVALID
    }
});

#[derive(Debug)]
struct Settings {
    file: PathBuf,
    include_filter: Option<Regex>,
    exclude_filter: Option<Regex>,
}

impl Default for Settings {
    fn default() -> Self {
        let mut file = glib::tmp_dir();
        file.push("queue_levels.log");

        Self {
            file,
            include_filter: None,
            exclude_filter: None,
        }
    }
}

impl Settings {
    fn update_from_params(&mut self, obj: &super::QueueLevels, params: String) {
        let s = match gst::Structure::from_str(&format!("queue-levels,{}", params)) {
            Ok(s) => s,
            Err(err) => {
                gst::warning!(CAT, obj: obj, "failed to parse tracer parameters: {}", err);
                return;
            }
        };

        if let Ok(file) = s.get::<&str>("file") {
            gst::log!(CAT, obj: obj, "file= {}", file);
            self.file = PathBuf::from(file);
        }

        if let Ok(filter) = s.get::<&str>("include-filter") {
            gst::log!(CAT, obj: obj, "include filter= {}", filter);
            let filter = match Regex::new(filter) {
                Ok(filter) => Some(filter),
                Err(err) => {
                    gst::error!(
                        CAT,
                        obj: obj,
                        "Failed to compile include-filter regex: {}",
                        err
                    );
                    None
                }
            };
            self.include_filter = filter;
        }

        if let Ok(filter) = s.get::<&str>("exclude-filter") {
            gst::log!(CAT, obj: obj, "exclude filter= {}", filter);
            let filter = match Regex::new(filter) {
                Ok(filter) => Some(filter),
                Err(err) => {
                    gst::error!(
                        CAT,
                        obj: obj,
                        "Failed to compile exclude-filter regex: {}",
                        err
                    );
                    None
                }
            };
            self.exclude_filter = filter;
        }
    }
}

#[derive(Default)]
struct State {
    queues: HashMap<usize, Arc<glib::GString>>,
    log: Vec<LogLine>,
    settings: Settings,
}

struct LogLine {
    timestamp: u64,
    name: Arc<glib::GString>,
    ptr: usize,
    cur_level_bytes: u32,
    cur_level_time: u64,
    cur_level_buffers: u32,
    max_size_bytes: u32,
    max_size_time: u64,
    max_size_buffers: u32,
}

#[derive(Default)]
pub struct QueueLevels {
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for QueueLevels {
    const NAME: &'static str = "GstQueueLevels";
    type Type = super::QueueLevels;
    type ParentType = gst::Tracer;
}

impl ObjectImpl for QueueLevels {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        if let Some(params) = obj.property::<Option<String>>("params") {
            let mut state = self.state.lock().unwrap();
            state.settings.update_from_params(obj, params);
        }

        Lazy::force(&QUEUE_TYPE);

        self.register_hook(TracerHook::ElementNew);
        self.register_hook(TracerHook::ObjectDestroyed);
        self.register_hook(TracerHook::PadPushPost);
        self.register_hook(TracerHook::PadPushListPost);
        #[cfg(feature = "v1_22")]
        {
            self.register_hook(TracerHook::PadChainPost);
            self.register_hook(TracerHook::PadChainListPost);
        }
        #[cfg(not(feature = "v1_22"))]
        {
            self.register_hook(TracerHook::PadPushPost);
            self.register_hook(TracerHook::PadPushListPost);
        }
        self.register_hook(TracerHook::PadPushPre);
        self.register_hook(TracerHook::PadPushListPre);
        self.register_hook(TracerHook::ElementChangeStatePost);
        self.register_hook(TracerHook::PadPushEventPre);
    }

    fn dispose(&self, obj: &Self::Type) {
        use std::io::prelude::*;

        let state = self.state.lock().unwrap();

        let mut file = match std::fs::File::create(&state.settings.file) {
            Ok(file) => file,
            Err(err) => {
                gst::error!(CAT, obj: obj, "Failed to create file: {err}");
                return;
            }
        };

        gst::debug!(
            CAT,
            obj: obj,
            "Writing file {}",
            state.settings.file.display()
        );

        for LogLine {
            timestamp,
            name,
            ptr,
            cur_level_bytes,
            cur_level_time,
            cur_level_buffers,
            max_size_bytes,
            max_size_time,
            max_size_buffers,
        } in &state.log
        {
            if let Err(err) = writeln!(&mut file, "{timestamp},{name},0x{ptr:08x},{cur_level_bytes},{cur_level_time},{cur_level_buffers},{max_size_bytes},{max_size_time},{max_size_buffers}") {
                gst::error!(CAT, obj: obj, "Failed to write to file: {err}");
                return;
            }
        }
    }
}

impl GstObjectImpl for QueueLevels {}

impl TracerImpl for QueueLevels {
    fn element_new(&self, _ts: u64, element: &gst::Element) {
        if element.type_() != *QUEUE_TYPE {
            return;
        }

        let tracer = self.instance();
        let ptr = element.as_ptr() as usize;
        gst::debug!(
            CAT,
            obj: &tracer,
            "new queue: {} 0x{:08x}",
            element.name(),
            ptr
        );

        let mut state = self.state.lock().unwrap();

        let name = element.name();
        if let Some(ref filter) = state.settings.include_filter {
            if !filter.is_match(&name) {
                return;
            }
        }
        if let Some(ref filter) = state.settings.exclude_filter {
            if filter.is_match(&name) {
                return;
            }
        }

        state.queues.entry(ptr).or_insert_with(|| Arc::new(name));
    }

    fn object_destroyed(&self, _ts: u64, object: std::ptr::NonNull<gst::ffi::GstObject>) {
        let ptr = object.as_ptr() as usize;
        let mut state = self.state.lock().unwrap();
        state.queues.remove(&ptr);
    }

    fn pad_push_pre(&self, ts: u64, pad: &gst::Pad, _buffer: &gst::Buffer) {
        let element =
            if let Some(parent) = pad.parent().and_then(|p| p.downcast::<gst::Element>().ok()) {
                if parent.type_() == *QUEUE_TYPE {
                    parent
                } else {
                    return;
                }
            } else {
                return;
            };

        self.log(&element, ts);
    }

    fn pad_push_list_pre(&self, ts: u64, pad: &gst::Pad, _list: &gst::BufferList) {
        let element =
            if let Some(parent) = pad.parent().and_then(|p| p.downcast::<gst::Element>().ok()) {
                if parent.type_() == *QUEUE_TYPE {
                    parent
                } else {
                    return;
                }
            } else {
                return;
            };

        self.log(&element, ts);
    }

    #[cfg(not(feature = "v1_22"))]
    fn pad_push_post(&self, ts: u64, pad: &gst::Pad, _result: gst::FlowReturn) {
        let element = if let Some(parent) = pad
            .peer()
            .and_then(|p| p.parent())
            .and_then(|p| p.downcast::<gst::Element>().ok())
        {
            if parent.type_() == *QUEUE_TYPE {
                parent
            } else {
                return;
            }
        } else {
            return;
        };

        self.log(&element, ts);
    }

    #[cfg(not(feature = "v1_22"))]
    fn pad_push_list_post(&self, ts: u64, pad: &gst::Pad, _result: gst::FlowReturn) {
        let element = if let Some(parent) = pad
            .peer()
            .and_then(|p| p.parent())
            .and_then(|p| p.downcast::<gst::Element>().ok())
        {
            if parent.type_() == *QUEUE_TYPE {
                parent
            } else {
                return;
            }
        } else {
            return;
        };

        self.log(&element, ts);
    }

    #[cfg(feature = "v1_22")]
    fn pad_chain_post(&self, ts: u64, pad: &gst::Pad, _result: gst::FlowReturn) {
        let element =
            if let Some(parent) = pad.parent().and_then(|p| p.downcast::<gst::Element>().ok()) {
                if parent.type_() == *QUEUE_TYPE {
                    parent
                } else {
                    return;
                }
            } else {
                return;
            };

        self.log(&element, ts);
    }

    #[cfg(feature = "v1_22")]
    fn pad_chain_list_post(&self, ts: u64, pad: &gst::Pad, _result: gst::FlowReturn) {
        let element =
            if let Some(parent) = pad.parent().and_then(|p| p.downcast::<gst::Element>().ok()) {
                if parent.type_() == *QUEUE_TYPE {
                    parent
                } else {
                    return;
                }
            } else {
                return;
            };

        self.log(&element, ts);
    }

    fn element_change_state_post(
        &self,
        ts: u64,
        element: &gst::Element,
        change: gst::StateChange,
        _result: gst::StateChangeReturn,
    ) {
        if change.next() != gst::State::Null {
            return;
        }

        if element.type_() != *QUEUE_TYPE {
            return;
        }

        self.log(element, ts);
    }

    fn pad_push_event_pre(&self, ts: u64, pad: &gst::Pad, ev: &gst::Event) {
        if ev.type_() != gst::EventType::FlushStop {
            return;
        }

        if let Some(parent) = pad.parent().and_then(|p| p.downcast::<gst::Element>().ok()) {
            if parent.type_() == *QUEUE_TYPE {
                self.log(&parent, ts);
            }
        }
    }
}

impl QueueLevels {
    fn log(&self, element: &gst::Element, timestamp: u64) {
        let ptr = element.as_ptr() as usize;

        let mut state = self.state.lock().unwrap();
        let name = match state.queues.get(&ptr) {
            Some(name) => name.clone(),
            None => return,
        };

        let cur_level_bytes = element.property::<u32>("current-level-bytes");
        let cur_level_time = element.property::<u64>("current-level-time");
        let cur_level_buffers = element.property::<u32>("current-level-buffers");
        let max_size_bytes = element.property::<u32>("max-size-bytes");
        let max_size_time = element.property::<u64>("max-size-time");
        let max_size_buffers = element.property::<u32>("max-size-buffers");
        state.log.push(LogLine {
            timestamp,
            name,
            ptr,
            cur_level_bytes,
            cur_level_time,
            cur_level_buffers,
            max_size_bytes,
            max_size_time,
            max_size_buffers,
        });
    }
}
