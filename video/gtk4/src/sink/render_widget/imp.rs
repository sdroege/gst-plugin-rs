//
// Copyright (C) 2024 Guillaume Desmottes <guillaume@desmottes.be>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::cell::{Cell, RefCell};

use gtk::{gdk, glib, prelude::*, subclass::prelude::*};
use once_cell::sync::Lazy;

#[derive(Default)]
pub struct RenderWidget {
    element: RefCell<Option<crate::PaintableSink>>,
    window_size: Cell<(u32, u32)>,
}

#[glib::object_subclass]
impl ObjectSubclass for RenderWidget {
    const NAME: &'static str = "GstGtk4ExampleRenderWidget";
    type Type = super::RenderWidget;
    type ParentType = gtk::Widget;

    fn class_init(klass: &mut Self::Class) {
        klass.set_layout_manager_type::<gtk::BinLayout>();
    }
}

impl ObjectImpl for RenderWidget {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecObject::builder::<crate::PaintableSink>("element")
                    .nick("Element")
                    .blurb("The GTK4 Paintable Sink GStreamer element")
                    .construct_only()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "element" => self.element.borrow().to_value(),
            _ => unimplemented!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "element" => {
                *self.element.borrow_mut() = value.get::<Option<crate::PaintableSink>>().unwrap();
            }

            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let element = self.element.borrow();
        let element = element.as_ref().unwrap();
        let paintable = element.property::<gdk::Paintable>("paintable");

        let picture = gtk::Picture::new();
        picture.set_paintable(Some(&paintable));

        #[cfg(feature = "gtk_v4_14")]
        {
            let offload = gtk::GraphicsOffload::new(Some(&picture));
            offload.set_enabled(gtk::GraphicsOffloadEnabled::Enabled);
            #[cfg(feature = "gtk_v4_16")]
            {
                offload.set_black_background(true);
            }
            offload.set_parent(self.obj().as_ref());
        }
        #[cfg(not(feature = "gtk_v4_14"))]
        {
            picture.set_parent(self.obj().as_ref());
        }
    }

    fn dispose(&self) {
        while let Some(child) = self.obj().first_child() {
            child.unparent();
        }
    }
}

impl WidgetImpl for RenderWidget {
    fn snapshot(&self, snapshot: &gtk::Snapshot) {
        let window_width = self.obj().width() as u32;
        let window_height = self.obj().height() as u32;
        let new_size = (window_width, window_height);
        let updated = self.window_size.replace(new_size) != new_size;

        if updated {
            let element = self.element.borrow();
            let element = element.as_ref().unwrap();
            element.set_property("window-width", window_width);
            element.set_property("window-height", window_height);
        }

        self.parent_snapshot(snapshot)
    }
}
