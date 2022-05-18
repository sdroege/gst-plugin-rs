// Copyright (C) 2022 OneStream Live <guillaume.desmottes@onestream.live>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/// This tracer provides an easy way to take a snapshot of all the pipelines without
/// having to modify the application.
/// One just have to load the tracer and send the `SIGUSR1` UNIX signal to take snapshots.
/// It currently only works on UNIX systems.
///
/// When taking a snapshot pipelines are saved to DOT files, but the tracer may be
/// extended in the future to dump more information.
///
/// Example:
///
/// ```console
/// $ GST_TRACERS="pipeline-snapshot" GST_DEBUG_DUMP_DOT_DIR=. gst-launch-1.0 audiotestsrc ! fakesink
/// ```
/// You can then trigger a snapshot using:
/// ```console
/// $ kill -SIGUSR1 $(pidof gst-launch-1.0)
/// ```
///
/// Parameters can be passed to configure the tracer:
/// - `dot-prefix` (string, default: "pipeline-snapshot-"): when dumping pipelines to a `dot` file each file is named `$prefix$pipeline_name.dot`.
/// - `dot-ts` (boolean, default: "true"): if the current timestamp should be added as a prefix to each pipeline `dot` file.
///
/// Example:
///
/// ```console
/// $ GST_TRACERS="pipeline-snapshot(dot-prefix="badger-",dot-ts=false)" GST_DEBUG_DUMP_DOT_DIR=. gst-launch-1.0 audiotestsrc ! fakesink
/// ```
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use gst::glib;
use gst::glib::translate::ToGlibPtr;
use gst::prelude::*;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "pipeline-snapshot",
        gst::DebugColorFlags::empty(),
        Some("pipeline snapshot tracer"),
    )
});

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ElementPtr(std::ptr::NonNull<gst::ffi::GstElement>);

unsafe impl Send for ElementPtr {}
unsafe impl Sync for ElementPtr {}

impl ElementPtr {
    fn from_ref(element: &gst::Element) -> Self {
        let p = element.to_glib_none().0;
        Self(std::ptr::NonNull::new(p).unwrap())
    }

    fn from_object_ptr(p: std::ptr::NonNull<gst::ffi::GstObject>) -> Self {
        let p = p.cast();
        Self(p)
    }
}

#[derive(Debug)]
struct Settings {
    dot_prefix: String,
    dot_ts: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dot_prefix: "pipeline-snapshot-".to_string(),
            dot_ts: true,
        }
    }
}

impl Settings {
    fn update_from_params(&mut self, obj: &super::PipelineSnapshot, params: String) {
        let s = match gst::Structure::from_str(&format!("pipeline-snapshot,{}", params)) {
            Ok(s) => s,
            Err(err) => {
                gst::warning!(CAT, obj: obj, "failed to parse tracer parameters: {}", err);
                return;
            }
        };

        if let Ok(dot_prefix) = s.get("dot-prefix") {
            gst::log!(CAT, obj: obj, "dot-prefix = {}", dot_prefix);
            self.dot_prefix = dot_prefix;
        }

        if let Ok(dot_ts) = s.get("dot-ts") {
            gst::log!(CAT, obj: obj, "dot-ts = {}", dot_ts);
            self.dot_ts = dot_ts;
        }
    }
}

#[derive(Default)]
pub struct PipelineSnapshot {
    pipelines: Arc<Mutex<HashMap<ElementPtr, glib::WeakRef<gst::Element>>>>,
    handles: Mutex<Option<Handles>>,
}

struct Handles {
    #[cfg(unix)]
    signal: signal_hook::iterator::Handle,
    thread: std::thread::JoinHandle<()>,
}

#[glib::object_subclass]
impl ObjectSubclass for PipelineSnapshot {
    const NAME: &'static str = "GstPipelineSnapshot";
    type Type = super::PipelineSnapshot;
    type ParentType = gst::Tracer;
}

impl ObjectImpl for PipelineSnapshot {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        let mut settings = Settings::default();
        if let Some(params) = obj.property::<Option<String>>("params") {
            settings.update_from_params(obj, params);
        }

        self.register_hook(TracerHook::ElementNew);
        self.register_hook(TracerHook::ObjectDestroyed);

        if let Err(err) = self.setup_signal(settings) {
            gst::warning!(CAT, obj: obj, "failed to setup UNIX signals: {}", err);
        }
    }

    fn dispose(&self, _obj: &Self::Type) {
        let mut handles = self.handles.lock().unwrap();
        if let Some(handles) = handles.take() {
            #[cfg(unix)]
            handles.signal.close();
            handles.thread.join().unwrap();
        }
    }
}

impl GstObjectImpl for PipelineSnapshot {}

impl TracerImpl for PipelineSnapshot {
    fn element_new(&self, _ts: u64, element: &gst::Element) {
        if element.is::<gst::Pipeline>() {
            let tracer = self.instance();
            gst::debug!(CAT, obj: &tracer, "new pipeline: {}", element.name());

            let weak = element.downgrade();
            let mut pipelines = self.pipelines.lock().unwrap();
            pipelines.insert(ElementPtr::from_ref(element), weak);
        }
    }

    fn object_destroyed(&self, _ts: u64, object: std::ptr::NonNull<gst::ffi::GstObject>) {
        let mut pipelines = self.pipelines.lock().unwrap();
        let object = ElementPtr::from_object_ptr(object);
        pipelines.remove(&object);
    }
}

impl PipelineSnapshot {
    #[cfg(unix)]
    fn setup_signal(&self, settings: Settings) -> anyhow::Result<()> {
        use signal_hook::consts::signal::*;
        use signal_hook::iterator::Signals;

        let mut signals = Signals::new(&[SIGUSR1])?;
        let signal_handle = signals.handle();

        let tracer_weak = self.instance().downgrade();
        let pipelines = self.pipelines.clone();

        let thread_handle = std::thread::spawn(move || {
            for signal in &mut signals {
                match signal {
                    SIGUSR1 => {
                        let tracer = match tracer_weak.upgrade() {
                            Some(tracer) => tracer,
                            None => break,
                        };

                        let pipelines = {
                            let weaks = pipelines.lock().unwrap();
                            weaks
                                .values()
                                .filter_map(|w| w.upgrade())
                                .collect::<Vec<_>>()
                        };

                        for pipeline in pipelines.into_iter() {
                            let pipeline = pipeline.downcast::<gst::Pipeline>().unwrap();
                            gst::debug!(CAT, obj: &tracer, "dump {}", pipeline.name());

                            let dump_name = format!("{}{}", settings.dot_prefix, pipeline.name());

                            if settings.dot_ts {
                                gst::debug_bin_to_dot_file_with_ts(
                                    &pipeline,
                                    gst::DebugGraphDetails::all(),
                                    &dump_name,
                                );
                            } else {
                                gst::debug_bin_to_dot_file(
                                    &pipeline,
                                    gst::DebugGraphDetails::all(),
                                    &dump_name,
                                );
                            }
                        }
                    }
                    _ => unreachable!(),
                }
            }
        });

        let mut handles = self.handles.lock().unwrap();
        *handles = Some(Handles {
            signal: signal_handle,
            thread: thread_handle,
        });

        Ok(())
    }

    #[cfg(not(unix))]
    fn setup_signal(&self) -> anyhow::Result<()> {
        anyhow::bail!("only supported on UNIX system");
    }
}
