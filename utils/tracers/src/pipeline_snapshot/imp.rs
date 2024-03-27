// Copyright (C) 2022 OneStream Live <guillaume.desmottes@onestream.live>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * tracer-pipeline-snapshot:
 *
 * This tracer provides an easy way to take a snapshot of all the pipelines without
 * having to modify the application.
 * One just have to load the tracer and send the `SIGUSR1` UNIX signal to take snapshots.
 * It currently only works on UNIX systems.
 *
 * When taking a snapshot pipelines are saved to DOT files, but the tracer may be
 * extended in the future to dump more information.
 *
 * Example:
 *
 * ```console
 * $ GST_TRACERS="pipeline-snapshot" GST_DEBUG_DUMP_DOT_DIR=. gst-launch-1.0 audiotestsrc ! fakesink
 * ```
 * You can then trigger a snapshot using:
 * ```console
 * $ kill -SIGUSR1 $(pidof gst-launch-1.0)
 * ```
 *
 * Parameters can be passed to configure the tracer:
 * - `dot-dir` (string, default: None): directory where to place dot files (overriding `GST_DEBUG_DUMP_DOT_DIR`). Set to `xdg-cache` to use the XDG cache directory.
 * - `dot-prefix` (string, default: "pipeline-snapshot-"): when dumping pipelines to a `dot` file each file is named `$prefix$pipeline_name.dot`.
 * - `dot-ts` (boolean, default: "true"): if the current timestamp should be added as a prefix to each pipeline `dot` file.
 *
 * Example:
 *
 * ```console
 * $ GST_TRACERS="pipeline-snapshot(dot-prefix="badger-",dot-ts=false)" GST_DEBUG_DUMP_DOT_DIR=. gst-launch-1.0 audiotestsrc ! fakesink
 * ```
 */
use std::collections::HashMap;
use std::io::Write;
use std::str::FromStr;
use std::sync::{Arc, Mutex, RwLock};

use gst::glib;
use gst::glib::translate::ToGlibPtr;
use gst::glib::Properties;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::LazyLock;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "pipeline-snapshot",
        gst::DebugColorFlags::empty(),
        Some("pipeline snapshot tracer"),
    )
});

static START_TIME: LazyLock<gst::ClockTime> = LazyLock::new(gst::get_timestamp);

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
    dot_prefix: Option<String>,
    dot_ts: bool,
    dot_dir: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dot_dir: None,
            dot_prefix: Some("pipeline-snapshot-".to_string()),
            dot_ts: true,
        }
    }
}

impl Settings {
    fn set_dot_dir(&mut self, dot_dir: Option<String>) {
        if let Some(dot_dir) = dot_dir {
            if dot_dir == "xdg-cache" {
                let mut path = dirs::cache_dir().expect("Failed to find cache directory");
                path.push("gstreamer-dots");
                self.dot_dir = path.to_str().map(|s| s.to_string());
            } else {
                self.dot_dir = Some(dot_dir);
            }
        } else {
            self.dot_dir = std::env::var("GST_DEBUG_DUMP_DOT_DIR").ok();
        }
    }

    fn update_from_params(&mut self, imp: &PipelineSnapshot, params: String) {
        let s = match gst::Structure::from_str(&format!("pipeline-snapshot,{params}")) {
            Ok(s) => s,
            Err(err) => {
                gst::warning!(CAT, imp = imp, "failed to parse tracer parameters: {}", err);
                return;
            }
        };

        if let Ok(dot_dir) = s.get("dot-dir") {
            self.set_dot_dir(dot_dir);
            gst::log!(CAT, imp = imp, "dot-dir = {:?}", self.dot_dir);
        }

        if let Ok(dot_prefix) = s.get("dot-prefix") {
            gst::log!(CAT, imp = imp, "dot-prefix = {:?}", dot_prefix);
            self.dot_prefix = dot_prefix;
        }

        if let Ok(dot_ts) = s.get("dot-ts") {
            gst::log!(CAT, imp = imp, "dot-ts = {}", dot_ts);
            self.dot_ts = dot_ts;
        }
    }
}

#[derive(Properties, Debug, Default)]
#[properties(wrapper_type = super::PipelineSnapshot)]
pub struct PipelineSnapshot {
    #[property(name="dot-dir", get, set = Self::set_dot_dir, construct_only, type = String, member = dot_dir, blurb = "Directory where to place dot files")]
    #[property(name="dot-prefix", get, set, type = String, member = dot_prefix, blurb = "Prefix for dot files")]
    #[property(name="dot-ts", get, set, type = bool, member = dot_ts, blurb = "Add timestamp to dot files")]
    settings: RwLock<Settings>,
    pipelines: Arc<Mutex<HashMap<ElementPtr, glib::WeakRef<gst::Element>>>>,
    handles: Mutex<Option<Handles>>,
}

#[derive(Debug)]
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

#[glib::derived_properties]
impl ObjectImpl for PipelineSnapshot {
    fn constructed(&self) {
        let _ = START_TIME.as_ref();
        self.parent_constructed();

        let mut settings = self.settings.write().unwrap();
        if let Some(params) = self.obj().property::<Option<String>>("params") {
            settings.update_from_params(self, params);
        }

        self.register_hook(TracerHook::ElementNew);
        self.register_hook(TracerHook::ObjectDestroyed);

        if let Err(err) = self.setup_signal() {
            gst::warning!(CAT, imp = self, "failed to setup UNIX signals: {}", err);
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![glib::subclass::Signal::builder("snapshot")
                .action()
                .class_handler(|_, args| {
                    args[0].get::<super::PipelineSnapshot>().unwrap().snapshot();

                    None
                })
                .build()]
        });

        SIGNALS.as_ref()
    }

    fn dispose(&self) {
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
            gst::debug!(CAT, imp = self, "new pipeline: {}", element.name());

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
    fn set_dot_dir(&self, dot_dir: Option<String>) {
        let mut settings = self.settings.write().unwrap();
        settings.set_dot_dir(dot_dir);
    }

    pub(crate) fn snapshot(&self) {
        let pipelines = {
            let weaks = self.pipelines.lock().unwrap();
            weaks
                .values()
                .filter_map(|w| w.upgrade())
                .collect::<Vec<_>>()
        };

        let settings = self.settings.read().unwrap();
        let dot_dir = if let Some(dot_dir) = settings.dot_dir.as_ref() {
            dot_dir
        } else {
            gst::info!(CAT, imp = self, "No dot-dir set, not dumping pipelines");
            return;
        };

        for pipeline in pipelines.into_iter() {
            let pipeline = pipeline.downcast::<gst::Pipeline>().unwrap();
            gst::debug!(CAT, imp = self, "dump {}", pipeline.name());

            let dump_name = if settings.dot_ts {
                format!(
                    "{}-{}{}",
                    gst::get_timestamp() - *START_TIME,
                    settings.dot_prefix.as_ref().map_or("", |s| s.as_str()),
                    pipeline.name()
                )
            } else {
                format!(
                    "{}{}",
                    settings.dot_prefix.as_ref().map_or("", |s| s.as_str()),
                    pipeline.name()
                )
            };

            let dot_path = format!("{}/{}.dot", dot_dir, dump_name);
            gst::debug!(CAT, imp = self, "Writing {}", dot_path);
            match std::fs::File::create(&dot_path) {
                Ok(mut f) => {
                    let data = pipeline.debug_to_dot_data(gst::DebugGraphDetails::all());
                    if let Err(e) = f.write_all(data.as_bytes()) {
                        gst::warning!(CAT, imp = self, "Failed to write {}: {}", dot_path, e);
                    }
                }
                Err(e) => {
                    gst::warning!(CAT, imp = self, "Failed to create {}: {}", dot_path, e);
                }
            }
        }
    }

    #[cfg(unix)]
    fn setup_signal(&self) -> anyhow::Result<()> {
        use signal_hook::consts::signal::*;
        use signal_hook::iterator::Signals;

        let mut signals = Signals::new([SIGUSR1])?;
        let signal_handle = signals.handle();

        let this_weak = self.obj().downgrade();
        let thread_handle = std::thread::spawn(move || {
            for signal in &mut signals {
                match signal {
                    SIGUSR1 => {
                        if let Some(this) = this_weak.upgrade() {
                            this.snapshot();
                        } else {
                            break;
                        };
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
