// Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:plugin-burn
 *
 * Plugin containing [burn](https://burn.dev)-based elements.
 *
 * Since: plugins-rs-0.15.0
 */
mod yoloxinference;

/**
 * GstBurnBackendType:
 *
 * Backend that should be used. The `NdArray` backend is always available and is a CPU-backed
 * backend.
 *
 * Available backends depend on build-time options.
 *
 * Since: plugins-rs-0.15.0
 */
#[derive(Copy, Clone, Default, PartialEq, Eq, glib::Enum)]
#[repr(C)]
#[enum_type(name = "GstBurnBackendType")]
pub enum BackendType {
    #[cfg(feature = "ndarray")]
    NdArray,
    #[default]
    Cpu,
    #[cfg(feature = "vulkan")]
    Vulkan,
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    yoloxinference::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    burn,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
