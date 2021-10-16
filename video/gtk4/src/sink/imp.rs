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

use super::SinkEvent;
use crate::sink::frame::Frame;
use crate::sink::paintable::SinkPaintable;

use glib::prelude::*;
use glib::Sender;

use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_trace};
use gst_base::subclass::prelude::*;
use gst_video::subclass::prelude::*;

use gtk::glib;

use once_cell::sync::Lazy;
use std::sync::Mutex;

use fragile::Fragile;

pub(super) static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "gtk4paintablesink",
        gst::DebugColorFlags::empty(),
        Some("GTK4 Paintable sink"),
    )
});

#[derive(Default)]
pub struct PaintableSink {
    pub(super) paintable: Mutex<Option<Fragile<SinkPaintable>>>,
    info: Mutex<Option<gst_video::VideoInfo>>,
    pub(super) sender: Mutex<Option<Sender<SinkEvent>>>,
    pub(super) pending_frame: Mutex<Option<Frame>>,
}

impl Drop for PaintableSink {
    fn drop(&mut self) {
        let mut paintable = self.paintable.lock().unwrap();

        // Drop the paintable from the main thread
        if let Some(paintable) = paintable.take() {
            let context = glib::MainContext::default();

            context.invoke(move || {
                drop(paintable);
            });
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for PaintableSink {
    const NAME: &'static str = "Gtk4PaintableSink";
    type Type = super::PaintableSink;
    type ParentType = gst_video::VideoSink;
}

impl ObjectImpl for PaintableSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpec::new_object(
                "paintable",
                "Paintable",
                "The Paintable the sink renders to",
                gtk::gdk::Paintable::static_type(),
                glib::ParamFlags::READABLE,
            )]
        });

        PROPERTIES.as_ref()
    }

    fn property(&self, obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "paintable" => {
                let mut paintable = self.paintable.lock().unwrap();
                if paintable.is_none() {
                    obj.initialize_paintable(&mut paintable);
                }

                let paintable = match &*paintable {
                    Some(ref paintable) => paintable,
                    None => {
                        gst_error!(CAT, obj: obj, "Failed to create paintable");
                        return None::<&gtk::gdk::Paintable>.to_value();
                    }
                };

                // Getter must be called from the main thread
                match paintable.try_get() {
                    Ok(paintable) => paintable.to_value(),
                    Err(_) => {
                        gst_error!(
                            CAT,
                            obj: obj,
                            "Can't retrieve Paintable from non-main thread"
                        );
                        None::<&gtk::gdk::Paintable>.to_value()
                    }
                }
            }
            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for PaintableSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "GTK 4 Paintable Sink",
                "Sink/Video",
                "A GTK 4 Paintable sink",
                "Bilal Elmoussaoui <bil.elmoussaoui@gmail.com>, Jordan Petridis <jordan@centricular.com>, Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            // Those are the supported formats by a gdk::Texture
            let caps = gst_video::video_make_raw_caps(&[
                gst_video::VideoFormat::Bgra,
                gst_video::VideoFormat::Argb,
                gst_video::VideoFormat::Rgba,
                gst_video::VideoFormat::Abgr,
                gst_video::VideoFormat::Rgb,
                gst_video::VideoFormat::Bgr,
            ])
            .any_features()
            .build();

            vec![gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap()]
        });

        PAD_TEMPLATES.as_ref()
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                let mut paintable = self.paintable.lock().unwrap();
                if paintable.is_none() {
                    element.initialize_paintable(&mut paintable);
                }

                if paintable.is_none() {
                    gst_error!(CAT, obj: element, "Failed to create paintable");
                    return Err(gst::StateChangeError);
                }
            }
            _ => (),
        }

        let res = self.parent_change_state(element, transition);

        match transition {
            gst::StateChange::PausedToReady => {
                let _ = self.info.lock().unwrap().take();
                let _ = self.pending_frame.lock().unwrap().take();
            }
            _ => (),
        }

        res
    }
}

impl BaseSinkImpl for PaintableSink {
    fn set_caps(&self, element: &Self::Type, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst_debug!(CAT, obj: element, "Setting caps {:?}", caps);

        let video_info = gst_video::VideoInfo::from_caps(caps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid caps"))?;

        self.info.lock().unwrap().replace(video_info);

        Ok(())
    }

    fn propose_allocation(
        &self,
        element: &Self::Type,
        mut query: gst::query::Allocation<&mut gst::QueryRef>,
    ) -> Result<(), gst::ErrorMessage> {
        query.add_allocation_meta::<gst_video::VideoMeta>(None);

        self.parent_propose_allocation(element, query)
    }
}

impl VideoSinkImpl for PaintableSink {
    fn show_frame(
        &self,
        element: &Self::Type,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_trace!(CAT, obj: element, "Rendering buffer {:?}", buffer);

        let info = self.info.lock().unwrap();
        let info = info.as_ref().ok_or_else(|| {
            gst_error!(CAT, obj: element, "Received no caps yet");
            gst::FlowError::NotNegotiated
        })?;

        let frame = Frame::new(buffer, info);
        self.pending_frame.lock().unwrap().replace(frame);

        let sender = self.sender.lock().unwrap();
        let sender = sender.as_ref().ok_or_else(|| {
            gst_error!(CAT, obj: element, "Have no main thread sender");
            gst::FlowError::Error
        })?;

        sender.send(SinkEvent::FrameChanged).map_err(|_| {
            gst_error!(CAT, obj: element, "Have main thread receiver shut down");
            gst::FlowError::Error
        })?;

        Ok(gst::FlowSuccess::Ok)
    }
}
