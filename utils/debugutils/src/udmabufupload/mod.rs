// Copyright (C) 2026 Netflix Inc.
//   Author: Xavier Claessens <xclaessens@netflix.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-udmabufupload
 *
 * Copies raw system memory video frames into udmabuf memory and outputs them
 * as DMA_DRM caps with the same pixel layout as the input (e.g. `NV12` becomes
 * `drm-format=NV12`, tiled formats get the matching DRM modifier).
 *
 * Output buffers are allocated from a #GstVideoDmabufPool using the udmabuf
 * allocator, unless downstream proposes a #GstVideoDmabufPool with its own
 * DMABuf allocator, in which case that one is used.
 *
 * If upstream already provides DMABuf backed buffers (for example by using
 * the udmabuf pool proposed by this element), they are forwarded without copy.
 *
 * If upstream negotiates `video/x-raw(memory:DMABuf)` caps, the element
 * operates in passthrough mode.
 *
 * ## Example launch line
 *
 * |[
 * gst-launch-1.0 videotestsrc ! video/x-raw,format=NV12 ! udmabufupload ! waylandsink
 * ]|
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct UdmabufUpload(ObjectSubclass<imp::UdmabufUpload>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "udmabufupload",
        gst::Rank::NONE,
        UdmabufUpload::static_type(),
    )
}
