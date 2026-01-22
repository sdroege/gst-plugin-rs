// Copyright (C) 2026 Thibault Saunier <tsaunier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * tracer-perfetto:
 *
 * This tracer outputs events in Perfetto native format, which can be
 * visualized in [perfetto](https://ui.perfetto.dev/).
 *
 * ## Parameters
 *
 * ### `backend`
 *
 * The tracing backend to use:
 * - `file` (default): Write to a `.pftrace` file in the current directory
 * - `system`: Connect to the system `traced` daemon for system-wide tracing
 *
 * Example (file output):
 *
 * ```console
 * $ GST_TRACERS='perfetto' gst-launch-1.0 videotestsrc num-buffers=120 ! fakesink
 * ```
 *
 * A `.pftrace` file will be created in the current directory.
 *
 * Example (system tracing):
 *
 * ```console
 * $ GST_TRACERS='perfetto(backend=system)' gst-launch-1.0 videotestsrc num-buffers=120 ! fakesink
 * ```
 *
 * Events will be sent to the system traced daemon.
 */
use gst::glib;
use gst::glib::Properties;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_tracing::tracer::{TracingTracer, TracingTracerImpl};
use std::fs::File;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tracing::error;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_perfetto_sdk_layer::{NativeLayer, SdkLayer};
use tracing_perfetto_sdk_schema as schema;
use tracing_subscriber::prelude::*;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "perfetto-tracer",
        gst::DebugColorFlags::empty(),
        Some("Perfetto tracer"),
    )
});

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "GstPerfettoTracerBackend")]
#[repr(i32)]
pub enum PerfettoBackend {
    #[default]
    #[enum_value(name = "File: Write to a .pftrace file", nick = "file")]
    File = 0,
    #[enum_value(name = "System: Connect to traced daemon", nick = "system")]
    System = 1,
}

enum LayerType {
    Native(NativeLayer<NonBlocking>),
    Sdk(SdkLayer),
}

const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct State {
    layer: Option<LayerType>,
    _guard: Option<WorkerGuard>,
}

#[derive(Default)]
struct Settings {
    backend: PerfettoBackend,
}

#[derive(Properties, Default)]
#[properties(wrapper_type = super::PerfettoTracer)]
pub struct PerfettoTracer {
    state: Mutex<State>,
    #[property(
        name = "backend",
        get,
        set,
        type = PerfettoBackend,
        member = backend,
        blurb = "The tracing backend to use",
        builder(PerfettoBackend::File)
    )]
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for PerfettoTracer {
    const NAME: &'static str = "GstPerfettoTracer";
    type Type = super::PerfettoTracer;
    type ParentType = TracingTracer;
}

#[glib::derived_properties]
impl ObjectImpl for PerfettoTracer {
    fn constructed(&self) {
        self.parent_constructed();

        let backend = self.settings.lock().unwrap().backend;
        gst::info!(CAT, imp = self, "Using backend: {:?}", backend);

        let config = schema::TraceConfig {
            buffers: vec![schema::trace_config::BufferConfig {
                size_kb: Some(65536),
                ..Default::default()
            }],
            data_sources: vec![schema::trace_config::DataSource {
                config: Some(schema::DataSourceConfig {
                    name: Some("rust_tracing".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        match backend {
            PerfettoBackend::System => {
                let layer = SdkLayer::from_config(config, None)
                    .build()
                    .expect("Failed to create SDK layer");

                {
                    let mut state = self.state.lock().unwrap();
                    state.layer = Some(LayerType::Sdk(layer.clone()));
                }

                if let Err(e) = tracing_subscriber::registry().with(layer).try_init() {
                    error!("Failed to initialize tracing subscriber: {e:?}");
                }
            }
            PerfettoBackend::File => {
                let pid = std::process::id();
                let filename = format!("trace-{pid}.pftrace");
                gst::info!(CAT, imp = self, "Writing to file: {filename}");
                let file = File::create(&filename).expect("Failed to create trace file");

                let (non_blocking, guard) = tracing_appender::non_blocking(file);
                let layer = NativeLayer::from_config(config, non_blocking)
                    .build()
                    .expect("Failed to create perfetto layer");

                {
                    let mut state = self.state.lock().unwrap();
                    state.layer = Some(LayerType::Native(layer.clone()));
                    state._guard = Some(guard);
                }

                if let Err(e) = tracing_subscriber::registry().with(layer).try_init() {
                    error!("Failed to initialize tracing subscriber: {e:?}");
                }
            }
        }
    }
}

impl GstObjectImpl for PerfettoTracer {}
impl TracerImpl for PerfettoTracer {
    const USE_STRUCTURE_PARAMS: bool = true;
}
impl TracingTracerImpl for PerfettoTracer {}

impl Drop for PerfettoTracer {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(layer) = state.layer.take() {
                match layer {
                    LayerType::Native(l) => {
                        let _ = l.flush(FLUSH_TIMEOUT, FLUSH_TIMEOUT);
                        let _ = l.stop();
                    }
                    LayerType::Sdk(l) => {
                        let _ = l.flush(FLUSH_TIMEOUT);
                        let _ = l.stop();
                    }
                }
            }
            // Guard drops here, flushing remaining data
        }
    }
}
