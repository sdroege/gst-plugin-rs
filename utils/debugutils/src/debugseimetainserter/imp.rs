// Copyright (C) 2025 Thibault Saunier <tsaunier@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::glib::Properties;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use gst_video::video_meta::VideoSeiUserDataUnregisteredMeta;
use std::sync::{LazyLock, Mutex};
use uuid::Uuid;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "debugseimetainserter",
        gst::DebugColorFlags::empty(),
        Some("Debug SEI Meta Inserter Element"),
    )
});

// Default UUID: deb95e10-deb9-5e10-deb9-5e10deb95e10
const DEFAULT_UUID: Uuid = Uuid::from_bytes([
    0xde, 0xb9, 0x5e, 0x10, 0xde, 0xb9, 0x5e, 0x10, 0xde, 0xb9, 0x5e, 0x10, 0xde, 0xb9, 0x5e, 0x10,
]);

#[derive(Debug)]
struct Settings {
    uuid: Uuid,
    data: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            uuid: DEFAULT_UUID,
            data: None,
        }
    }
}

#[derive(Properties, Default)]
#[properties(wrapper_type = super::DebugSeiMetaInserter)]
pub struct DebugSeiMetaInserter {
    #[property(name = "uuid", nick = "UUID",
               blurb = "16-byte UUID as hex string (e.g., 12345678-1234-1234-1234-123456789abc)",
               get = Self::uuid, set = Self::set_uuid,
               type = String)]
    #[property(name = "data", nick = "Data",
               blurb = "Payload data to insert as SEI user data",
               get, set, type = Option<String>, member = data)]
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for DebugSeiMetaInserter {
    const NAME: &'static str = "GstDebugSeiMetaInserter";
    type Type = super::DebugSeiMetaInserter;
    type ParentType = gst_base::BaseTransform;
}

#[glib::derived_properties]
impl ObjectImpl for DebugSeiMetaInserter {}

impl DebugSeiMetaInserter {
    fn uuid(&self) -> String {
        let settings = self.settings.lock().unwrap();
        settings.uuid.to_string()
    }

    fn set_uuid(&self, uuid_str: String) {
        let mut settings = self.settings.lock().unwrap();
        match Uuid::parse_str(&uuid_str) {
            Ok(uuid) => {
                gst::info!(CAT, imp = self, "Setting UUID to {}", uuid);
                settings.uuid = uuid;
            }
            Err(e) => {
                gst::error!(CAT, imp = self, "Invalid UUID '{}': {}", uuid_str, e);
            }
        }
    }
}

impl GstObjectImpl for DebugSeiMetaInserter {}

impl ElementImpl for DebugSeiMetaInserter {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Debug SEI Meta Inserter",
                "Debug/Video",
                "Adds GstVideoSEIUserDataUnregisteredMeta to video buffers for testing",
                "Thibault Saunier <tsaunier@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder_full()
                .structure(gst::Structure::builder("video/x-raw").build())
                .structure(gst::Structure::builder("video/x-h264").build())
                .structure(gst::Structure::builder("video/x-h265").build())
                .structure(gst::Structure::builder("video/x-h266").build())
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for DebugSeiMetaInserter {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap();

        if let Some(data) = &settings.data
            && !data.is_empty()
        {
            VideoSeiUserDataUnregisteredMeta::add(
                buffer,
                settings.uuid.as_bytes(),
                data.as_bytes(),
            );
            gst::debug!(
                CAT,
                imp = self,
                "Added SEI meta with UUID {} and {} bytes of data",
                settings.uuid,
                data.len()
            );
        }

        Ok(gst::FlowSuccess::Ok)
    }
}
