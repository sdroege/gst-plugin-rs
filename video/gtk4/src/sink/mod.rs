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

use gtk::glib;
use gtk::glib::prelude::*;
use gtk::subclass::prelude::*;

use gst::{gst_debug, gst_trace};

use fragile::Fragile;

use std::sync::{mpsc, MutexGuard};

mod frame;
mod imp;
mod paintable;

use frame::Frame;
use paintable::SinkPaintable;

enum SinkEvent {
    FrameChanged,
}

glib::wrapper! {
    pub struct PaintableSink(ObjectSubclass<imp::PaintableSink>)
        @extends gst_video::VideoSink, gst_base::BaseSink, gst::Element, gst::Object;
}

// GStreamer elements need to be thread-safe. For the private implementation this is automatically
// enforced but for the public wrapper type we need to specify this manually.
unsafe impl Send for PaintableSink {}
unsafe impl Sync for PaintableSink {}

impl PaintableSink {
    pub fn new(name: Option<&str>) -> Self {
        glib::Object::new(&[("name", &name)]).expect("Failed to create a GTK4Sink")
    }

    fn pending_frame(&self) -> Option<Frame> {
        let imp = self.imp();
        imp.pending_frame.lock().unwrap().take()
    }

    fn initialize_paintable(
        &self,
        paintable_storage: &mut MutexGuard<Option<Fragile<SinkPaintable>>>,
    ) {
        gst_debug!(imp::CAT, obj: self, "Initializing paintable");

        let context = glib::MainContext::default();

        // The channel for the SinkEvents
        let (sender, receiver) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);

        // This is an one time channel we send into the closure, so we can block until the paintable has been
        // created.
        let (send, recv) = mpsc::channel();
        context.invoke(glib::clone!(
            @weak self as s =>
            move || {
                let paintable = Fragile::new(SinkPaintable::new());
                send.send(paintable).expect("Somehow we dropped the receiver");
            }
        ));

        let paintable = recv.recv().expect("Somehow we dropped the sender");

        receiver.attach(
            None,
            glib::clone!(
                @weak self as sink => @default-return glib::Continue(false),
                move |action| sink.do_action(action)
            ),
        );

        **paintable_storage = Some(paintable);

        let imp = self.imp();
        *imp.sender.lock().unwrap() = Some(sender);
    }

    fn do_action(&self, action: SinkEvent) -> glib::Continue {
        let imp = self.imp();
        let paintable = imp.paintable.lock().unwrap().clone();
        let paintable = match paintable {
            Some(paintable) => paintable,
            None => return glib::Continue(false),
        };

        match action {
            SinkEvent::FrameChanged => {
                gst_trace!(imp::CAT, obj: self, "Frame changed");
                paintable.get().handle_frame_changed(self.pending_frame())
            }
        }

        glib::Continue(true)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "gtk4paintablesink",
        gst::Rank::None,
        PaintableSink::static_type(),
    )
}
