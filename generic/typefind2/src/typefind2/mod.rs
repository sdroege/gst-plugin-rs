// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct TypeFind2(ObjectSubclass<imp::TypeFind2>)
        @extends gst::Element, gst::Object;
}

impl TypeFind2 {
    // rustdoc-stripper-ignore-next
    /// Emitted whenever a media type (and its probability) has been found.
    pub fn connect_have_type<F: Fn(&TypeFind2, u32, &gst::Caps) + Send + Sync + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_closure(
            "have-type",
            false,
            glib::closure!(|instance: &TypeFind2, probability: u32, caps: &gst::Caps| {
                f(instance, probability, caps)
            }),
        )
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "typefind2",
        gst::Rank::NONE,
        TypeFind2::static_type(),
    )
}
