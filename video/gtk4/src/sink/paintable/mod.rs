//
// Copyright (C) 2021 Bilal Elmoussaoui <bil.elmoussaoui@gmail.com>
// Copyright (C) 2021 Jordan Petridis <jordan@centricular.com>
// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use crate::sink::frame::Frame;

use gtk::subclass::prelude::*;
use gtk::{gdk, glib};

mod imp;

glib::wrapper! {
    pub struct SinkPaintable(ObjectSubclass<imp::SinkPaintable>)
        @implements gdk::Paintable;
}

impl SinkPaintable {
    pub fn new() -> Self {
        glib::Object::new(&[]).expect("Failed to create a SinkPaintable")
    }
}

impl Default for SinkPaintable {
    fn default() -> Self {
        Self::new()
    }
}

impl SinkPaintable {
    pub(crate) fn handle_frame_changed(&self, frame: Option<Frame>) {
        let self_ = self.imp();
        self_.handle_frame_changed(self, frame);
    }
}
