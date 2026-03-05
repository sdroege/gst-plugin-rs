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

use crate::flate::FlateMethod;

const DEFAULT_COMPRESSION_LEVEL: u32 = 6;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "flatecompress",
        gst::DebugColorFlags::empty(),
        Some("Flate Compressor Element"),
    )
});

#[derive(Default)]
struct State {
    // Set by set_caps once negotiation completes and reflects the actual method.
    active_method: Option<FlateMethod>,
}

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
pub struct FlateCompress {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl FlateCompress {
    pub(crate) fn set_active_method(&self, method: FlateMethod) {
        self.state.lock().unwrap().active_method = Some(method);
    }

    fn compress_data(
        &self,
        data: &[u8],
        level: u32,
        out: &mut [u8],
        zlib_header: bool,
    ) -> Result<usize, std::io::Error> {
        let mut compressor =
            flate2::Compress::new(flate2::Compression::new(level.min(9)), zlib_header);
        let status = compressor
            .compress(data, out, flate2::FlushCompress::Finish)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        match status {
            flate2::Status::StreamEnd => Ok(compressor.total_out() as usize),
            flate2::Status::Ok | flate2::Status::BufError => Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "output buffer too small for compressed data",
            )),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for FlateCompress {
    const NAME: &'static str = "GstFlateCompress";
    type Type = super::FlateCompress;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for FlateCompress {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                /**
                 * GstFlateCompress:level:
                 *
                 * Compression level. 0 is fastest with least compression, 9 is
                 * slowest with best compression.
                 */
                glib::ParamSpecUInt::builder("level")
                    .nick("Compression Level")
                    .blurb("Compression level (0=fast, 9=best)")
                    .minimum(0)
                    .maximum(9)
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

impl GstObjectImpl for FlateCompress {}

impl ElementImpl for FlateCompress {}

impl BaseTransformImpl for FlateCompress {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn set_caps(&self, _incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let media_type = outcaps.structure(0).map(|s| s.name().as_str()).unwrap();

        let Some(method) = FlateMethod::ALL
            .into_iter()
            .find(|m| m.media_type() == media_type)
        else {
            return Err(gst::loggable_error!(
                CAT,
                "Unsupported compressed media type in caps: {media_type}"
            ));
        };

        self.state.lock().unwrap().active_method = Some(method);
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
        // Conservative worst-case upper bound for zlib/deflate output.
        Some(size + (size / 10) + 1024)
    }

    fn transform(
        &self,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let level = self.settings.lock().unwrap().level;

        let zlib_header = self
            .state
            .lock()
            .unwrap()
            .active_method
            .map(|m| m.zlib_header())
            .ok_or_else(|| {
                gst::error!(CAT, imp = self, "Compression method not negotiated");
                gst::FlowError::NotNegotiated
            })?;

        let inmap = inbuf.map_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "Failed to map input buffer readable");
            gst::FlowError::Error
        })?;

        let mut outmap = outbuf.map_writable().map_err(|_| {
            gst::error!(CAT, imp = self, "Failed to map output buffer writable");
            gst::FlowError::Error
        })?;

        let written = self
            .compress_data(&inmap, level, &mut outmap, zlib_header)
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

pub trait FlateCompressImpl: BaseTransformImpl {}

unsafe impl<T: FlateCompressImpl> IsSubclassable<T> for super::FlateCompress {}

#[derive(Default)]
pub struct ZlibCompress {}

#[glib::object_subclass]
impl ObjectSubclass for ZlibCompress {
    const NAME: &'static str = "GstZlibCompress";
    type Type = super::ZlibCompress;
    type ParentType = super::FlateCompress;
}

impl FlateCompressImpl for ZlibCompress {}

impl ObjectImpl for ZlibCompress {
    fn constructed(&self) {
        self.parent_constructed();
        self.obj()
            .upcast_ref::<super::FlateCompress>()
            .imp()
            .set_active_method(FlateMethod::Zlib);
    }
}
impl GstObjectImpl for ZlibCompress {}

impl BaseTransformImpl for ZlibCompress {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
}

impl ElementImpl for ZlibCompress {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Zlib Compressor",
                "Encoder/Generic",
                "Compress data using zlib (with checksum)",
                "Daniel Morin <daniel.morin@collabora.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("application/x-zlib-compressed").build(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }
}

#[derive(Default)]
pub struct DeflateCompress {}

#[glib::object_subclass]
impl ObjectSubclass for DeflateCompress {
    const NAME: &'static str = "GstDeflateCompress";
    type Type = super::DeflateCompress;
    type ParentType = super::FlateCompress;
}

impl FlateCompressImpl for DeflateCompress {}

impl ObjectImpl for DeflateCompress {
    fn constructed(&self) {
        self.parent_constructed();
        self.obj()
            .upcast_ref::<super::FlateCompress>()
            .imp()
            .set_active_method(FlateMethod::Deflate);
    }
}
impl GstObjectImpl for DeflateCompress {}

impl BaseTransformImpl for DeflateCompress {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
}

impl ElementImpl for DeflateCompress {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Deflate Compressor",
                "Encoder/Generic",
                "Compress data using deflate (no checksum)",
                "Daniel Morin <daniel.morin@collabora.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("application/x-deflate-compressed").build(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_data_output_too_small_zlib() {
        let element = FlateCompress::default();
        let data = vec![0u8; 1024];
        let mut out = vec![0u8; 1];
        let result = element.compress_data(&data, 6, &mut out, true);
        assert!(
            result.is_err(),
            "compress_data must fail when output buffer is too small (zlib)"
        );
    }

    #[test]
    fn test_compress_data_output_too_small_deflate() {
        let element = FlateCompress::default();
        let data = vec![0u8; 1024];
        let mut out = vec![0u8; 1];
        let result = element.compress_data(&data, 6, &mut out, false);
        assert!(
            result.is_err(),
            "compress_data must fail when output buffer is too small (deflate)"
        );
    }
}
