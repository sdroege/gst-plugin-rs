// Copyright (C) 2026 Collabora Ltd
//  @author: Olivier Crête <olivier.crete@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

pub mod imp;
mod meanface;

glib::wrapper! {
    pub struct PipnetTensorDec(ObjectSubclass<imp::PipnetTensorDec>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "pipnettensordec",
        gst::Rank::PRIMARY,
        PipnetTensorDec::static_type(),
    )
}
