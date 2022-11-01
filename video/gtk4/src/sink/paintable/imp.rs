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

use crate::sink::frame::{Frame, Texture};

use std::cell::RefCell;
use std::collections::HashMap;

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
    paintables: RefCell<Vec<Texture>>,
    cached_textures: RefCell<HashMap<usize, gdk::Texture>>,
}

#[glib::object_subclass]
impl ObjectSubclass for SinkPaintable {
    const NAME: &'static str = "GstGtk4PaintableSinkPaintable";
    type Type = super::SinkPaintable;
    type ParentType = glib::Object;
    type Interfaces = (gdk::Paintable,);
}

impl ObjectImpl for SinkPaintable {}

impl PaintableImpl for SinkPaintable {
    fn intrinsic_height(&self) -> i32 {
        if let Some(paintable) = self.paintables.borrow().first() {
            f32::round(paintable.height) as i32
        } else {
            0
        }
    }

    fn intrinsic_width(&self) -> i32 {
        if let Some(paintable) = self.paintables.borrow().first() {
            f32::round(paintable.width) as i32
        } else {
            0
        }
    }

    fn intrinsic_aspect_ratio(&self) -> f64 {
        if let Some(paintable) = self.paintables.borrow().first() {
            paintable.width as f64 / paintable.height as f64
        } else {
            0.0
        }
    }

    fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
        let snapshot = snapshot.downcast_ref::<gtk::Snapshot>().unwrap();

        let paintables = self.paintables.borrow();

        if !paintables.is_empty() {
            gst::trace!(CAT, imp: self, "Snapshotting frame");

            let (frame_width, frame_height) =
                paintables.first().map(|p| (p.width, p.height)).unwrap();

            let mut scale_x = width / frame_width as f64;
            let mut scale_y = height / frame_height as f64;
            let mut trans_x = 0.0;
            let mut trans_y = 0.0;

            // TODO: Property for keeping aspect ratio or not
            if (scale_x - scale_y).abs() > f64::EPSILON {
                if scale_x > scale_y {
                    trans_x =
                        ((frame_width as f64 * scale_x) - (frame_width as f64 * scale_y)) / 2.0;
                    scale_x = scale_y;
                } else {
                    trans_y =
                        ((frame_height as f64 * scale_y) - (frame_height as f64 * scale_x)) / 2.0;
                    scale_y = scale_x;
                }
            }

            if trans_x != 0.0 || trans_y != 0.0 {
                snapshot.append_color(
                    &gdk::RGBA::BLACK,
                    &graphene::Rect::new(0f32, 0f32, width as f32, height as f32),
                );
            }

            snapshot.translate(&graphene::Point::new(trans_x as f32, trans_y as f32));
            snapshot.scale(scale_x as f32, scale_y as f32);

            for Texture {
                texture,
                x,
                y,
                width: paintable_width,
                height: paintable_height,
                global_alpha,
            } in &*paintables
            {
                snapshot.push_opacity(*global_alpha as f64);
                snapshot.append_texture(
                    texture,
                    &graphene::Rect::new(*x, *y, *paintable_width, *paintable_height),
                );
                snapshot.pop();
            }
        } else {
            gst::trace!(CAT, imp: self, "Snapshotting black frame");
            snapshot.append_color(
                &gdk::RGBA::BLACK,
                &graphene::Rect::new(0f32, 0f32, width as f32, height as f32),
            );
        }
    }
}

impl SinkPaintable {
    pub(super) fn handle_frame_changed(&self, frame: Option<Frame>) {
        if let Some(frame) = frame {
            gst::trace!(CAT, imp: self, "Received new frame");

            let new_paintables = frame.into_textures(&mut self.cached_textures.borrow_mut());
            let new_size = new_paintables
                .first()
                .map(|p| (f32::round(p.width) as u32, f32::round(p.height) as u32))
                .unwrap();

            let old_paintables = self.paintables.replace(new_paintables);
            let old_size = old_paintables
                .first()
                .map(|p| (f32::round(p.width) as u32, f32::round(p.height) as u32));

            if Some(new_size) != old_size {
                gst::debug!(
                    CAT,
                    imp: self,
                    "Size changed from {:?} to {:?}",
                    old_size,
                    new_size,
                );
                self.obj().invalidate_size();
            }

            self.obj().invalidate_contents();
        }
    }
}
