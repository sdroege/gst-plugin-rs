// Copyright (C) 2026 Collabora Ltd
//   @author: Daniel Morin <daniel.morin@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;

use crate::compress_caps_helper::compress_transform_caps;

use std::sync::LazyLock;
use std::sync::Mutex;

const DEFAULT_COMPRESSION_LEVEL: u32 = 6;
const MEDIA_TYPE: &str = "application/x-brotli-compressed";

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "brotlicompress",
        gst::DebugColorFlags::empty(),
        Some("Brotli Compressor Element"),
    )
});

struct Settings {
    level: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            level: DEFAULT_COMPRESSION_LEVEL,
        }
    }
}

#[derive(Default)]
pub struct BrotliCompress {
    settings: Mutex<Settings>,
}

impl BrotliCompress {
    fn compress_data(
        &self,
        mut data: &[u8],
        level: u32,
        mut out: &mut [u8],
    ) -> Result<usize, std::io::Error> {
        let params = brotli::enc::BrotliEncoderParams {
            quality: level as i32,
            ..Default::default()
        };
        brotli::BrotliCompress(&mut data, &mut out, &params)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for BrotliCompress {
    const NAME: &'static str = "GstBrotliCompress";
    type Type = super::BrotliCompress;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for BrotliCompress {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                /**
                 * GstBrotliCompress:level:
                 *
                 * Compression level. 0 is fastest with least compression, 11 is
                 * slowest with most compression.
                 */
                glib::ParamSpecUInt::builder("level")
                    .nick("Compression Level")
                    .blurb("Brotli compression level (0=fastest, 11=slowest/best ratio)")
                    .minimum(0)
                    .maximum(11)
                    .default_value(DEFAULT_COMPRESSION_LEVEL)
                    .mutable_playing()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "level" => {
                let mut settings = self.settings.lock().unwrap();
                settings.level = value.get().unwrap();
                gst::debug!(
                    CAT,
                    imp = self,
                    "Compression level changed to {level}",
                    level = settings.level
                );
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "level" => self.settings.lock().unwrap().level.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for BrotliCompress {}

impl ElementImpl for BrotliCompress {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Brotli Compressor",
                "Encoder/Generic",
                "Compress data using Brotli",
                "Daniel Morin <daniel.morin@collabora.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst::Caps::new_any();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder(MEDIA_TYPE).build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for BrotliCompress {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn set_caps(&self, _incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let media_type = outcaps.structure(0).map(|s| s.name().as_str()).unwrap();
        debug_assert_eq!(
            media_type, MEDIA_TYPE,
            "set_caps called with unexpected media type: {media_type}"
        );
        Ok(())
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        compress_transform_caps(self.obj().upcast_ref(), direction, caps, filter, &CAT)
    }

    fn transform_size(
        &self,
        _direction: gst::PadDirection,
        _caps: &gst::Caps,
        size: usize,
        _othercaps: &gst::Caps,
    ) -> Option<usize> {
        // Brotli worst-case: input + (input >> 2) + 512 (conservative upper bound).
        Some(size + (size >> 2) + 512)
    }

    fn transform(
        &self,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let level = self.settings.lock().unwrap().level;

        let inmap = inbuf.map_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "Failed to map input buffer readable");
            gst::FlowError::Error
        })?;

        let mut outmap = outbuf.map_writable().map_err(|_| {
            gst::error!(CAT, imp = self, "Failed to map output buffer writable");
            gst::FlowError::Error
        })?;

        let written = self
            .compress_data(&inmap, level, &mut outmap)
            .map_err(|err| {
                gst::error!(CAT, imp = self, "Compression failed: {err}");
                gst::FlowError::Error
            })?;

        gst::trace!(
            CAT,
            imp = self,
            "Compressed {len} -> {written} bytes ({ratio:.1}%)",
            len = inmap.len(),
            ratio = written as f64 / inmap.len().max(1) as f64 * 100.0,
        );

        drop(outmap);
        outbuf.set_size(written);

        Ok(gst::FlowSuccess::Ok)
    }

    fn src_event(&self, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::Seek(_) => {
                gst::debug!(CAT, imp = self, "Refusing seek event on compressed stream");
                false
            }
            _ => self.parent_src_event(event),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Brotli doesn't expose a max compressed size API, so the transform_size overhead
    // (input + input/4 + 512) is an estimation. This test validates that the budget
    // is sufficient for the worst case: a 1-byte payload where the fixed overhead dominates.
    #[test]
    fn test_compress_data_1_byte() {
        let element = BrotliCompress::default();
        let data = vec![0xABu8; 1];
        // buf_size = 1 + (1 >> 2) + 512 = 513
        let buf_size = 513;
        let mut out = vec![0u8; buf_size];
        let written = element
            .compress_data(&data, DEFAULT_COMPRESSION_LEVEL, &mut out)
            .expect("compress_data must succeed for 1-byte input");
        assert!(written > 0, "compressed output must be non-empty");
        assert!(
            written <= buf_size,
            "compressed output ({written}) must fit in transform_size budget ({buf_size})"
        );
    }
}
