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

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene};

use gst::gst_trace;

use crate::sink::frame::Paintable;

use std::cell::RefCell;

use once_cell::sync::Lazy;

pub(super) static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "gtk4paintablesink-paintable",
        gst::DebugColorFlags::empty(),
        Some("GTK4 Paintable Sink Paintable"),
    )
});

#[derive(Default)]
pub struct SinkPaintable {
    pub paintable: RefCell<Option<Paintable>>,
}

#[glib::object_subclass]
impl ObjectSubclass for SinkPaintable {
    const NAME: &'static str = "Gtk4PaintableSinkPaintable";
    type Type = super::SinkPaintable;
    type ParentType = glib::Object;
    type Interfaces = (gdk::Paintable,);
}

impl ObjectImpl for SinkPaintable {}

impl PaintableImpl for SinkPaintable {
    fn intrinsic_height(&self, _paintable: &Self::Type) -> i32 {
        if let Some(Paintable { ref paintable, .. }) = *self.paintable.borrow() {
            paintable.intrinsic_height()
        } else {
            0
        }
    }

    fn intrinsic_width(&self, _paintable: &Self::Type) -> i32 {
        if let Some(Paintable {
            ref paintable,
            pixel_aspect_ratio,
        }) = *self.paintable.borrow()
        {
            f64::round(paintable.intrinsic_width() as f64 * pixel_aspect_ratio) as i32
        } else {
            0
        }
    }

    fn intrinsic_aspect_ratio(&self, _paintable: &Self::Type) -> f64 {
        if let Some(Paintable {
            ref paintable,
            pixel_aspect_ratio,
        }) = *self.paintable.borrow()
        {
            paintable.intrinsic_aspect_ratio() * pixel_aspect_ratio
        } else {
            0.0
        }
    }

    fn current_image(&self, _paintable: &Self::Type) -> gdk::Paintable {
        if let Some(Paintable { ref paintable, .. }) = *self.paintable.borrow() {
            paintable.clone()
        } else {
            gdk::Paintable::new_empty(0, 0).expect("Couldn't create empty paintable")
        }
    }

    fn snapshot(&self, paintable: &Self::Type, snapshot: &gdk::Snapshot, width: f64, height: f64) {
        if let Some(Paintable { ref paintable, .. }) = *self.paintable.borrow() {
            gst_trace!(CAT, obj: paintable, "Snapshotting frame");
            paintable.snapshot(snapshot, width, height);
        } else {
            gst_trace!(CAT, obj: paintable, "Snapshotting black frame");
            let snapshot = snapshot.downcast_ref::<gtk::Snapshot>().unwrap();
            snapshot.append_color(
                &gdk::RGBA::BLACK,
                &graphene::Rect::new(0f32, 0f32, width as f32, height as f32),
            );
        }
    }
}
