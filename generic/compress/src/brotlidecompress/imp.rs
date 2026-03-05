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

const MEDIA_TYPE: &str = "application/x-brotli-compressed";

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "brotlidecompress",
        gst::DebugColorFlags::empty(),
        Some("Brotli Decompressor Element"),
    )
});

struct State {
    adapter: gst_base::UniqueAdapter,
}

impl Default for State {
    fn default() -> Self {
        State {
            adapter: gst_base::UniqueAdapter::new(),
        }
    }
}

#[derive(Default)]
pub struct BrotliDecompress {
    state: Mutex<State>,
}

impl BrotliDecompress {
    // Uses BrotliDecompressStream directly to get exact input consumption.
    // The simple BrotliDecompress API reads into a 4096-byte internal buffer,
    // causing Cursor::position() to over-count when multiple compressed
    // streams are concatenated in a single data slice.
    fn try_decompress(&self, data: &[u8]) -> Result<Option<(Vec<u8>, usize)>, gst::FlowError> {
        let mut state = brotli::BrotliState::new(
            brotli::HeapAlloc::<u8>::new(0),
            brotli::HeapAlloc::<u32>::new(0),
            brotli::HeapAlloc::<brotli::HuffmanCode>::new(brotli::HuffmanCode::default()),
        );

        let mut available_in = data.len();
        let mut input_offset: usize = 0;
        let mut output_buf = vec![0u8; 65536];
        let mut available_out = output_buf.len();
        let mut output_offset: usize = 0;
        let mut total_out: usize = 0;
        let mut result_data = Vec::with_capacity(data.len() * 4);

        loop {
            let result = brotli::BrotliDecompressStream(
                &mut available_in,
                &mut input_offset,
                data,
                &mut available_out,
                &mut output_offset,
                &mut output_buf,
                &mut total_out,
                &mut state,
            );

            if output_offset > 0 {
                result_data.extend_from_slice(&output_buf[..output_offset]);
                output_offset = 0;
                available_out = output_buf.len();
            }

            match result {
                brotli::BrotliResult::ResultSuccess => {
                    return Ok(Some((result_data, input_offset)));
                }
                brotli::BrotliResult::NeedsMoreOutput => continue,
                brotli::BrotliResult::NeedsMoreInput => return Ok(None),
                brotli::BrotliResult::ResultFailure => return Err(gst::FlowError::Error),
            }
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for BrotliDecompress {
    const NAME: &'static str = "GstBrotliDecompress";
    type Type = super::BrotliDecompress;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for BrotliDecompress {}

impl GstObjectImpl for BrotliDecompress {}

impl ElementImpl for BrotliDecompress {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Brotli Decompressor",
                "Decoder/Generic",
                "Decompress data using Brotli",
                "Daniel Morin <daniel.morin@collabora.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst::Caps::builder(MEDIA_TYPE).build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::new_any();
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

impl BaseTransformImpl for BrotliDecompress {
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
        debug_assert_eq!(
            media_type, MEDIA_TYPE,
            "set_caps called with unexpected media type: {media_type}"
        );
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        self.state.lock().unwrap().adapter.clear();
        Ok(())
    }

    // Push the incoming buffer into the adapter.
    // On discontinuity, clear any accumulated bytes first to avoid
    // attempting to decompress across a stream boundary.
    //
    // We check the buffer's DISCONT flag rather than the `is_discont`
    // parameter from BaseTransform. BaseTransform keeps its internal
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

        let data_map = state.adapter.map(available).map_err(|_| {
            gst::error!(CAT, imp = self, "Failed to map adapter data");
            gst::FlowError::Error
        })?;

        match self.try_decompress(&data_map).inspect_err(|_| {
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

#[cfg(all(test, feature = "brotli-corruption-tests"))]
mod tests {
    use super::*;

    // Identified by exploration: corrupted at offset 66 produces NeedsMoreInput (truncated),
    // corrupted at offset 68 produces garbled output. In both cases brotli can't
    // detect corruption.
    fn test_data() -> Vec<u8> {
        (0u8..=255).chain(0u8..=255).collect()
    }

    #[test]
    fn test_structural_corruption_returns_error() {
        gst::init().unwrap();
        // Validates that brotli can detect corruption in its structural data
        let element = BrotliDecompress::default();

        let data = test_data();
        let mut compressed = Vec::new();
        brotli::BrotliCompress(
            &mut data.as_slice(),
            &mut compressed,
            &brotli::enc::BrotliEncoderParams::default(),
        )
        .unwrap();

        // Flip the first 4 bytes to corrupt brotli structural data
        for b in compressed[..4].iter_mut() {
            *b ^= 0xff;
        }

        let result = element.try_decompress(&compressed);
        assert!(
            result.is_err(),
            "structural header corruption must return a flow error, got: {result:?}"
        );
    }

    #[test]
    fn test_payload_corruption_unnoticed() {
        gst::init().unwrap();
        // Demonstrates that non-structural payload corruption is never detected
        // by brotli.
        // Two outcomes are possible, both without a FlowError:
        //   - Garbled output: brotli decodes successfully but produces wrong data
        //   - Truncated stream: brotli treats the stream as incomplete (NeedsMoreInput)
        // These specific offsets were identified by exploration and are hardcoded here.
        let element = BrotliDecompress::default();

        let data = test_data();
        let mut compressed = Vec::new();
        brotli::BrotliCompress(
            &mut data.as_slice(),
            &mut compressed,
            &brotli::enc::BrotliEncoderParams::default(),
        )
        .unwrap();

        // Corrupted result in garbled output
        {
            let mut corrupted = compressed.clone();
            for b in corrupted[68..72].iter_mut() {
                *b ^= 0xff;
            }

            let result = element.try_decompress(&corrupted);
            assert!(result.is_ok(), "payload corruption goes undetected");
            let decompressed = result.unwrap().map(|(d, _)| d);
            assert!(
                matches!(decompressed.as_deref(), Some(d) if d != data.as_slice()),
                "expected garbled output"
            );
        }

        // Corrupted result in truncated
        {
            let mut corrupted = compressed.clone();
            for b in corrupted[66..70].iter_mut() {
                *b ^= 0xff;
            }

            let result = element.try_decompress(&corrupted);
            assert!(result.is_ok(), "payload corruption goes undetected");
            let decompressed = result.unwrap().map(|(d, _)| d);
            assert!(
                decompressed.as_deref() != Some(data.as_slice()),
                "expected truncated"
            );
        }
    }
}
