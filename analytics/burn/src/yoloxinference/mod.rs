// Copyright (C) 2025 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

pub mod imp;
#[allow(unused)]
#[allow(clippy::unused_enumerate_index)]
#[allow(clippy::large_enum_variant)]
mod yolox_burn;

/**
 * GstBurnYoloxModelType:
 *
 * YOLOX model that should be used.
 *
 * Since: plugins-rs-0.15.0
 */
#[derive(Copy, Clone, Default, PartialEq, Eq, glib::Enum)]
#[repr(C)]
#[enum_type(name = "GstBurnYoloxModelType")]
pub enum ModelType {
    Nano,
    #[default]
    Tiny,
    Small,
    Medium,
    Large,
    ExtraLarge,
}

glib::wrapper! {
    pub struct YoloxInference(ObjectSubclass<imp::YoloxInference>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    crate::BackendType::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    ModelType::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());

    gst::Element::register(
        Some(plugin),
        "burn-yoloxinference",
        gst::Rank::NONE,
        YoloxInference::static_type(),
    )
}
