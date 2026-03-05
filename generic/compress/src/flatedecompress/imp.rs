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

use crate::compress_caps_helper::decompress_transform_caps;

use std::sync::LazyLock;
use std::sync::Mutex;

use crate::flate::FlateMethod;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "flatedecompress",
        gst::DebugColorFlags::empty(),
        Some("Flate Decompressor Element"),
    )
});

struct State {
    // Accumulates bytes from upstream
    adapter: gst_base::UniqueAdapter,
    // Set by set_caps once negotiation completes; reflects the actual format.
    active_method: Option<FlateMethod>,
}

impl Default for State {
    fn default() -> Self {
        State {
            adapter: gst_base::UniqueAdapter::new(),
            active_method: None,
        }
    }
}

#[derive(Default)]
pub struct FlateDecompress {
    state: Mutex<State>,
}

impl FlateDecompress {
    pub(crate) fn set_active_method(&self, method: FlateMethod) {
        self.state.lock().unwrap().active_method = Some(method);
    }

    fn try_decompress(
        &self,
        data: &[u8],
        zlib_header: bool,
    ) -> Result<Option<(Vec<u8>, usize)>, gst::FlowError> {
        let mut decomp = flate2::Decompress::new(zlib_header);
        let mut output = Vec::with_capacity(data.len() * 4);

        loop {
            let in_pos = decomp.total_in() as usize;
            let out_pos = decomp.total_out() as usize;

            if output.len() == out_pos {
                output.resize(out_pos + 65536, 0);
            }

            let status = decomp
                .decompress(
                    &data[in_pos..],
                    &mut output[out_pos..],
                    flate2::FlushDecompress::None,
                )
                .map_err(|_| gst::FlowError::Error)?;

            let new_in_pos = decomp.total_in() as usize;

            match status {
                flate2::Status::StreamEnd => {
                    output.truncate(decomp.total_out() as usize);
                    return Ok(Some((output, new_in_pos)));
                }
                flate2::Status::Ok | flate2::Status::BufError => {
                    if new_in_pos == in_pos {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for FlateDecompress {
    const NAME: &'static str = "GstFlateDecompress";
    type Type = super::FlateDecompress;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for FlateDecompress {}

impl GstObjectImpl for FlateDecompress {}

impl ElementImpl for FlateDecompress {}

impl BaseTransformImpl for FlateDecompress {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        decompress_transform_caps(self.obj().upcast_ref(), direction, caps, filter, &CAT)
    }

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let media_type = incaps.structure(0).map(|s| s.name().as_str()).unwrap();
        let active_method = FlateMethod::ALL
            .into_iter()
            .find(|m| m.media_type() == media_type)
            .ok_or_else(|| {
                gst::loggable_error!(CAT, "Unexpected media type on sinkpad: {media_type}")
            })?;

        self.state.lock().unwrap().active_method = Some(active_method);
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        state.adapter.clear();
        state.active_method = None;
        Ok(())
    }

    // Push the incoming buffer into the adapter.
    // On discontinuity, clear any accumulated bytes first to avoid
    // attempting to decompress across a stream boundary.
    //
    // We check the buffer's DISCONT flag rather than the `is_discont`
    // parameter from BaseTransform.  BaseTransform keeps its internal
    // `priv->discont` set to TRUE until a buffer is actually pushed downstream,
    // so when our decompressor returns NoOutput while accumulating partial
    // data every subsequent chunk is reported as discont — causing the adapter
    // to be cleared on each chunk and breaking multi-chunk reassembly.
    fn submit_input_buffer(
        &self,
        _is_discont: bool,
        inbuf: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        if inbuf.flags().contains(gst::BufferFlags::DISCONT) {
            gst::debug!(CAT, imp = self, "Discontinuity: clearing adapter");
            state.adapter.clear();
        }
        state.adapter.push(inbuf);
        Ok(gst::FlowSuccess::Ok)
    }

    fn generate_output(
        &self,
    ) -> Result<gst_base::subclass::base_transform::GenerateOutputSuccess, gst::FlowError> {
        use gst_base::subclass::base_transform::GenerateOutputSuccess;

        let mut state = self.state.lock().unwrap();

        let available = state.adapter.available();
        if available == 0 {
            return Ok(GenerateOutputSuccess::NoOutput);
        }

        let zlib_header = state
            .active_method
            .map(|m| m.zlib_header())
            .ok_or_else(|| {
                gst::error!(CAT, imp = self, "Decompression method not determined");
                gst::FlowError::NotNegotiated
            })?;

        let data_map = state.adapter.map(available).map_err(|_| {
            gst::error!(CAT, imp = self, "Failed to map adapter data");
            gst::FlowError::Error
        })?;

        match self
            .try_decompress(&data_map, zlib_header)
            .inspect_err(|_| {
                gst::error!(CAT, imp = self, "Decompression error: corrupted stream");
            })? {
            Some((decompressed, consumed)) => {
                drop(data_map);

                let src_buf = state.adapter.buffer_fast(consumed).ok();
                let (pts, _) = state.adapter.prev_pts_at_offset(0);
                let (dts, _) = state.adapter.prev_dts_at_offset(0);
                let duration = src_buf.as_ref().and_then(|b| b.duration());
                state.adapter.flush(consumed);

                gst::trace!(
                    CAT,
                    imp = self,
                    "Decompressed {len} bytes (consumed {consumed} of {available} available)",
                    len = decompressed.len(),
                );

                let mut outbuf = gst::Buffer::from_mut_slice(decompressed);
                {
                    let outbuf = outbuf.get_mut().unwrap();
                    outbuf.set_pts(pts);
                    outbuf.set_dts(dts);
                    outbuf.set_duration(duration);
                    if let Some(src) = src_buf
                        && let Err(e) = src.copy_into(outbuf, gst::BufferCopyFlags::META, ..)
                    {
                        gst::debug!(CAT, imp = self, "Could not copy buffer metas: {e}");
                    }
                }

                Ok(GenerateOutputSuccess::Buffer(outbuf))
            }
            None => {
                gst::trace!(
                    CAT,
                    imp = self,
                    "Incomplete stream ({available} bytes available), waiting for more data"
                );
                Ok(GenerateOutputSuccess::NoOutput)
            }
        }
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if let gst::EventView::FlushStop(_) = event.view() {
            gst::debug!(CAT, imp = self, "flush-stop event: clearing adapter");
            self.state.lock().unwrap().adapter.clear();
        }
        self.parent_sink_event(event)
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

pub trait FlateDecompressImpl: BaseTransformImpl {}

unsafe impl<T: FlateDecompressImpl> IsSubclassable<T> for super::FlateDecompress {}

#[derive(Default)]
pub struct ZlibDecompress {}

#[glib::object_subclass]
impl ObjectSubclass for ZlibDecompress {
    const NAME: &'static str = "GstZlibDecompress";
    type Type = super::ZlibDecompress;
    type ParentType = super::FlateDecompress;
}

impl FlateDecompressImpl for ZlibDecompress {}

impl ObjectImpl for ZlibDecompress {
    fn constructed(&self) {
        self.parent_constructed();
        self.obj()
            .upcast_ref::<super::FlateDecompress>()
            .imp()
            .set_active_method(FlateMethod::Zlib);
    }
}
impl GstObjectImpl for ZlibDecompress {}

impl BaseTransformImpl for ZlibDecompress {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
}

impl ElementImpl for ZlibDecompress {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Zlib Decompressor",
                "Decoder/Generic",
                "Decompress zlib-compressed data",
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
                &gst::Caps::builder("application/x-zlib-compressed").build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }
}

#[derive(Default)]
pub struct DeflateDecompress {}

#[glib::object_subclass]
impl ObjectSubclass for DeflateDecompress {
    const NAME: &'static str = "GstDeflateDecompress";
    type Type = super::DeflateDecompress;
    type ParentType = super::FlateDecompress;
}

impl FlateDecompressImpl for DeflateDecompress {}

impl ObjectImpl for DeflateDecompress {
    fn constructed(&self) {
        self.parent_constructed();
        self.obj()
            .upcast_ref::<super::FlateDecompress>()
            .imp()
            .set_active_method(FlateMethod::Deflate);
    }
}
impl GstObjectImpl for DeflateDecompress {}

impl BaseTransformImpl for DeflateDecompress {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
}

impl ElementImpl for DeflateDecompress {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Deflate Decompressor",
                "Decoder/Generic",
                "Decompress deflate-compressed data",
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
                &gst::Caps::builder("application/x-deflate-compressed").build(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }
}
