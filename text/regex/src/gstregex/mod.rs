// Copyright (C) 2021 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstRegExMultiBufferMode")]
#[non_exhaustive]
pub enum RegExMultiBufferMode {
    #[enum_value(name = "Disabled", nick = "disabled")]
    Disabled = 0,
    #[enum_value(name = "Compress", nick = "compress")]
    Compress = 1,
}

glib::wrapper! {
    pub struct RegEx(ObjectSubclass<imp::RegEx>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    RegExMultiBufferMode::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    gst::Element::register(Some(plugin), "regex", gst::Rank::NONE, RegEx::static_type())
}
