// Copyright (C) 2026 Netflix Inc.
//   Author: Xavier Claessens <xclaessens@netflix.com>
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
use gst_video::prelude::*;

use std::sync::{LazyLock, Mutex};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "udmabufupload",
        gst::DebugColorFlags::empty(),
        Some("udmabuf upload element"),
    )
});

struct State {
    in_info: gst_video::VideoInfo,
    out_info: gst_video::VideoInfoDmaDrm,
}

#[derive(Default)]
pub struct UdmabufUpload {
    state: Mutex<Option<State>>,
}

/// Maps a GStreamer format name to the equivalent DRM format string.
fn gst_format_to_drm_format(s: &str) -> Option<String> {
    let format = s.parse::<gst_video::VideoFormat>().ok()?;
    let (fourcc, modifier) = gst_video::dma_drm_format_from_gst_format(format).ok()?;
    Some(gst_video::dma_drm_fourcc_to_string(fourcc, modifier).to_string())
}

/// Maps a DRM format string to the equivalent GStreamer format name.
fn drm_format_to_gst_format(s: &str) -> Option<String> {
    let (fourcc, modifier) = gst_video::dma_drm_fourcc_from_str(s).ok()?;
    let format = gst_video::dma_drm_format_to_gst_format(fourcc, modifier).ok()?;
    Some(format.to_str().to_string())
}

/// Maps a string or list of strings, returns `None` if nothing could be mapped.
fn map_format_value(
    value: &glib::SendValue,
    func: impl Fn(&str) -> Option<String>,
) -> Option<glib::SendValue> {
    let mut formats = Vec::new();

    if let Ok(s) = value.get::<&str>() {
        formats.extend(func(s));
    } else if let Ok(list) = value.get::<gst::List>() {
        for v in list.iter() {
            if let Ok(s) = v.get::<&str>() {
                formats.extend(func(s));
            }
        }
    }

    match formats.len() {
        0 => None,
        1 => Some(formats.remove(0).to_send_value()),
        _ => Some(gst::List::new(formats).to_send_value()),
    }
}

/// Converts a system memory structure into DMA_DRM, or returns `None` if none
/// of its formats have a DRM equivalent.
fn sysmem_to_dma_drm_structure(s: &gst::StructureRef) -> Option<gst::Structure> {
    let mut s = s.to_owned();

    if let Ok(value) = s.value("format") {
        let drm_formats = map_format_value(value, gst_format_to_drm_format)?;
        s.set_value("drm-format", drm_formats);
    }
    s.set("format", "DMA_DRM");

    Some(s)
}

/// Converts a DMA_DRM structure into system memory, or returns `None` if none
/// of its DRM formats have a GStreamer equivalent.
fn dma_drm_to_sysmem_structure(s: &gst::StructureRef) -> Option<gst::Structure> {
    if let Ok(format) = s.get::<&str>("format")
        && format != "DMA_DRM"
    {
        return None;
    }

    let mut s = s.to_owned();
    if let Ok(value) = s.value("drm-format") {
        let formats = map_format_value(value, drm_format_to_gst_format)?;
        s.set_value("format", formats);
        s.remove_field("drm-format");
    } else {
        s.remove_field("format");
    }

    Some(s)
}

fn buffer_is_dmabuf(buffer: &gst::BufferRef) -> bool {
    buffer.n_memory() > 0
        && buffer
            .iter_memories()
            .all(|mem| mem.is_memory_type::<gst_allocators::DmaBufMemory>())
}

/// Configures @pool and returns the buffer size chosen by the pool.
fn configure_pool(
    pool: &gst::BufferPool,
    caps: &gst::Caps,
    size: u32,
    min: u32,
    max: u32,
    allocator: &gst::Allocator,
) -> Result<u32, glib::BoolError> {
    let mut config = pool.config();
    config.set_params(Some(caps), size, min, max);
    config.set_allocator(Some(allocator), Some(&gst::AllocationParams::default()));
    config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META.as_str());
    config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_ALIGNMENT.as_str());
    config.set_video_alignment(&gst_video::VideoAlignment::default());

    if pool.set_config(config).is_err() {
        // GstVideoDmabufPool adjusts allocator params and alignment and
        // returns FALSE, try again with the updated config.
        let config = pool.config();
        config.validate_params(Some(caps), size, min, max)?;
        pool.set_config(config)?;
    }

    let (_, size, _, _) = pool
        .config()
        .params()
        .ok_or_else(|| glib::bool_error!("Pool has no params"))?;

    Ok(size)
}

/// Returns the DMABuf allocator configured on @pool, if any.
fn pool_dmabuf_allocator(pool: &gst::BufferPool) -> Option<gst::Allocator> {
    let (allocator, _) = pool.config().allocator()?;
    let allocator = allocator?;

    let custom_alloc =
        allocator.object_flags().bits() & gst::ffi::GST_ALLOCATOR_FLAG_CUSTOM_ALLOC != 0;
    (allocator.is::<gst_allocators::DmaBufAllocator>() && !custom_alloc).then_some(allocator)
}

#[glib::object_subclass]
impl ObjectSubclass for UdmabufUpload {
    const NAME: &'static str = "GstUdmabufUpload";
    type Type = super::UdmabufUpload;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for UdmabufUpload {}

impl GstObjectImpl for UdmabufUpload {}

impl ElementImpl for UdmabufUpload {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "udmabuf upload",
                "Filter/Video",
                "Copies raw video frames into udmabuf memory",
                "Xavier Claessens <xclaessens@netflix.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let dma_drm_caps = gst_video::VideoCapsBuilder::new()
                .features([gst_allocators::CAPS_FEATURE_MEMORY_DMABUF])
                .format(gst_video::VideoFormat::DmaDrm)
                .build();

            let mut sink_caps = dma_drm_caps.clone();
            sink_caps.merge(gst_video::VideoCapsBuilder::new().build());

            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &sink_caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &dma_drm_caps,
                )
                .unwrap(),
            ]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for UdmabufUpload {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    // DMABuf input caps are identical to the output caps, basetransform
    // operates in passthrough in that case.
    const PASSTHROUGH_ON_SAME_CAPS: bool = true;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let mut ret = gst::Caps::new_empty();
        for (s, features) in caps.iter_with_features() {
            let is_dmabuf = features.contains(gst_allocators::CAPS_FEATURE_MEMORY_DMABUF);

            // DMABuf caps are passed through unchanged, list them first
            // to prefer passthrough.
            if is_dmabuf {
                ret.merge_structure_full(s.to_owned(), Some(features.to_owned()));
            }

            if direction == gst::PadDirection::Sink && !is_dmabuf {
                if let Some(s) = sysmem_to_dma_drm_structure(s) {
                    ret.merge_structure_full(
                        s,
                        Some(gst::CapsFeatures::new([
                            gst_allocators::CAPS_FEATURE_MEMORY_DMABUF,
                        ])),
                    );
                }
            } else if direction == gst::PadDirection::Src
                && is_dmabuf
                && let Some(s) = dma_drm_to_sysmem_structure(s)
            {
                ret.merge_structure_full(
                    s,
                    Some(gst::CapsFeatures::new([
                        gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY,
                    ])),
                );
            }
        }

        if let Some(filter) = filter {
            ret = filter.intersect_with_mode(&ret, gst::CapsIntersectMode::First);
        }

        gst::debug!(CAT, imp = self, "Transformed {caps:?} into {ret:?}");

        Some(ret)
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let mut state = self.state.lock().unwrap();

        if gst_video::is_dma_drm_caps(incaps) {
            if incaps != outcaps {
                return Err(gst::loggable_error!(
                    CAT,
                    "DMABuf input caps {incaps:?} differ from output {outcaps:?}"
                ));
            }
            *state = None;
            return Ok(());
        }

        let in_info = gst_video::VideoInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid input caps {incaps:?}"))?;
        let out_info = gst_video::VideoInfoDmaDrm::from_caps(outcaps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid output caps {outcaps:?}"))?;

        let (fourcc, modifier) = gst_video::dma_drm_format_from_gst_format(in_info.format())
            .map_err(|_| gst::loggable_error!(CAT, "Unsupported format {:?}", in_info.format()))?;
        if fourcc != out_info.fourcc() || modifier != out_info.modifier() {
            return Err(gst::loggable_error!(
                CAT,
                "Input format {:?} does not match output {outcaps:?}",
                in_info.format()
            ));
        }

        *state = Some(State { in_info, out_info });

        Ok(())
    }

    fn propose_allocation(
        &self,
        decide_query: Option<&gst::query::Allocation>,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        // In passthrough, let downstream answer
        if self.obj().is_passthrough() {
            return self.parent_propose_allocation(decide_query, query);
        }

        let (caps, need_pool) = query.get_owned();
        let caps = caps.ok_or_else(|| gst::loggable_error!(CAT, "No caps in query"))?;
        let info = gst_video::VideoInfo::from_caps(&caps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid caps {caps:?}"))?;

        query.add_allocation_meta::<gst_video::VideoMeta>(None);

        if !need_pool {
            return Ok(());
        }

        let Some(allocator) = gst_allocators::UdmabufAllocator::get() else {
            return Ok(());
        };

        // Offer a pool so that upstream can write directly into udmabuf
        // memory, in which case we can forward buffers without copy.
        let Some(pool) = gst_video::VideoDmabufPool::new() else {
            return Ok(());
        };
        match configure_pool(
            pool.upcast_ref(),
            &caps,
            info.size() as u32,
            0,
            0,
            &allocator,
        ) {
            Ok(size) => query.add_allocation_pool(Some(&pool), size, 0, 0),
            Err(err) => gst::warning!(CAT, imp = self, "Failed to configure proposed pool: {err}"),
        }

        Ok(())
    }

    fn decide_allocation(
        &self,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        let out_size = match &*self.state.lock().unwrap() {
            Some(state) => state.out_info.size() as u32,
            None => return self.parent_decide_allocation(query),
        };

        let (caps, _) = query.get_owned();
        let caps = caps.ok_or_else(|| gst::loggable_error!(CAT, "No caps in query"))?;

        if query
            .find_allocation_meta::<gst_video::VideoMeta>()
            .is_none()
        {
            gst::info!(
                CAT,
                imp = self,
                "Downstream does not support GstVideoMeta, stride and offsets will not be communicated"
            );
        }

        let (mut pool, mut size, min, max, update_pool) = match query.allocation_pools().next() {
            Some((pool, size, min, max)) => (pool, size, min, max, true),
            None => (None, 0, 0, 0, false),
        };
        pool = pool.filter(|pool| pool.is::<gst_video::VideoDmabufPool>());

        // Keep the DMABuf allocator of a downstream GstVideoDmabufPool,
        // otherwise use udmabuf.
        let allocator = pool.as_ref().and_then(pool_dmabuf_allocator);
        let pool = match pool {
            Some(pool) => pool,
            None => gst_video::VideoDmabufPool::new()
                .ok_or_else(|| gst::loggable_error!(CAT, "DMABuf pool not supported"))?
                .upcast(),
        };
        let Some(allocator) = allocator.or_else(gst_allocators::UdmabufAllocator::get) else {
            gst::element_imp_error!(
                self,
                gst::ResourceError::NotFound,
                ["udmabuf allocator is not available"]
            );
            return Err(gst::loggable_error!(
                CAT,
                "udmabuf allocator is not available"
            ));
        };

        gst::debug!(
            CAT,
            imp = self,
            "Using pool {pool:?} with allocator {allocator:?}"
        );

        size = size.max(out_size);
        let size = configure_pool(&pool, &caps, size, min, max, &allocator).map_err(|err| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Settings,
                ["Failed to configure DMABuf pool: {err}"]
            );
            gst::loggable_error!(CAT, "Failed to configure DMABuf pool: {err}")
        })?;

        if update_pool {
            query.set_nth_allocation_pool(0, Some(&pool), size, min, max);
        } else {
            query.add_allocation_pool(Some(&pool), size, min, max);
        }

        Ok(())
    }

    fn generate_output(
        &self,
    ) -> Result<gst_base::subclass::base_transform::GenerateOutputSuccess, gst::FlowError> {
        // If upstream already provides DMABuf memory, forward the input buffer
        // as-is instead of copying it into a new buffer.
        if !self.obj().is_passthrough()
            && self
                .queued_buffer()
                .is_some_and(|buffer| buffer_is_dmabuf(&buffer))
        {
            let buffer = self.take_queued_buffer().unwrap();
            gst::trace!(CAT, imp = self, "Input is already DMABuf, forwarding");
            return Ok(gst_base::subclass::base_transform::GenerateOutputSuccess::Buffer(buffer));
        }

        self.parent_generate_output()
    }

    fn transform(
        &self,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Clone the state out so the lock is not held while copying the frame
        let (in_info, out_info) = match &*self.state.lock().unwrap() {
            Some(state) => (state.in_info.clone(), state.out_info.clone()),
            None => return Err(gst::FlowError::NotNegotiated),
        };

        let in_frame = gst_video::VideoFrameRef::from_buffer_ref_readable(inbuf, &in_info)
            .map_err(|_| {
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Failed,
                    ["Failed to map input buffer"]
                );
                gst::FlowError::Error
            })?;

        let mut out_frame = gst_video::VideoFrameRef::from_buffer_ref_writable(outbuf, &out_info)
            .map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["Failed to map output buffer"]
            );
            gst::FlowError::Error
        })?;

        in_frame.copy(&mut out_frame).map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["Failed to copy video frame"]
            );
            gst::FlowError::Error
        })?;

        Ok(gst::FlowSuccess::Ok)
    }
}
