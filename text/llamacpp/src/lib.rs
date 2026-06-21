// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(unused_doc_comments)]

/**
 * SECTION:plugin-llamacpp
 *
 * Plugin containing [llama.cpp](https://github.com/ggml-org/llama.cpp)-based elements.
 *
 * Since: plugins-rs-0.16.0
 */
use std::sync::LazyLock;

mod texttransform;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "llamacpp",
        gst::DebugColorFlags::empty(),
        Some("llama.cpp plugin"),
    )
});

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    if let Err(err) = &*BACKEND {
        gst::error!(CAT, "Failed to initialize llama.cpp: {err}");
    }
    texttransform::register(plugin)?;
    Ok(())
}

fn initialize_logging(envvar_name: &str) -> Result<(), anyhow::Error> {
    use tracing_subscriber::layer::SubscriberExt as _;

    tracing_log::LogTracer::init()?;
    let env_filter = tracing_subscriber::EnvFilter::try_from_env(envvar_name)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_thread_ids(true)
        .with_target(true)
        .with_span_events(
            tracing_subscriber::fmt::format::FmtSpan::NEW
                | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
        );
    let subscriber = tracing_subscriber::Registry::default()
        .with(env_filter)
        .with(fmt_layer);
    tracing::subscriber::set_global_default(subscriber)?;

    Ok(())
}

static BACKEND: LazyLock<llama_cpp_2::Result<llama_cpp_2::llama_backend::LlamaBackend>> =
    LazyLock::new(|| {
        let tracing = if initialize_logging("GST_LLAMA_CPP_LOG").is_ok() {
            llama_cpp_2::send_logs_to_tracing(
                llama_cpp_2::LogOptions::default().with_logs_enabled(true),
            );
            true
        } else {
            false
        };

        let mut b = llama_cpp_2::llama_backend::LlamaBackend::init()?;
        if !tracing {
            b.void_logs();
        }

        Ok(b)
    });

gst::plugin_define!(
    llamacpp,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
