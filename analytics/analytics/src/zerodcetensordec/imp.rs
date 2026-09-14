// Copyright (C) 2026 Denis Shimizu <denis.shimizu@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, subclass::prelude::*};
use gst_video::{prelude::*, subclass::prelude::*};

use byte_slice_cast::*;

use std::sync::{LazyLock, Mutex};

const IMAGE_RGB_HWC_OUT: &glib::GStr = glib::gstr!("image-rgb-hwc-out");

// Samples per pixel, in both the tensor and the RGB frame.
const NUM_CHANNELS: usize = 3;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "zerodcetensordec",
        gst::DebugColorFlags::empty(),
        Some("Zero-DCE tensor decoder element"),
    )
});

// `tensors` caps field for the [1 x] HEIGHT x WIDTH x 3 tensor. With a `(width, height)`, the
// tensor must match it or be unknown (0); without one, any extent is accepted.
fn image_tensorgroups(size: Option<(i32, i32)>) -> gst::Structure {
    let extent = |value: Option<i32>| match value {
        Some(value) => gst::List::new([value, 0]).to_send_value(),
        None => gst::IntRange::new(0, i32::MAX).to_send_value(),
    };

    let image_tensor_caps = |batched: bool| {
        let mut dims = vec![
            extent(size.map(|(_, height)| height)),
            extent(size.map(|(width, _)| width)),
            (NUM_CHANNELS as i32).to_send_value(),
        ];
        if batched {
            dims.insert(0, 1i32.to_send_value());
        }

        gst::Caps::builder("tensor/strided")
            .field("tensor-id", IMAGE_RGB_HWC_OUT)
            .field("dims", gst::Array::from_values(dims))
            .field("dims-order", "row-major")
            .field("type", "float32")
            .build()
    };

    gst::Structure::builder("tensorgroups")
        .field(
            IMAGE_RGB_HWC_OUT,
            gst::UniqueList::new([image_tensor_caps(true), image_tensor_caps(false)]),
        )
        .build()
}

#[derive(Default)]
pub struct ZeroDceTensorDec {
    // Negotiated frame format, set in `set_caps`.
    video_info: Mutex<Option<gst_video::VideoInfo>>,
}

#[glib::object_subclass]
impl ObjectSubclass for ZeroDceTensorDec {
    const NAME: &'static str = "GstZeroDceTensorDec";
    type Type = super::ZeroDceTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for ZeroDceTensorDec {
    fn constructed(&self) {
        self.parent_constructed();

        // BaseTransform accepts caps that merely intersect the sink caps; a tensor decoder must
        // only accept tensors whose caps are a subset of them.
        self.obj()
            .sink_pad()
            .unset_pad_flags(gst::PadFlags::ACCEPT_INTERSECT);
    }
}

impl GstObjectImpl for ZeroDceTensorDec {}

// Element metadata and pad caps.
impl ElementImpl for ZeroDceTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Zero-DCE Tensor Decoder Element",
                "Tensordecoder/Video",
                "Decodes the enhanced image tensor of a Zero-DCE low-light image enhancement model",
                "Denis Shimizu <denis.shimizu@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_video::VideoCapsBuilder::new()
                .format_list([
                    gst_video::VideoFormat::RgbF32le,
                    gst_video::VideoFormat::Rgb,
                ])
                .field("tensors", image_tensorgroups(None))
                .build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst_video::VideoCapsBuilder::new()
                .format_list([
                    gst_video::VideoFormat::RgbF32le,
                    gst_video::VideoFormat::Rgb,
                ])
                .build();
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

// Decodes the tensor onto the frame it comes with, in place.
impl BaseTransformImpl for ZeroDceTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    // In-place transform, so both pads share the caps. `tensors` is kept towards the src pad,
    // as the tensor meta is forwarded, and constrained to the frame size towards the sink pad,
    // so that a resolution mismatch fails at negotiation instead of at runtime.

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let mut other_caps = caps.clone();
        for s in other_caps.make_mut().iter_mut() {
            match direction {
                gst::PadDirection::Sink => (),
                gst::PadDirection::Src => {
                    let size = s.get::<i32>("width").ok().zip(s.get::<i32>("height").ok());
                    s.set("tensors", image_tensorgroups(size));
                }
                _ => return None,
            }
        }

        gst::debug!(
            CAT,
            imp = self,
            "Transformed {caps:?} in direction {direction:?} to {other_caps:?}"
        );

        Some(match filter {
            Some(filter) => filter.intersect_with_mode(&other_caps, gst::CapsIntersectMode::First),
            None => other_caps,
        })
    }

    // Caches the negotiated frame format for `transform_ip`.
    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let info = gst_video::VideoInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid caps {incaps:?}"))?;
        *self.video_info.lock().unwrap() = Some(info);
        Ok(())
    }

    // Drops the cached frame format.
    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.video_info.lock().unwrap() = None;
        gst::log!(CAT, imp = self, "Stopped");
        Ok(())
    }

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Fetch the negotiated frame format.
        let info = self.video_info.lock().unwrap().clone();
        let Some(info) = info else {
            gst::error!(CAT, imp = self, "No video caps negotiated yet");
            return Err(gst::FlowError::NotNegotiated);
        };

        self.log_tensor_metas(buffer);

        // Fetch the tensor data before mapping the frame writable below, since it borrows the
        // meta attached to `buffer`.
        let Some(tensor_buf) = self.find_image_tensor(buffer, &info)? else {
            return Ok(gst::FlowSuccess::Ok);
        };

        let width = info.width() as usize;
        let height = info.height() as usize;
        let num_samples = width * height * NUM_CHANNELS;

        // Read the tensor as raw f32 samples.
        let map = tensor_buf.map_readable().map_err(|_| {
            gst::error!(
                CAT,
                imp = self,
                "Failed to map the enhanced image tensor readable"
            );
            gst::FlowError::Error
        })?;
        let data = map.as_slice_of::<f32>().map_err(|err| {
            gst::error!(
                CAT,
                imp = self,
                "Enhanced image tensor data is not usable as f32: {err}"
            );
            gst::FlowError::Error
        })?;

        if data.len() < num_samples {
            gst::error!(
                CAT,
                imp = self,
                "Tensor holds {} samples, expected {num_samples}",
                data.len(),
            );
            return Err(gst::FlowError::Error);
        }

        gst::log!(CAT, imp = self, "Decoding {width}x{height} enhanced image");

        // Map the frame writable, and write the tensor over it row by row.
        let mut frame = gst_video::VideoFrameRef::from_buffer_ref_writable(buffer, &info)
            .map_err(|_| gst::FlowError::Error)?;
        let stride = frame.plane_stride()[0] as usize;
        let row_size = width * NUM_CHANNELS;
        let dst = frame.plane_data_mut(0).map_err(|_| gst::FlowError::Error)?;

        for (src_row, dst_row) in data[..num_samples]
            .chunks_exact(row_size)
            .zip(dst.chunks_mut(stride))
        {
            match info.format() {
                // The tensor already holds floats in HWC order, the RGB_F32LE layout: copy
                // each sample clamped to [0.0, 1.0] as little-endian bytes.
                gst_video::VideoFormat::RgbF32le => {
                    for (&sample, dst) in src_row
                        .iter()
                        .zip(dst_row[..row_size * 4].as_chunks_mut::<4>().0)
                    {
                        *dst = sample.clamp(0.0, 1.0).to_le_bytes();
                    }
                }
                // RGB, the only other format of the pad templates: denormalize each sample to
                // 8 bits; `as u8` saturates out-of-range and NaN inputs.
                _ => {
                    for (&sample, dst) in src_row.iter().zip(dst_row[..row_size].iter_mut()) {
                        *dst = (sample * 255. + 0.5) as u8;
                    }
                }
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

// Helpers for `transform_ip`.
impl ZeroDceTensorDec {
    /// Logs the tensor metas attached to `buffer`, for debugging.
    fn log_tensor_metas(&self, buffer: &gst::BufferRef) {
        for (index, meta) in buffer.iter_meta::<gst_analytics::TensorMeta>().enumerate() {
            let tensors = meta.as_slice();
            gst::log!(
                CAT,
                imp = self,
                "Tensor meta {index} of buffer with PTS {} carries {} tensor(s)",
                buffer.pts().display(),
                tensors.len(),
            );

            for tensor in tensors {
                gst::log!(
                    CAT,
                    imp = self,
                    "  tensor {}: type {:?}, dims-order {:?}, dims {:?}, {} bytes",
                    tensor.id().as_str(),
                    tensor.data_type(),
                    tensor.dims_order(),
                    tensor.dims(),
                    tensor.data().size(),
                );
            }
        }
    }

    /// Finds the buffer's enhanced image tensor and returns its data as an owned buffer.
    ///
    /// Returns `None` if there is no such tensor, so the frame passes through unmodified. A
    /// tensor present at the wrong resolution is an error instead, to surface caps mismatches.
    fn find_image_tensor(
        &self,
        buffer: &gst::BufferRef,
        info: &gst_video::VideoInfo,
    ) -> Result<Option<gst::Buffer>, gst::FlowError> {
        let width = info.width() as usize;
        let height = info.height() as usize;

        // Match any width/height here; the resolution is checked, and reported, below. Only a
        // batch of 1 is accepted.
        const ANY: usize = usize::MAX;
        let batched = [1, ANY, ANY, NUM_CHANNELS];
        let unbatched = [ANY, ANY, NUM_CHANNELS];

        let found = buffer
            .iter_meta::<gst_analytics::TensorMeta>()
            .find_map(|meta| {
                [batched.as_slice(), unbatched.as_slice()]
                    .into_iter()
                    .find_map(|dims| image_tensor(&meta, dims))
                    .map(|tensor| (tensor.dims().to_owned(), tensor.data().to_owned()))
            });

        let Some((dims, data)) = found else {
            gst::trace!(CAT, imp = self, "No enhanced image tensor meta found");
            return Ok(None);
        };

        let (tensor_height, tensor_width) = (dims[dims.len() - 3], dims[dims.len() - 2]);
        if (tensor_width, tensor_height) != (width, height) {
            gst::error!(
                CAT,
                imp = self,
                "Enhanced image tensor is {tensor_width}x{tensor_height}, expected {width}x{height}"
            );
            return Err(gst::FlowError::Error);
        }

        Ok(Some(data))
    }
}

/// Returns the enhanced image tensor of `meta`, if it holds one of exactly `dims`.
fn image_tensor<'a>(
    meta: &'a gst_analytics::TensorMeta,
    dims: &[usize],
) -> Option<&'a gst_analytics::Tensor> {
    meta.typed_tensor(
        glib::Quark::from_static_str(IMAGE_RGB_HWC_OUT),
        gst_analytics::TensorDataType::Float32,
        gst_analytics::TensorDimOrder::RowMajor,
        dims,
    )
}
